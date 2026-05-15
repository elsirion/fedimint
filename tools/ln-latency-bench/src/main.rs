#![allow(clippy::pedantic)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::Parser;
use fedimint_bip39::{Bip39RootSecretStrategy, Mnemonic};
use fedimint_client::secret::RootSecretStrategy;
use fedimint_client::{Client, ClientHandleArc, RootSecret};
use fedimint_connectors::ConnectorRegistry;
use fedimint_core::Amount;
use fedimint_core::db::Database;
use fedimint_core::util::SafeUrl;
use fedimint_ln_client::{
    InternalPayState, LightningClientInit, LightningClientModule, LightningPaymentOutcome,
    LnPayState, LnReceiveState, OutgoingLightningPayment, PayType,
};
use fedimint_ln_client::common::LightningGateway;
use fedimint_lnv2_client::LightningClientModule as LightningClientModuleV2;
use futures::StreamExt;
use lightning_invoice::{Bolt11InvoiceDescription, Description};
use rand::thread_rng;
use serde_json::Value;
use tracing::info;

/// LN payment latency benchmark for Fedimint federations.
///
/// Tests internal (same-federation) and cross-federation LN payments,
/// comparing iroh:// vs https:// gateways, and LNv1 vs LNv2.
#[derive(Parser)]
struct Cli {
    /// Data directory for federation A
    #[arg(long, env = "BENCH_FED_A_DIR")]
    fed_a_dir: PathBuf,

    /// Data directory for federation B
    #[arg(long, env = "BENCH_FED_B_DIR")]
    fed_b_dir: PathBuf,

    /// Number of iterations per scenario
    #[arg(long, default_value = "100")]
    iterations: usize,

    /// Payment amount in msat
    #[arg(long, default_value = "100000")]
    amount_msat: u64,

    /// Per-payment timeout in seconds
    #[arg(long, default_value = "180")]
    timeout_secs: u64,

    /// Only use these gateway IDs (hex pubkeys, comma-separated). If set,
    /// only gateways matching these IDs are used for LNv1 scenarios.
    #[arg(long, env = "BENCH_GATEWAY_IDS", value_delimiter = ',')]
    gateway_ids: Vec<String>,

    /// Gateway URL to use for LNv2 scenarios (bypasses server-side registration).
    /// The gateway must support both LNv1 and LNv2 at this URL.
    #[arg(long, env = "BENCH_LNV2_GATEWAY")]
    lnv2_gateway: Option<SafeUrl>,
}

#[derive(Clone, Debug)]
struct GatewayInfo {
    gateway: LightningGateway,
    transport: GatewayTransport,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum GatewayTransport {
    Https,
    Iroh,
    Other(String),
}

impl std::fmt::Display for GatewayTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GatewayTransport::Https => write!(f, "https"),
            GatewayTransport::Iroh => write!(f, "iroh"),
            GatewayTransport::Other(s) => write!(f, "{s}"),
        }
    }
}

#[derive(Clone, Debug)]
enum ScenarioProtocol {
    LnV1 {
        recv_gateway: GatewayInfo,
        send_gateway: GatewayInfo,
    },
    LnV2 {
        gateway_url: SafeUrl,
    },
    /// Sender uses LNv2, receiver uses LNv1
    V2SendV1Recv {
        send_gateway_url: SafeUrl,
        recv_gateway: GatewayInfo,
    },
    /// Sender uses LNv1, receiver uses LNv2
    V1SendV2Recv {
        send_gateway: GatewayInfo,
        recv_gateway_url: SafeUrl,
    },
}

#[derive(Clone, Debug)]
struct Scenario {
    name: String,
    recv_label: &'static str,
    send_label: &'static str,
    protocol: ScenarioProtocol,
}

#[derive(Clone, Debug)]
struct Sample {
    invoice_create_ms: u64,
    pay_ms: u64,
    await_recv_ms: u64,
    total_ms: u64,
    success: bool,
}

#[derive(Clone, Debug)]
struct Stats {
    count: usize,
    median: u64,
    p90: u64,
    p95: u64,
    iqr: u64,
    sigma: f64,
    mean: f64,
    min: u64,
    max: u64,
}

