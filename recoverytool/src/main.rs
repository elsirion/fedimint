pub mod envs;

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fmt::{Display, Formatter};
use std::hash::Hasher;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use bitcoin::hashes::hex::FromHex;
use bitcoin::network::constants::Network;
use bitcoin::OutPoint;
use clap::{ArgGroup, Parser, Subcommand};
use fedimint_core::bitcoin_migration::{
    bitcoin29_to_bitcoin30_network, bitcoin29_to_bitcoin30_outpoint,
    bitcoin29_to_bitcoin30_secp256k1_secret_key, bitcoin30_to_bitcoin29_network,
    bitcoin30_to_bitcoin29_secp256k1_secret_key, bitcoin_29_to_bitcoin30_amount,
};
use fedimint_core::core::{
    LEGACY_HARDCODED_INSTANCE_ID_LN, LEGACY_HARDCODED_INSTANCE_ID_MINT,
    LEGACY_HARDCODED_INSTANCE_ID_WALLET,
};
use fedimint_core::db::{Database, IDatabaseTransactionOpsCoreTyped};
use fedimint_core::epoch::ConsensusItem;
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::module::CommonModuleInit;
use fedimint_core::session_outcome::SignedSessionOutcome;
use fedimint_core::transaction::Transaction;
use fedimint_core::ServerModule;
use fedimint_ln_common::LightningCommonInit;
use fedimint_ln_server::Lightning;
use fedimint_logging::TracingSetup;
use fedimint_mint_server::common::MintCommonInit;
use fedimint_mint_server::Mint;
use fedimint_rocksdb::{RocksDb, RocksDbReadOnly};
use fedimint_server::config::io::read_server_config;
use fedimint_server::db::SignedSessionOutcomePrefix;
use fedimint_wallet_server::common::config::WalletConfig;
use fedimint_wallet_server::common::keys::CompressedPublicKey;
use fedimint_wallet_server::common::tweakable::Tweakable;
use fedimint_wallet_server::common::{
    PegInDescriptor, SpendableUTXO, WalletCommonInit, WalletInput,
};
use fedimint_wallet_server::db::{UTXOKey, UTXOPrefixKey};
use fedimint_wallet_server::{nonce_from_idx, Wallet};
use futures::stream::StreamExt;
use miniscript::{Descriptor, MiniscriptKey, ToPublicKey, TranslatePk, Translator};
use secp256k1::SecretKey;
use serde::Serialize;
use tracing::info;

use crate::envs::FM_PASSWORD_ENV;

/// Tool to recover the on-chain wallet of a Fedimint federation
#[derive(Debug, Parser)]
#[command(version)]
#[command(group(
    ArgGroup::new("keysource")
        .required(true)
        .args(["config", "descriptor"]),
))]
struct RecoveryTool {
    /// Directory containing server config files
    #[arg(long = "cfg")]
    config: Option<PathBuf>,
    /// The password that encrypts the configs
    #[arg(long, env = FM_PASSWORD_ENV, requires = "config")]
    password: String,
    /// Wallet descriptor, can be used instead of --cfg
    #[arg(long)]
    descriptor: Option<PegInDescriptor>,
    /// Wallet secret key, can be used instead of config together with
    /// --descriptor
    #[arg(long, requires = "descriptor")]
    key: Option<SecretKey>,
    /// Network to operate on, has to be specified if --cfg isn't present
    #[arg(long, default_value = "bitcoin", requires = "descriptor")]
    network: Network,
    /// Open the database in read-only mode, useful for debugging, should not be
    /// used in production
    #[arg(long)]
    readonly: bool,
    #[command(subcommand)]
    strategy: TweakSource,
}

#[derive(Debug, Clone, Subcommand)]
enum TweakSource {
    /// Derive the wallet descriptor using a single tweak
    Direct {
        #[arg(long, value_parser = tweak_parser)]
        tweak: [u8; 33],
    },
    /// Derive all wallet descriptors of confirmed UTXOs in the on-chain wallet.
    /// Note that unconfirmed change UTXOs will not appear here.
    Utxos {
        /// Extract UTXOs from a database without module partitioning
        #[arg(long)]
        legacy: bool,
        /// Path to database
        #[arg(long)]
        db: PathBuf,
    },
    /// Derive all wallet descriptors of tweaks that were ever used according to
    /// the epoch log. In a long-running and busy federation this list will
    /// contain many empty descriptors.
    Epochs {
        /// Path to database
        #[arg(long)]
        db: PathBuf,
    },
}

