#![allow(clippy::let_unit_value)]

pub mod debug;
mod interconnect;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::iter::FromIterator;
use std::sync::Arc;

use fedimint_api::core::ModuleKey;
use fedimint_api::db::{Database, DatabaseTransaction};
use fedimint_api::encoding::{Decodable, Encodable, ModuleRegistry};
use fedimint_api::module::audit::Audit;
use fedimint_api::module::{ModuleError, TransactionItemAmount};
use fedimint_api::server::{ServerModule, VerificationCache};
use fedimint_api::{Amount, OutPoint, PeerId, TransactionId};
use fedimint_core::epoch::*;
use fedimint_core::outcome::TransactionStatus;
use futures::future::select_all;
use hbbft::honey_badger::Batch;
use itertools::Itertools;
use rand::rngs::OsRng;
use thiserror::Error;
use tokio::sync::Notify;
use tracing::{debug, error, info_span, instrument, trace, warn, Instrument};

use crate::config::ServerConfig;
use crate::consensus::interconnect::FedimintInterconnect;
use crate::db::{
    AcceptedTransactionKey, DropPeerKey, DropPeerKeyPrefix, EpochHistoryKey, LastEpochKey,
    ProposedTransactionKey, ProposedTransactionKeyPrefix, RejectedTransactionKey,
};
use crate::rng::RngGenerator;
use crate::transaction::{Transaction, TransactionError};
use crate::OsRngGen;

pub type SerdeConsensusOutcome = Batch<Vec<SerdeConsensusItem>, PeerId>;
pub type ConsensusOutcome = Batch<Vec<ConsensusItem>, PeerId>;
pub type HoneyBadgerMessage = hbbft::honey_badger::Message<PeerId>;

// TODO remove HBBFT `Batch` from `ConsensusOutcome`
#[derive(Debug, Clone)]
pub struct ConsensusOutcomeConversion(pub ConsensusOutcome);

impl PartialEq<Self> for ConsensusOutcomeConversion {
    fn eq(&self, other: &Self) -> bool {
        self.0.epoch.eq(&other.0.epoch) && self.0.contributions.eq(&other.0.contributions)
    }
}

impl From<OutcomeHistory> for ConsensusOutcomeConversion {
    fn from(history: OutcomeHistory) -> Self {
        ConsensusOutcomeConversion(Batch {
            epoch: history.epoch,
            contributions: BTreeMap::from_iter(history.items.into_iter()),
        })
    }
}

/// Proposed HBBFT consensus changes including removing peers
#[derive(Debug, Clone)]
pub struct ConsensusProposal {
    pub items: Vec<ConsensusItem>,
    pub drop_peers: Vec<PeerId>,
}

// TODO: we should make other fields private and get rid of this
#[non_exhaustive]
pub struct FedimintConsensus {
    /// Cryptographic random number generator used for everything
    pub rng_gen: Box<dyn RngGenerator<Rng = OsRng>>,
    /// Configuration describing the federation and containing our secrets
    pub cfg: ServerConfig,

    pub modules: BTreeMap<ModuleKey, ServerModule>,
    /// KV Database into which all state is persisted to recover from in case of a crash
    pub db: Database,

    /// Notifies tasks when there is a new transaction
    pub transaction_notify: Arc<Notify>,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Encodable, Decodable)]
pub struct AcceptedTransaction {
    pub epoch: u64,
    pub transaction: Transaction,
}

#[derive(Debug)]
struct VerificationCaches {
    caches: HashMap<ModuleKey, VerificationCache>,
}

struct FundingVerifier {
    input_amount: Amount,
    output_amount: Amount,
    fee_amount: Amount,
}

impl FedimintConsensus {
    pub fn new(cfg: ServerConfig, db: Database) -> Self {
        Self {
            rng_gen: Box::new(OsRngGen),
            cfg,
            modules: BTreeMap::default(),
            db,
            transaction_notify: Arc::new(Notify::new()),
        }
    }