fn compute_stats(values: &[u64]) -> Stats {
    let n = values.len();
    if n == 0 {
        return Stats {
            count: 0,
            median: 0,
            p90: 0,
            p95: 0,
            iqr: 0,
            sigma: 0.0,
            mean: 0.0,
            min: 0,
            max: 0,
        };
    }
    let mut sorted = values.to_vec();
    sorted.sort();

    let percentile = |p: f64| -> u64 {
        let idx = (p / 100.0 * (n - 1) as f64).round() as usize;
        sorted[idx.min(n - 1)]
    };

    let mean = sorted.iter().sum::<u64>() as f64 / n as f64;
    let variance = sorted.iter().map(|&v| (v as f64 - mean).powi(2)).sum::<f64>() / n as f64;

    Stats {
        count: n,
        median: percentile(50.0),
        p90: percentile(90.0),
        p95: percentile(95.0),
        iqr: percentile(75.0).saturating_sub(percentile(25.0)),
        sigma: variance.sqrt(),
        mean,
        min: sorted[0],
        max: sorted[n - 1],
    }
}

async fn open_db(dir: &PathBuf) -> anyhow::Result<Database> {
    let db_path = dir.join("client.db");
    Ok(fedimint_rocksdb::RocksDb::build(db_path)
        .open()
        .await?
        .into())
}

async fn load_mnemonic(db: &Database) -> anyhow::Result<Mnemonic> {
    if let Ok(entropy) = Client::load_decodable_client_secret::<Vec<u8>>(db).await {
        Ok(Mnemonic::from_entropy(&entropy)?)
    } else {
        let mnemonic = Bip39RootSecretStrategy::<12>::random(&mut thread_rng());
        Client::store_encodable_client_secret(db, mnemonic.to_entropy()).await?;
        Ok(mnemonic)
    }
}

async fn open_client(dir: &PathBuf) -> anyhow::Result<ClientHandleArc> {
    let db = open_db(dir).await?;
    let mnemonic = load_mnemonic(&db).await?;
    let root_secret =
        RootSecret::StandardDoubleDerive(Bip39RootSecretStrategy::<12>::to_root_secret(&mnemonic));

    let connectors = ConnectorRegistry::build_from_client_defaults()
        .iroh_next(true)
        .iroh_pkarr_dht(true)
        .bind()
        .await?;

    let mut builder = Client::builder()
        .await?
        .with_iroh_enable_dht(true)
        .with_iroh_enable_next(true);
    builder.with_module_inits({
        let mut inits = fedimint_client::module_init::ClientModuleInitRegistry::new();
        inits.attach(LightningClientInit::default());
        inits.attach(fedimint_lnv2_client::LightningClientInit::default());
        inits.attach(fedimint_mint_client::MintClientInit);
        inits.attach(fedimint_wallet_client::WalletClientInit::default());
        inits.attach(fedimint_meta_client::MetaClientInit);
        inits
    });

    let client = builder.open(connectors, db, root_secret).await?;
    Ok(Arc::new(client))
}

fn classify_transport(api: &str) -> GatewayTransport {
    if api.starts_with("iroh://") {
        GatewayTransport::Iroh
    } else if api.starts_with("https://") || api.starts_with("http://") {
        GatewayTransport::Https
    } else {
        GatewayTransport::Other(api.split("://").next().unwrap_or("unknown").to_string())
    }
}

fn has_lnv2(client: &ClientHandleArc) -> bool {
    client
        .get_first_module::<LightningClientModuleV2>()
        .is_ok()
}