fn tweak_parser(hex: &str) -> anyhow::Result<[u8; 33]> {
    <Vec<u8> as FromHex>::from_hex(hex)?
        .try_into()
        .map_err(|_| anyhow!("tweaks have to be 33 bytes long"))
}

fn get_db(readonly: bool, path: &Path, module_decoders: ModuleDecoderRegistry) -> Database {
    if readonly {
        Database::new(
            RocksDbReadOnly::open_read_only(path).expect("Error opening readonly DB"),
            module_decoders,
        )
    } else {
        Database::new(
            RocksDb::open(path).expect("Error opening DB"),
            module_decoders,
        )
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    TracingSetup::default().init()?;

    let opts: RecoveryTool = RecoveryTool::parse();

    let (base_descriptor, base_key, network) = if let Some(config) = opts.config {
        let cfg = read_server_config(&opts.password, config).expect("Could not read config file");
        let wallet_cfg: WalletConfig = cfg
            .get_module_config_typed(LEGACY_HARDCODED_INSTANCE_ID_WALLET)
            .expect("Malformed wallet config");
        let base_descriptor = wallet_cfg.consensus.peg_in_descriptor;
        let base_key = wallet_cfg.private.peg_in_key;
        let network = wallet_cfg.consensus.network;

        (base_descriptor, base_key, network)
    } else if let (Some(descriptor), Some(key)) = (opts.descriptor, opts.key) {
        (
            descriptor,
            key,
            bitcoin30_to_bitcoin29_network(opts.network),
        )
    } else {
        panic!("Either config or descriptor need to be provided by clap");
    };

    match opts.strategy {
        TweakSource::Direct { tweak } => {
            let descriptor = tweak_descriptor(
                &base_descriptor,
                &base_key,
                &tweak,
                bitcoin29_to_bitcoin30_network(network),
            );
            let wallets = vec![ImportableWalletMin { descriptor }];

            serde_json::to_writer(std::io::stdout().lock(), &wallets)
                .expect("Could not encode to stdout")
        }
        TweakSource::Utxos { legacy, db } => {
            let db = get_db(opts.readonly, &db, Default::default());

            let db = if legacy {
                db
            } else {
                db.with_prefix_module_id(LEGACY_HARDCODED_INSTANCE_ID_WALLET)
            };

            let utxos: Vec<ImportableWallet> = db
                .begin_transaction()
                .await
                .find_by_prefix(&UTXOPrefixKey)
                .await
                .map(|(UTXOKey(outpoint), SpendableUTXO { tweak, amount })| {
                    let descriptor = tweak_descriptor(
                        &base_descriptor,
                        &base_key,
                        &tweak,
                        bitcoin29_to_bitcoin30_network(network),
                    );

                    ImportableWallet {
                        outpoint: bitcoin29_to_bitcoin30_outpoint(outpoint),
                        descriptor,
                        amount_sat: bitcoin_29_to_bitcoin30_amount(amount),
                    }
                })
                .collect()
                .await;

            serde_json::to_writer(std::io::stdout().lock(), &utxos)
                .expect("Could not encode to stdout")
        }
        TweakSource::Epochs { db } => {
            let decoders = ModuleDecoderRegistry::from_iter([
                (
                    LEGACY_HARDCODED_INSTANCE_ID_LN,
                    LightningCommonInit::KIND,
                    <Lightning as ServerModule>::decoder(),
                ),
                (
                    LEGACY_HARDCODED_INSTANCE_ID_MINT,
                    MintCommonInit::KIND,
                    <Mint as ServerModule>::decoder(),
                ),
                (
                    LEGACY_HARDCODED_INSTANCE_ID_WALLET,
                    WalletCommonInit::KIND,
                    <Wallet as ServerModule>::decoder(),
                ),
            ]);

            let db = get_db(opts.readonly, &db, decoders);
            let mut dbtx = db.begin_transaction().await;

            let mut change_tweak_idx: u64 = 0;

            let tweaks = dbtx
                .find_by_prefix(&SignedSessionOutcomePrefix)
                .await
                .flat_map(
                    |(
                        _key,
                        SignedSessionOutcome {
                            session_outcome: block,
                            ..
                        },
                    )| {
                        let transaction_cis: Vec<Transaction> = block
                            .items
                            .into_iter()
                            .filter_map(|item| match item.item {
                                ConsensusItem::Transaction(tx) => Some(tx),
                                ConsensusItem::Module(_) => None,
                                ConsensusItem::Default { .. } => None,
                            })
                            .collect();

                        // Get all user-submitted tweaks and if we did a peg-out tx also return the
                        // consensus round's tweak used for change
                        let (mut peg_in_tweaks, peg_out_present) =
                            input_tweaks_output_present(transaction_cis.into_iter());

                        if peg_out_present {
                            info!("Found change output, adding tweak {change_tweak_idx} to list");
                            peg_in_tweaks.insert(nonce_from_idx(change_tweak_idx));
                            change_tweak_idx += 1;
                        }

                        futures::stream::iter(peg_in_tweaks.into_iter())
                    },
                );

            let wallets = tweaks
                .map(|tweak| {
                    let descriptor = tweak_descriptor(
                        &base_descriptor,
                        &base_key,
                        &tweak,
                        bitcoin29_to_bitcoin30_network(network),
                    );
                    ImportableWalletMin { descriptor }
                })
                .collect::<Vec<_>>()
                .await;

            serde_json::to_writer(std::io::stdout().lock(), &wallets)
                .expect("Could not encode to stdout")
        }
    }

    Ok(())
}

fn input_tweaks_output_present(
    transactions: impl Iterator<Item = Transaction>,
) -> (BTreeSet<[u8; 33]>, bool) {
    let mut contains_peg_out = false;
    let tweaks =
        transactions
            .flat_map(|tx| {
                if tx.outputs.iter().any(|output| {
                    output.module_instance_id() == LEGACY_HARDCODED_INSTANCE_ID_WALLET
                }) {
                    contains_peg_out = true;
                }

                tx.inputs.into_iter().filter_map(|input| {
                    if input.module_instance_id() != LEGACY_HARDCODED_INSTANCE_ID_WALLET {
                        return None;
                    }

                    Some(
                        input
                            .as_any()
                            .downcast_ref::<WalletInput>()
                            .expect("Instance id mapping incorrect")
                            .ensure_v0_ref()
                            .expect("recoverytool only supports v0 wallet inputs")
                            .0
                            .tweak_contract_key()
                            .serialize(),
                    )
                })
            })
            .collect::<BTreeSet<_>>();

    (tweaks, contains_peg_out)
}

fn tweak_descriptor(
    base_descriptor: &PegInDescriptor,
    base_sk: &SecretKey,
    tweak: &[u8; 33],
    network: Network,
) -> Descriptor<Key> {
    let secret_key = base_sk.tweak(tweak, secp256k1::SECP256K1);
    let pub_key =
        CompressedPublicKey::new(secp256k1::PublicKey::from_secret_key_global(&secret_key));
    base_descriptor
        .tweak(tweak, secp256k1::SECP256K1)
        .translate_pk(&mut SecretKeyInjector {
            secret: bitcoin::key::PrivateKey {
                compressed: true,
                network,
                inner: bitcoin29_to_bitcoin30_secp256k1_secret_key(secret_key),
            },
            public: pub_key,
        })
        .expect("can't fail")
}

/// A UTXO with its Bitcoin Core importable descriptor
#[derive(Debug, Serialize)]
struct ImportableWallet {
    outpoint: OutPoint,
    descriptor: Descriptor<Key>,
    #[serde(with = "bitcoin::amount::serde::as_sat")]
    amount_sat: bitcoin::Amount,
}

/// A Bitcoin Core importable descriptor
#[derive(Debug, Serialize)]
struct ImportableWalletMin {
    descriptor: Descriptor<Key>,
}

/// `MiniscriptKey` that is either a WIF-encoded private key or a compressed,
/// hex-encoded public key
#[derive(Debug, Clone, Copy, Eq)]
enum Key {
    Public(CompressedPublicKey),
    Private(bitcoin::key::PrivateKey),
}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(
            self.to_compressed_public_key()
                .cmp(&other.to_compressed_public_key()),
        )
    }
}