    pub fn register_module(&mut self, module: ServerModule) -> &mut Self {
        if self.modules.insert(module.module_key(), module).is_some() {
            panic!("Must not register modules with key conflict");
        }
        self
    }
}

impl VerificationCaches {
    fn get_cache(&self, modue_key: ModuleKey) -> &VerificationCache {
        self.caches
            .get(&modue_key)
            .expect("Verification caches were built for all modules")
    }
}

impl FedimintConsensus {
    pub fn decoders(&self) -> ModuleRegistry {
        self.modules
            .iter()
            .map(|(module_id, module)| (*module_id, module.decoder()))
            .collect()
    }

    pub fn database_transaction(&self) -> DatabaseTransaction<'_> {
        self.db.begin_transaction(self.decoders())
    }

    pub async fn submit_transaction(
        &self,
        transaction: Transaction,
    ) -> Result<(), TransactionSubmissionError> {
        // we already processed the transaction before the request was received
        if self.transaction_status(transaction.tx_hash()).is_some() {
            return Ok(());
        }

        let tx_hash = transaction.tx_hash();
        debug!(%tx_hash, "Received mint transaction");

        let mut funding_verifier = FundingVerifier::default();

        let mut pub_keys = Vec::new();

        // Create read-only DB tx so that the read state is consistent
        let mut dbtx = self.db.begin_transaction(self.decoders());

        for input in &transaction.inputs {
            let module = self
                .modules
                .get(&input.module_key())
                .expect("Parsing the input should fail if the module doesn't exist");

            let cache = module.build_verification_cache(&[input.clone()]);
            let interconnect = self.build_interconnect();
            let meta = module
                .validate_input(&interconnect, &mut dbtx, &cache, input)
                .await
                .map_err(|e| TransactionSubmissionError::ModuleError(tx_hash, e))?;

            pub_keys.push(meta.puk_keys);
            funding_verifier.add_input(meta.amount);
        }
        transaction.validate_signature(pub_keys.into_iter().flatten())?;

        for output in &transaction.outputs {
            let module = self
                .modules
                .get(&output.module_key())
                .expect("Parsing the input should fail if the module doesn't exist");
            let amount = module
                .validate_output(&dbtx, output)
                .map_err(|e| TransactionSubmissionError::ModuleError(tx_hash, e))?;
            funding_verifier.add_output(amount);
        }

        funding_verifier.verify_funding()?;

        let new = dbtx
            .insert_entry(&ProposedTransactionKey(tx_hash), &transaction)
            .expect("DB error");
        dbtx.commit_tx().await.expect("DB Error");

        if new.is_some() {
            warn!("Added consensus item was already in consensus queue");
        }

        self.transaction_notify.notify_one();
        Ok(())
    }

    #[instrument(skip_all, fields(epoch = consensus_outcome.epoch))]
    pub async fn process_consensus_outcome(&self, consensus_outcome: ConsensusOutcome) {
        let epoch = consensus_outcome.epoch;
        let epoch_peers: HashSet<PeerId> =
            consensus_outcome.contributions.keys().copied().collect();
        let outcome = consensus_outcome.clone();

        let UnzipConsensusItem {
            epoch_info: _epoch_info_cis,
            transaction: transaction_cis,
            module: module_cis,
        } = consensus_outcome
            .contributions
            .into_iter()
            .flat_map(|(peer, cis)| cis.into_iter().map(move |ci| (peer, ci)))
            .unzip_consensus_item();

        // Begin consensus epoch
        {
            let per_module_cis: HashMap<
                ModuleKey,
                Vec<(PeerId, fedimint_api::core::ConsensusItem)>,
            > = module_cis
                .into_iter()
                .into_group_map_by(|(_peer, mci)| mci.module_key());

            let mut dbtx = self.db.begin_transaction(self.decoders());
            for (module_key, module_cis) in per_module_cis {
                let module = self
                    .modules
                    .get(&module_key)
                    .expect("CIs were decoded, so the module exists");
                module.begin_consensus_epoch(&mut dbtx, module_cis).await;
            }

            dbtx.commit_tx().await.expect("DB Error");
        }

        // Process transactions
        {
            let mut dbtx = self.db.begin_transaction(self.decoders());

            let caches = self.build_verification_caches(transaction_cis.iter().map(|(_, tx)| tx));
            for (_, transaction) in transaction_cis {
                let span = info_span!("Processing transaction");
                async {
                    trace!(?transaction);
                    dbtx.remove_entry(&ProposedTransactionKey(transaction.tx_hash()))
                        .await
                        .expect("DB Error");

                    dbtx.set_tx_savepoint();

                    // TODO: @dpc eventually we need to distinguish between skipped tx from this and
                    // previous epochs to only include the ones from previous epoch in the "invalid tx" list
                    if dbtx
                        .get_value(&AcceptedTransactionKey(transaction.tx_hash()))
                        .expect("DB error")
                        .is_some()
                    {
                        debug!("Transaction was already successfully processed, skipping");
                        return;
                    }

                    match self
                        .process_transaction(&mut dbtx, transaction.clone(), &caches)
                        .await
                    {
                        Ok(()) => {
                            dbtx.insert_entry(
                                &AcceptedTransactionKey(transaction.tx_hash()),
                                &AcceptedTransaction { epoch, transaction },
                            )
                            .expect("DB Error");
                        }
                        Err(error) => {
                            dbtx.rollback_tx_to_savepoint().await;
                            warn!(%error, "Transaction failed");
                            dbtx.insert_entry(
                                &RejectedTransactionKey(transaction.tx_hash()),
                                &format!("{:?}", error),
                            )
                            .expect("DB Error");
                        }
                    }
                }
                .instrument(span)
                .await;
            }
            dbtx.commit_tx().await.expect("DB Error");
        }

        // End consensus epoch
        {
            let mut dbtx = self.db.begin_transaction(self.decoders());
            let mut drop_peers = Vec::<PeerId>::new();

            self.save_epoch_history(outcome, &mut dbtx, &mut drop_peers);

            for module in self.modules.values() {
                let module_drop_peers = module.end_consensus_epoch(&epoch_peers, &mut dbtx).await;
                drop_peers.extend(module_drop_peers);
            }

            for peer in drop_peers {
                dbtx.insert_entry(&DropPeerKey(peer), &())
                    .expect("DB Error");
            }

            dbtx.commit_tx().await.expect("DB Error");
        }

        let audit = self.audit();
        if audit.sum().milli_sat < 0 {
            panic!(
                "Balance sheet of the fed has gone negative, this should never happen! {}",
                audit
            )
        }
    }

    pub fn get_last_epoch(&self) -> Option<u64> {
        self.db
            .begin_transaction(self.decoders())
            .get_value(&LastEpochKey)
            .expect("db query must not fail")
            .map(|e| e.0)
    }

    pub fn epoch_history(&self, epoch: u64) -> Option<EpochHistory> {
        self.db
            .begin_transaction(self.decoders())
            .get_value(&EpochHistoryKey(epoch))
            .unwrap()
    }

    fn save_epoch_history<'a>(
        &self,
        outcome: ConsensusOutcome,
        dbtx: &mut DatabaseTransaction<'a>,
        drop_peers: &mut Vec<PeerId>,
    ) {
        let prev_epoch_key = EpochHistoryKey(outcome.epoch.saturating_sub(1));
        let peers: Vec<PeerId> = outcome.contributions.keys().cloned().collect();
        let maybe_prev_epoch = self
            .db
            .begin_transaction(self.decoders())
            .get_value(&prev_epoch_key)
            .expect("DB error");

        let current = EpochHistory::new(outcome.epoch, outcome.contributions, &maybe_prev_epoch);

        // validate and update sigs on prev epoch
        if let Some(prev_epoch) = maybe_prev_epoch {
            let pks = &self.cfg.epoch_pk_set;

            match current.add_sig_to_prev(pks, prev_epoch) {
                Ok(prev_epoch) => {
                    dbtx.insert_entry(&prev_epoch_key, &prev_epoch)
                        .expect("DB Error");
                }
                Err(EpochVerifyError::NotEnoughValidSigShares(contributing_peers)) => {
                    warn!("Unable to sign epoch {}", prev_epoch_key.0);
                    for peer in peers {
                        if !contributing_peers.contains(&peer) {
                            warn!("Dropping {} for not contributing valid epoch sigs.", peer);
                            drop_peers.push(peer);
                        }
                    }
                }
                Err(_) => panic!("Not possible"),
            }
        }

        dbtx.insert_entry(&LastEpochKey, &EpochHistoryKey(current.outcome.epoch))
            .expect("DB Error");
        dbtx.insert_entry(&EpochHistoryKey(current.outcome.epoch), &current)
            .expect("DB Error");
    }

    pub async fn await_consensus_proposal(&self) {
        let dbtx = self.database_transaction();
        let proposal_futures = self
            .modules
            .iter()
            .map(|(_, module)| module.await_consensus_proposal(&dbtx))
            .collect::<Vec<_>>();

        select_all(proposal_futures).await;
    }

    pub async fn get_consensus_proposal(&self) -> ConsensusProposal {
        let mut dbtx = self.database_transaction();

        let drop_peers = dbtx
            .find_by_prefix(&DropPeerKeyPrefix)
            .map(|res| {
                let key = res.expect("DB error").0;
                key.0
            })
            .collect();

        let mut items: Vec<ConsensusItem> = dbtx
            .find_by_prefix(&ProposedTransactionKeyPrefix)
            .map(|res| {
                let (_key, value) = res.expect("DB error");
                ConsensusItem::Transaction(value)
            })
            .collect();

        for module in self.modules.values() {
            items.extend(
                module
                    .consensus_proposal(&mut dbtx)
                    .await
                    .into_iter()
                    .map(ConsensusItem::Module),
            );
        }

        if let Some(epoch) = dbtx.get_value(&LastEpochKey).unwrap() {
            let last_epoch = dbtx.get_value(&epoch).unwrap().unwrap();
            let sig = self.cfg.epoch_sks.0.sign(last_epoch.hash);
            let item = ConsensusItem::EpochInfo(EpochSignatureShare(sig));
            items.push(item);
        };

        ConsensusProposal { items, drop_peers }
    }

    async fn process_transaction<'a>(
        &self,
        dbtx: &mut DatabaseTransaction<'a>,
        transaction: Transaction,
        caches: &VerificationCaches,
    ) -> Result<(), TransactionSubmissionError> {
        let mut funding_verifier = FundingVerifier::default();

        let tx_hash = transaction.tx_hash();

        let mut pub_keys = Vec::new();
        for input in transaction.inputs.iter() {
            let module = self
                .modules
                .get(&input.module_key())
                .expect("Parsing the input should fail if the module doesn't exist");
            let meta = module
                .apply_input(
                    &self.build_interconnect(),
                    dbtx,
                    input,
                    caches.get_cache(input.module_key()),
                )
                .await
                .map_err(|e| TransactionSubmissionError::ModuleError(tx_hash, e))?;
            pub_keys.push(meta.puk_keys);
            funding_verifier.add_input(meta.amount);
        }
        transaction.validate_signature(pub_keys.into_iter().flatten())?;

        for (idx, output) in transaction.outputs.into_iter().enumerate() {
            let out_point = OutPoint {
                txid: tx_hash,
                out_idx: idx as u64,
            };
            let module = self
                .modules
                .get(&output.module_key())
                .expect("Parsing the input should fail if the module doesn't exist");

            let amount = module
                .apply_output(dbtx, &output, out_point)
                .await
                .map_err(|e| TransactionSubmissionError::ModuleError(tx_hash, e))?;
            funding_verifier.add_output(amount);
        }

        funding_verifier.verify_funding()?;

        Ok(())
    }

    pub fn transaction_status(
        &self,
        txid: TransactionId,
    ) -> Option<crate::outcome::TransactionStatus> {
        let mut dbtx = self.database_transaction();

        let accepted: Option<AcceptedTransaction> = dbtx
            .get_value(&AcceptedTransactionKey(txid))
            .expect("DB error");

        if let Some(accepted_tx) = accepted {
            let outputs = accepted_tx
                .transaction
                .outputs
                .iter()
                .enumerate()
                .map(|(out_idx, output)| {
                    let outpoint = OutPoint {
                        txid,
                        out_idx: out_idx as u64,
                    };
                    let outcome = self
                        .modules
                        .get(&output.module_key())
                        .expect("Module exists because parsing succeeded")
                        .output_status(&mut dbtx, outpoint)
                        .expect("the transaction was processed, so should be known");
                    (&outcome).into()
                })
                .collect();

            return Some(crate::outcome::TransactionStatus::Accepted {
                epoch: accepted_tx.epoch,
                outputs,
            });
        }

        let rejected: Option<String> = self
            .db
            .begin_transaction(self.decoders())
            .get_value(&RejectedTransactionKey(txid))
            .expect("DB error");

        if let Some(message) = rejected {
            return Some(TransactionStatus::Rejected(message));
        }

        None
    }

    fn build_verification_caches<'a>(
        &self,
        transactions: impl Iterator<Item = &'a Transaction> + Send,
    ) -> VerificationCaches {
        let module_inputs = transactions
            .flat_map(|tx| tx.inputs.iter())
            .cloned()
            .into_group_map_by(|input| input.module_key());

        // TODO: should probably run in parallel, but currently only the mint does anything at all
        let caches = module_inputs
            .into_iter()
            .map(|(module_key, inputs)| {
                let module = self
                    .modules
                    .get(&module_key)
                    .expect("Inputs were parsed so module exists");
                (module_key, module.build_verification_cache(&inputs))
            })
            .collect();

        VerificationCaches { caches }
    }

    pub fn audit(&self) -> Audit {
        let dbtx = self.database_transaction();
        let mut audit = Audit::default();
        for module in self.modules.values() {
            module.audit(&dbtx, &mut audit)
        }
        audit
    }

    fn build_interconnect(&self) -> FedimintInterconnect {
        FedimintInterconnect { fedimint: self }
    }
}