async fn discover_gateways(client: &ClientHandleArc) -> anyhow::Result<Vec<GatewayInfo>> {
    let ln = client.get_first_module::<LightningClientModule>()?;

    let mut last_err = None;
    for attempt in 0..10 {
        match ln.update_gateway_cache().await {
            Ok(()) => {
                let announcements = ln.list_gateways().await;
                if !announcements.is_empty() {
                    return Ok(announcements
                        .into_iter()
                        .map(|ann| {
                            let transport = classify_transport(ann.info.api.as_str());
                            GatewayInfo {
                                gateway: ann.info,
                                transport,
                            }
                        })
                        .collect());
                }
                println!("  attempt {}: no gateways yet, retrying...", attempt + 1);
            }
            Err(e) => {
                println!(
                    "  attempt {}: gateway discovery failed: {e:#}, retrying...",
                    attempt + 1
                );
                last_err = Some(e);
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("No gateways found after retries")))
}

async fn run_payment_v1(
    send_client: &ClientHandleArc,
    recv_client: &ClientHandleArc,
    send_gw: &LightningGateway,
    recv_gw: &LightningGateway,
    amount: Amount,
) -> anyhow::Result<Sample> {
    let total_start = Instant::now();

    // Step 1: Create invoice on receiver
    let invoice_start = Instant::now();
    let recv_ln = recv_client.get_first_module::<LightningClientModule>()?;
    let desc = Description::new("bench".to_string())?;
    let (recv_op_id, invoice, _) = recv_ln
        .create_bolt11_invoice(
            amount,
            Bolt11InvoiceDescription::Direct(desc),
            None,
            (),
            Some(recv_gw.clone()),
        )
        .await?;
    let invoice_create_ms = invoice_start.elapsed().as_millis() as u64;

    // Step 2: Pay invoice from sender — return as soon as preimage is available
    let pay_start = Instant::now();
    let send_ln = send_client.get_first_module::<LightningClientModule>()?;
    let OutgoingLightningPayment {
        payment_type,
        contract_id: _,
        fee: _,
    } = send_ln
        .pay_bolt11_invoice(Some(send_gw.clone()), invoice, ())
        .await?;

    match payment_type {
        PayType::Lightning(op_id) => {
            // Break on AwaitingChange — the gateway already confirmed payment
            // at this point, we just haven't waited for change reissuance
            let mut stream = send_ln.subscribe_ln_pay(op_id).await?.into_stream();
            while let Some(state) = stream.next().await {
                match state {
                    LnPayState::AwaitingChange | LnPayState::Success { .. } => break,
                    LnPayState::Refunded { gateway_error } => {
                        bail!("Payment refunded: {gateway_error:?}");
                    }
                    LnPayState::UnexpectedError { error_message } => {
                        bail!("Payment error: {error_message}");
                    }
                    _ => {}
                }
            }
        }
        PayType::Internal(op_id) => {
            let mut stream = send_ln.subscribe_internal_pay(op_id).await?.into_stream();
            while let Some(state) = stream.next().await {
                match state {
                    InternalPayState::Preimage(_) => break,
                    InternalPayState::RefundSuccess { error, .. } => {
                        bail!("Internal payment refunded: {error:?}");
                    }
                    InternalPayState::RefundError { error_message, .. } => {
                        bail!("Internal payment refund error: {error_message}");
                    }
                    _ => {}
                }
            }
        }
    }
    let pay_ms = pay_start.elapsed().as_millis() as u64;

    // Step 3: Await receive
    let recv_start = Instant::now();
    let mut updates = recv_ln
        .subscribe_ln_receive(recv_op_id)
        .await?
        .into_stream();
    while let Some(update) = updates.next().await {
        match update {
            LnReceiveState::Claimed => break,
            LnReceiveState::Canceled { reason } => {
                bail!("Receive canceled: {reason}");
            }
            _ => {}
        }
    }
    let await_recv_ms = recv_start.elapsed().as_millis() as u64;

    let total_ms = total_start.elapsed().as_millis() as u64;

    Ok(Sample {
        invoice_create_ms,
        pay_ms,
        await_recv_ms,
        total_ms,
        success: true,
    })
}

async fn run_payment_v2(
    send_client: &ClientHandleArc,
    recv_client: &ClientHandleArc,
    gateway_url: &SafeUrl,
    amount: Amount,
) -> anyhow::Result<Sample> {
    let total_start = Instant::now();

    // Step 1: Create invoice on receiver via lnv2
    let invoice_start = Instant::now();
    let recv_ln = recv_client.get_first_module::<LightningClientModuleV2>()?;
    let (invoice, recv_op_id) = recv_ln
        .receive(
            amount,
            3600,
            fedimint_lnv2_client::common::Bolt11InvoiceDescription::Direct(String::new()),
            Some(gateway_url.clone()),
            Value::Null,
        )
        .await?;
    let invoice_create_ms = invoice_start.elapsed().as_millis() as u64;

    // Step 2: Pay invoice from sender via lnv2
    let pay_start = Instant::now();
    let send_ln = send_client.get_first_module::<LightningClientModuleV2>()?;
    let send_op_id = send_ln
        .send(invoice, Some(gateway_url.clone()), Value::Null)
        .await?;
    let send_state = send_ln
        .await_final_send_operation_state(send_op_id)
        .await?;
    let pay_ms = pay_start.elapsed().as_millis() as u64;

    match send_state {
        fedimint_lnv2_client::FinalSendOperationState::Success => {}
        other => bail!("LNv2 send failed: {other:?}"),
    }

    // Step 3: Await receive
    let recv_start = Instant::now();
    let recv_state = recv_ln
        .await_final_receive_operation_state(recv_op_id)
        .await?;
    let await_recv_ms = recv_start.elapsed().as_millis() as u64;

    match recv_state {
        fedimint_lnv2_client::FinalReceiveOperationState::Claimed => {}
        other => bail!("LNv2 receive failed: {other:?}"),
    }

    let total_ms = total_start.elapsed().as_millis() as u64;

    Ok(Sample {
        invoice_create_ms,
        pay_ms,
        await_recv_ms,
        total_ms,
        success: true,
    })
}

/// Sender uses LNv2, receiver uses LNv1
async fn run_payment_v2_send_v1_recv(
    send_client: &ClientHandleArc,
    recv_client: &ClientHandleArc,
    send_gw_url: &SafeUrl,
    recv_gw: &LightningGateway,
    amount: Amount,
) -> anyhow::Result<Sample> {
    let total_start = Instant::now();

    // Step 1: Create invoice on receiver via LNv1
    let invoice_start = Instant::now();
    let recv_ln = recv_client.get_first_module::<LightningClientModule>()?;
    let desc = Description::new("bench".to_string())?;
    let (recv_op_id, invoice, _) = recv_ln
        .create_bolt11_invoice(
            amount,
            Bolt11InvoiceDescription::Direct(desc),
            None,
            (),
            Some(recv_gw.clone()),
        )
        .await?;
    let invoice_create_ms = invoice_start.elapsed().as_millis() as u64;

    // Step 2: Pay via LNv2
    let pay_start = Instant::now();
    let send_ln = send_client.get_first_module::<LightningClientModuleV2>()?;
    let send_op_id = send_ln
        .send(invoice, Some(send_gw_url.clone()), Value::Null)
        .await?;
    let send_state = send_ln
        .await_final_send_operation_state(send_op_id)
        .await?;
    let pay_ms = pay_start.elapsed().as_millis() as u64;

    match send_state {
        fedimint_lnv2_client::FinalSendOperationState::Success => {}
        other => bail!("LNv2 send failed: {other:?}"),
    }

    // Step 3: Await receive via LNv1
    let recv_start = Instant::now();
    let mut updates = recv_ln
        .subscribe_ln_receive(recv_op_id)
        .await?
        .into_stream();
    while let Some(update) = updates.next().await {
        match update {
            LnReceiveState::Claimed => break,
            LnReceiveState::Canceled { reason } => {
                bail!("Receive canceled: {reason}");
            }
            _ => {}
        }
    }
    let await_recv_ms = recv_start.elapsed().as_millis() as u64;

    let total_ms = total_start.elapsed().as_millis() as u64;
    Ok(Sample {
        invoice_create_ms,
        pay_ms,
        await_recv_ms,
        total_ms,
        success: true,
    })
}

/// Sender uses LNv1, receiver uses LNv2
async fn run_payment_v1_send_v2_recv(
    send_client: &ClientHandleArc,
    recv_client: &ClientHandleArc,
    send_gw: &LightningGateway,
    recv_gw_url: &SafeUrl,
    amount: Amount,
) -> anyhow::Result<Sample> {
    let total_start = Instant::now();

    // Step 1: Create invoice on receiver via LNv2
    let invoice_start = Instant::now();
    let recv_ln = recv_client.get_first_module::<LightningClientModuleV2>()?;
    let (invoice, recv_op_id) = recv_ln
        .receive(
            amount,
            3600,
            fedimint_lnv2_client::common::Bolt11InvoiceDescription::Direct(String::new()),
            Some(recv_gw_url.clone()),
            Value::Null,
        )
        .await?;
    let invoice_create_ms = invoice_start.elapsed().as_millis() as u64;

    // Step 2: Pay via LNv1
    let pay_start = Instant::now();
    let send_ln = send_client.get_first_module::<LightningClientModule>()?;
    let OutgoingLightningPayment {
        payment_type,
        contract_id: _,
        fee: _,
    } = send_ln
        .pay_bolt11_invoice(Some(send_gw.clone()), invoice, ())
        .await?;
    match payment_type {
        PayType::Lightning(op_id) => {
            let mut stream = send_ln.subscribe_ln_pay(op_id).await?.into_stream();
            while let Some(state) = stream.next().await {
                match state {
                    LnPayState::AwaitingChange | LnPayState::Success { .. } => break,
                    LnPayState::Refunded { gateway_error } => {
                        bail!("Payment refunded: {gateway_error:?}");
                    }
                    LnPayState::UnexpectedError { error_message } => {
                        bail!("Payment error: {error_message}");
                    }
                    _ => {}
                }
            }
        }
        PayType::Internal(op_id) => {
            let mut stream = send_ln.subscribe_internal_pay(op_id).await?.into_stream();
            while let Some(state) = stream.next().await {
                match state {
                    InternalPayState::Preimage(_) => break,
                    InternalPayState::RefundSuccess { error, .. } => {
                        bail!("Internal payment refunded: {error:?}");
                    }
                    InternalPayState::RefundError { error_message, .. } => {
                        bail!("Internal payment refund error: {error_message}");
                    }
                    _ => {}
                }
            }
        }
    }
    let pay_ms = pay_start.elapsed().as_millis() as u64;

    // Step 3: Await receive via LNv2
    let recv_start = Instant::now();
    let recv_state = recv_ln
        .await_final_receive_operation_state(recv_op_id)
        .await?;
    let await_recv_ms = recv_start.elapsed().as_millis() as u64;

    match recv_state {
        fedimint_lnv2_client::FinalReceiveOperationState::Claimed => {}
        other => bail!("LNv2 receive failed: {other:?}"),
    }

    let total_ms = total_start.elapsed().as_millis() as u64;
    Ok(Sample {
        invoice_create_ms,
        pay_ms,
        await_recv_ms,
        total_ms,
        success: true,
    })
}

fn build_scenarios(
    gateways_a: &[GatewayInfo],
    gateways_b: &[GatewayInfo],
    lnv2_gateway: Option<&SafeUrl>,
    client_a_has_v2: bool,
    client_b_has_v2: bool,
) -> Vec<Scenario> {
    let mut scenarios = Vec::new();

    let a_https: Vec<_> = gateways_a
        .iter()
        .filter(|g| g.transport == GatewayTransport::Https)
        .collect();
    let a_iroh: Vec<_> = gateways_a
        .iter()
        .filter(|g| g.transport == GatewayTransport::Iroh)
        .collect();
    let b_https: Vec<_> = gateways_b
        .iter()
        .filter(|g| g.transport == GatewayTransport::Https)
        .collect();
    let b_iroh: Vec<_> = gateways_b
        .iter()
        .filter(|g| g.transport == GatewayTransport::Iroh)
        .collect();

    // LNv1 internal Fed A
    if let Some(gw) = a_https.first() {
        scenarios.push(Scenario {
            name: "internal_A_v1".to_string(),
            recv_label: "A",
            send_label: "A",
            protocol: ScenarioProtocol::LnV1 {
                recv_gateway: (*gw).clone(),
                send_gateway: (*gw).clone(),
            },
        });
    }
    if let Some(gw) = a_iroh.first() {
        scenarios.push(Scenario {
            name: "internal_A_v1_iroh".to_string(),
            recv_label: "A",
            send_label: "A",
            protocol: ScenarioProtocol::LnV1 {
                recv_gateway: (*gw).clone(),
                send_gateway: (*gw).clone(),
            },
        });
    }

    // LNv2 internal Fed A
    if let Some(gw_url) = lnv2_gateway {
        if client_a_has_v2 {
            scenarios.push(Scenario {
                name: "internal_A_v2".to_string(),
                recv_label: "A",
                send_label: "A",
                protocol: ScenarioProtocol::LnV2 {
                    gateway_url: gw_url.clone(),
                },
            });

            // Mixed: v2 send, v1 recv (internal Fed A)
            if let Some(gw) = a_https.first() {
                scenarios.push(Scenario {
                    name: "internal_A_v2send_v1recv".to_string(),
                    recv_label: "A",
                    send_label: "A",
                    protocol: ScenarioProtocol::V2SendV1Recv {
                        send_gateway_url: gw_url.clone(),
                        recv_gateway: (*gw).clone(),
                    },
                });

                // Mixed: v1 send, v2 recv (internal Fed A)
                scenarios.push(Scenario {
                    name: "internal_A_v1send_v2recv".to_string(),
                    recv_label: "A",
                    send_label: "A",
                    protocol: ScenarioProtocol::V1SendV2Recv {
                        send_gateway: (*gw).clone(),
                        recv_gateway_url: gw_url.clone(),
                    },
                });
            }
        }
    }

    // LNv1 internal Fed B
    if let Some(gw) = b_https.first() {
        scenarios.push(Scenario {
            name: "internal_B_v1".to_string(),
            recv_label: "B",
            send_label: "B",
            protocol: ScenarioProtocol::LnV1 {
                recv_gateway: (*gw).clone(),
                send_gateway: (*gw).clone(),
            },
        });
    }
    if let Some(gw) = b_iroh.first() {
        scenarios.push(Scenario {
            name: "internal_B_v1_iroh".to_string(),
            recv_label: "B",
            send_label: "B",
            protocol: ScenarioProtocol::LnV1 {
                recv_gateway: (*gw).clone(),
                send_gateway: (*gw).clone(),
            },
        });
    }

    // LNv2 internal Fed B
    if let Some(gw_url) = lnv2_gateway {
        if client_b_has_v2 {
            scenarios.push(Scenario {
                name: "internal_B_v2".to_string(),
                recv_label: "B",
                send_label: "B",
                protocol: ScenarioProtocol::LnV2 {
                    gateway_url: gw_url.clone(),
                },
            });
        }
    }

    // Cross-fed LNv1
    if let (Some(send_gw), Some(recv_gw)) = (a_https.first(), b_https.first()) {
        scenarios.push(Scenario {
            name: "cross_A_to_B_v1".to_string(),
            recv_label: "B",
            send_label: "A",
            protocol: ScenarioProtocol::LnV1 {
                recv_gateway: (*recv_gw).clone(),
                send_gateway: (*send_gw).clone(),
            },
        });
    }
    if let (Some(send_gw), Some(recv_gw)) = (a_https.first(), b_iroh.first()) {
        scenarios.push(Scenario {
            name: "cross_A_to_B_v1_iroh".to_string(),
            recv_label: "B",
            send_label: "A",
            protocol: ScenarioProtocol::LnV1 {
                recv_gateway: (*recv_gw).clone(),
                send_gateway: (*send_gw).clone(),
            },
        });
    }
    if let (Some(send_gw), Some(recv_gw)) = (b_https.first(), a_https.first()) {
        scenarios.push(Scenario {
            name: "cross_B_to_A_v1".to_string(),
            recv_label: "A",
            send_label: "B",
            protocol: ScenarioProtocol::LnV1 {
                recv_gateway: (*recv_gw).clone(),
                send_gateway: (*send_gw).clone(),
            },
        });
    }
    if let (Some(send_gw), Some(recv_gw)) = (b_iroh.first(), a_https.first()) {
        scenarios.push(Scenario {
            name: "cross_B_to_A_v1_iroh".to_string(),
            recv_label: "A",
            send_label: "B",
            protocol: ScenarioProtocol::LnV1 {
                recv_gateway: (*recv_gw).clone(),
                send_gateway: (*send_gw).clone(),
            },
        });
    }

    // Cross-fed LNv2 (both sides need lnv2 module)
    if let Some(gw_url) = lnv2_gateway {
        if client_a_has_v2 && client_b_has_v2 {
            scenarios.push(Scenario {
                name: "cross_A_to_B_v2".to_string(),
                recv_label: "B",
                send_label: "A",
                protocol: ScenarioProtocol::LnV2 {
                    gateway_url: gw_url.clone(),
                },
            });
            scenarios.push(Scenario {
                name: "cross_B_to_A_v2".to_string(),
                recv_label: "A",
                send_label: "B",
                protocol: ScenarioProtocol::LnV2 {
                    gateway_url: gw_url.clone(),
                },
            });
        }

        // Cross-fed mixed: A has v2, so test v2 send from A and v2 recv on A
        if client_a_has_v2 {
            // A(v2) sends to B(v1)
            if let Some(recv_gw) = b_https.first() {
                scenarios.push(Scenario {
                    name: "cross_A_to_B_v2send".to_string(),
                    recv_label: "B",
                    send_label: "A",
                    protocol: ScenarioProtocol::V2SendV1Recv {
                        send_gateway_url: gw_url.clone(),
                        recv_gateway: (*recv_gw).clone(),
                    },
                });
            }

            // B(v1) sends to A(v2)
            if let Some(send_gw) = b_https.first() {
                scenarios.push(Scenario {
                    name: "cross_B_to_A_v2recv".to_string(),
                    recv_label: "A",
                    send_label: "B",
                    protocol: ScenarioProtocol::V1SendV2Recv {
                        send_gateway: (*send_gw).clone(),
                        recv_gateway_url: gw_url.clone(),
                    },
                });
            }
        }
    }

    scenarios
}

fn print_results(scenario_name: &str, samples: &[Sample]) {
    let successful: Vec<_> = samples.iter().filter(|s| s.success).collect();
    let totals: Vec<u64> = successful.iter().map(|s| s.total_ms).collect();
    let invoices: Vec<u64> = successful.iter().map(|s| s.invoice_create_ms).collect();
    let pays: Vec<u64> = successful.iter().map(|s| s.pay_ms).collect();
    let recvs: Vec<u64> = successful.iter().map(|s| s.await_recv_ms).collect();

    let total_stats = compute_stats(&totals);
    let invoice_stats = compute_stats(&invoices);
    let pay_stats = compute_stats(&pays);
    let recv_stats = compute_stats(&recvs);

    println!("\n{}", "=".repeat(60));
    println!("  {scenario_name}");
    println!("  {}/{} succeeded", successful.len(), samples.len());
    println!("{}\n", "=".repeat(60));

    println!(
        "  {:>18} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "phase", "median", "p90", "p95", "IQR", "σ", "mean"
    );
    println!(
        "  {:>18} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "-----", "------", "---", "---", "---", "-", "----"
    );
    for (label, stats) in [
        ("invoice_create", &invoice_stats),
        ("ln_pay", &pay_stats),
        ("await_recv", &recv_stats),
        ("TOTAL", &total_stats),
    ] {
        println!(
            "  {:>18} {:>7}ms {:>7}ms {:>7}ms {:>7}ms {:>7.0}ms {:>7.0}ms",
            label, stats.median, stats.p90, stats.p95, stats.iqr, stats.sigma, stats.mean,
        );
    }
    println!(
        "\n  total range: {}ms - {}ms",
        total_stats.min, total_stats.max
    );
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    fedimint_logging::TracingSetup::default()
        .with_base_level("info")
        .init()
        .expect("tracing initializes");

    let cli = Cli::parse();
    let amount = Amount::from_msats(cli.amount_msat);

    println!("Opening clients...");
    let client_a = open_client(&cli.fed_a_dir)
        .await
        .context("Failed to open federation A client")?;
    let client_b = open_client(&cli.fed_b_dir)
        .await
        .context("Failed to open federation B client")?;
    println!("Clients ready.");

    let a_has_v2 = has_lnv2(&client_a);
    let b_has_v2 = has_lnv2(&client_b);
    println!(
        "LNv2 support: Fed A={}, Fed B={}",
        if a_has_v2 { "yes" } else { "no" },
        if b_has_v2 { "yes" } else { "no" },
    );

    println!("Discovering gateways...");
    let mut gateways_a = discover_gateways(&client_a).await?;
    let mut gateways_b = discover_gateways(&client_b).await?;

    if !cli.gateway_ids.is_empty() {
        let allowed: std::collections::HashSet<&str> =
            cli.gateway_ids.iter().map(|s| s.as_str()).collect();
        let filter = |gws: &mut Vec<GatewayInfo>| {
            gws.retain(|g| allowed.contains(g.gateway.gateway_id.to_string().as_str()));
        };
        filter(&mut gateways_a);
        filter(&mut gateways_b);
        println!("  filtered to {} allowed gateway IDs", cli.gateway_ids.len());
    }

    println!("\nFed A gateways (LNv1):");
    for gw in &gateways_a {
        println!(
            "  [{}] {} ({})",
            gw.transport, gw.gateway.lightning_alias, gw.gateway.api
        );
    }
    println!("\nFed B gateways (LNv1):");
    for gw in &gateways_b {
        println!(
            "  [{}] {} ({})",
            gw.transport, gw.gateway.lightning_alias, gw.gateway.api
        );
    }
    if let Some(ref gw) = cli.lnv2_gateway {
        println!("\nLNv2 gateway override: {gw}");
    }

    let scenarios = build_scenarios(
        &gateways_a,
        &gateways_b,
        cli.lnv2_gateway.as_ref(),
        a_has_v2,
        b_has_v2,
    );
    if scenarios.is_empty() {
        bail!("No test scenarios could be constructed from available gateways");
    }

    println!("\nScenarios to test:");
    for s in &scenarios {
        let proto = match &s.protocol {
            ScenarioProtocol::LnV1 { .. } => "v1",
            ScenarioProtocol::LnV2 { .. } => "v2",
            ScenarioProtocol::V2SendV1Recv { .. } => "v2send/v1recv",
            ScenarioProtocol::V1SendV2Recv { .. } => "v1send/v2recv",
        };
        println!(
            "  {} (send={}, recv={}, proto={})",
            s.name, s.send_label, s.recv_label, proto,
        );
    }

    let mut all_results: BTreeMap<String, Vec<Sample>> = BTreeMap::new();

    println!(
        "\nRunning {} iterations per scenario ({} scenarios, {} total payments)...\n",
        cli.iterations,
        scenarios.len(),
        cli.iterations * scenarios.len()
    );

    for i in 0..cli.iterations {
        for scenario in &scenarios {
            let (send_client, recv_client) = match (scenario.send_label, scenario.recv_label) {
                ("A", "A") => (&client_a, &client_a),
                ("B", "B") => (&client_b, &client_b),
                ("A", "B") => (&client_a, &client_b),
                ("B", "A") => (&client_b, &client_a),
                _ => unreachable!(),
            };

            let timeout = Duration::from_secs(cli.timeout_secs);
            let result = match &scenario.protocol {
                ScenarioProtocol::LnV1 {
                    recv_gateway,
                    send_gateway,
                } => {
                    tokio::time::timeout(
                        timeout,
                        run_payment_v1(
                            send_client,
                            recv_client,
                            &send_gateway.gateway,
                            &recv_gateway.gateway,
                            amount,
                        ),
                    )
                    .await
                }
                ScenarioProtocol::LnV2 { gateway_url } => {
                    tokio::time::timeout(
                        timeout,
                        run_payment_v2(send_client, recv_client, gateway_url, amount),
                    )
                    .await
                }
                ScenarioProtocol::V2SendV1Recv {
                    send_gateway_url,
                    recv_gateway,
                } => {
                    tokio::time::timeout(
                        timeout,
                        run_payment_v2_send_v1_recv(
                            send_client,
                            recv_client,
                            send_gateway_url,
                            &recv_gateway.gateway,
                            amount,
                        ),
                    )
                    .await
                }
                ScenarioProtocol::V1SendV2Recv {
                    send_gateway,
                    recv_gateway_url,
                } => {
                    tokio::time::timeout(
                        timeout,
                        run_payment_v1_send_v2_recv(
                            send_client,
                            recv_client,
                            &send_gateway.gateway,
                            recv_gateway_url,
                            amount,
                        ),
                    )
                    .await
                }
            }
            .unwrap_or_else(|_| Err(anyhow::anyhow!("timeout after {}s", cli.timeout_secs)));

            let entry = all_results.entry(scenario.name.clone()).or_default();

            match result {
                Ok(sample) => {
                    info!(
                        "[{}/{}] {} -> {}ms (invoice={}ms pay={}ms recv={}ms)",
                        i + 1,
                        cli.iterations,
                        scenario.name,
                        sample.total_ms,
                        sample.invoice_create_ms,
                        sample.pay_ms,
                        sample.await_recv_ms,
                    );
                    println!(
                        "  [{:>3}/{}] {:>30} {:>6}ms  (inv={:>5}ms pay={:>5}ms recv={:>5}ms)",
                        i + 1,
                        cli.iterations,
                        scenario.name,
                        sample.total_ms,
                        sample.invoice_create_ms,
                        sample.pay_ms,
                        sample.await_recv_ms,
                    );
                    entry.push(sample);
                }
                Err(e) => {
                    println!(
                        "  [{:>3}/{}] {:>30} FAILED: {}",
                        i + 1,
                        cli.iterations,
                        scenario.name,
                        e
                    );
                    entry.push(Sample {
                        invoice_create_ms: 0,
                        pay_ms: 0,
                        await_recv_ms: 0,
                        total_ms: 0,
                        success: false,
                    });
                }
            }
        }
    }

    // Print summary
    println!("\n\n{}", "#".repeat(70));
    println!(
        "  RESULTS SUMMARY ({} iterations per scenario)",
        cli.iterations
    );
    println!("{}", "#".repeat(70));

    for (name, samples) in &all_results {
        print_results(name, samples);
    }

    // Print compact comparison table
    println!("\n\nCOMPACT COMPARISON (total e2e ms):\n");
    println!(
        "  {:>30} {:>5} {:>7} {:>7} {:>7} {:>7} {:>7}",
        "scenario", "ok/n", "median", "p90", "p95", "IQR", "σ"
    );
    println!(
        "  {:>30} {:>5} {:>7} {:>7} {:>7} {:>7} {:>7}",
        "--------", "----", "------", "---", "---", "---", "-"
    );
    for (name, samples) in &all_results {
        let successful: Vec<u64> = samples
            .iter()
            .filter(|s| s.success)
            .map(|s| s.total_ms)
            .collect();
        let stats = compute_stats(&successful);
        println!(
            "  {:>30} {}/{:>3} {:>6}ms {:>6}ms {:>6}ms {:>6}ms {:>6.0}ms",
            name,
            stats.count,
            samples.len(),
            stats.median,
            stats.p90,
            stats.p95,
            stats.iqr,
            stats.sigma,
        );
    }

    println!();
    Ok(())
}