impl Ord for Key {
    fn cmp(&self, other: &Self) -> Ordering {
        self.to_compressed_public_key()
            .cmp(&other.to_compressed_public_key())
    }
}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.to_compressed_public_key()
            .eq(&other.to_compressed_public_key())
    }
}

impl std::hash::Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.to_compressed_public_key().hash(state)
    }
}

impl Key {
    fn to_compressed_public_key(self) -> CompressedPublicKey {
        match self {
            Key::Public(pk) => pk,
            Key::Private(sk) => {
                CompressedPublicKey::new(secp256k1::PublicKey::from_secret_key_global(
                    &bitcoin30_to_bitcoin29_secp256k1_secret_key(sk.inner),
                ))
            }
        }
    }
}

impl Display for Key {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Key::Public(pk) => Display::fmt(pk, f),
            Key::Private(sk) => Display::fmt(sk, f),
        }
    }
}

impl MiniscriptKey for Key {
    fn is_uncompressed(&self) -> bool {
        false
    }

    fn num_der_paths(&self) -> usize {
        0
    }

    type Sha256 = bitcoin::hashes::sha256::Hash;
    type Hash256 = miniscript::hash256::Hash;
    type Ripemd160 = bitcoin::hashes::ripemd160::Hash;
    type Hash160 = bitcoin::hashes::hash160::Hash;
}