impl FundingVerifier {
    fn add_input(&mut self, input_amount: TransactionItemAmount) {
        self.input_amount += input_amount.amount;
        self.fee_amount += input_amount.fee;
    }

    fn add_output(&mut self, output_amount: TransactionItemAmount) {
        self.output_amount += output_amount.amount;
        self.fee_amount += output_amount.fee;
    }

    fn verify_funding(self) -> Result<(), TransactionError> {
        if self.input_amount == (self.output_amount + self.fee_amount) {
            Ok(())
        } else {
            Err(TransactionError::UnbalancedTransaction {
                inputs: self.input_amount,
                outputs: self.output_amount,
                fee: self.fee_amount,
            })
        }
    }
}

impl Default for FundingVerifier {
    fn default() -> Self {
        FundingVerifier {
            input_amount: Amount::ZERO,
            output_amount: Amount::ZERO,
            fee_amount: Amount::ZERO,
        }
    }
}

#[derive(Debug, Error)]
pub enum TransactionSubmissionError {
    #[error("High level transaction error: {0}")]
    TransactionError(#[from] TransactionError),
    #[error("Module input or output error in tx {0}: {1}")]
    ModuleError(TransactionId, ModuleError),
    #[error("Transaction conflict error")]
    TransactionConflictError,
}