impl ToPublicKey for Key {
    fn to_public_key(&self) -> miniscript::bitcoin::PublicKey {
        self.to_compressed_public_key().to_public_key()
    }

    fn to_sha256(
        hash: &<Self as MiniscriptKey>::Sha256,
    ) -> miniscript::bitcoin::hashes::sha256::Hash {
        *hash
    }

    fn to_hash256(hash: &<Self as MiniscriptKey>::Hash256) -> miniscript::hash256::Hash {
        *hash
    }

    fn to_ripemd160(
        hash: &<Self as MiniscriptKey>::Ripemd160,
    ) -> miniscript::bitcoin::hashes::ripemd160::Hash {
        *hash
    }

    fn to_hash160(
        hash: &<Self as MiniscriptKey>::Hash160,
    ) -> miniscript::bitcoin::hashes::hash160::Hash {
        *hash
    }
}

/// Miniscript [`Translator`] that replaces a public key with a private key we
/// know.
#[derive(Debug)]
struct SecretKeyInjector {
    secret: bitcoin::key::PrivateKey,
    public: CompressedPublicKey,
}

impl Translator<CompressedPublicKey, Key, ()> for SecretKeyInjector {
    fn pk(&mut self, pk: &CompressedPublicKey) -> Result<Key, ()> {
        if &self.public == pk {
            Ok(Key::Private(self.secret))
        } else {
            Ok(Key::Public(*pk))
        }
    }

    fn sha256(
        &mut self,
        _sha256: &<CompressedPublicKey as MiniscriptKey>::Sha256,
    ) -> Result<<Key as MiniscriptKey>::Sha256, ()> {
        unimplemented!()
    }

    fn hash256(
        &mut self,
        _hash256: &<CompressedPublicKey as MiniscriptKey>::Hash256,
    ) -> Result<<Key as MiniscriptKey>::Hash256, ()> {
        unimplemented!()
    }

    fn ripemd160(
        &mut self,
        _ripemd160: &<CompressedPublicKey as MiniscriptKey>::Ripemd160,
    ) -> Result<<Key as MiniscriptKey>::Ripemd160, ()> {
        unimplemented!()
    }

    fn hash160(
        &mut self,
        _hash160: &<CompressedPublicKey as MiniscriptKey>::Hash160,
    ) -> Result<<Key as MiniscriptKey>::Hash160, ()> {
        unimplemented!()
    }
}

#[test]
fn parses_valid_length_tweaks() {
    use hex::ToHex;

    let bad_length_tweak: [u8; 32] = rand::random::<[u8; 32]>();
    let bad_length_tweak_hex = bad_length_tweak.encode_hex::<String>();
    // rand::random only supports random byte arrays up to 32 bytes
    let good_length_tweak: [u8; 33] = core::array::from_fn(|_| rand::random::<u8>());
    let good_length_tweak_hex = good_length_tweak.encode_hex::<String>();
    assert_eq!(
        tweak_parser(good_length_tweak_hex.as_str()).expect("should parse valid length hex"),
        good_length_tweak
    );
    assert!(tweak_parser(bad_length_tweak_hex.as_str()).is_err());
}
