use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use fedimint_api::cancellable::Cancellable;
use fedimint_api::config::GenerateConfig;
use fedimint_api::task::TaskGroup;
use fedimint_api::{Amount, PeerId};
use fedimint_core::config::ClientConfig;
use fedimint_server::config::{PeerServerParams, ServerConfig, ServerConfigParams};
use itertools::Itertools;
use rand::rngs::OsRng;
use ring::aead::LessSafeKey;
use tokio_rustls::rustls;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::encrypt::*;

struct CreateCert {
    /// Directory to output all the generated config files
    dir_out_path: PathBuf,
    /// Our external address
    address: String,
    /// Our base port, ports may be used from base_port to base_port+10, default 4000
    base_port: u16,
    /// Our node name, must be unique among peers
    name: String,
    /// The password that encrypts the configs, will prompt if not passed in
    password: String,
}

/// All peers must run distributed key gen at the same time to create configs
struct Run {
    /// Directory to output all the generated config files
    dir_out_path: PathBuf,
    /// Federation name, same for all peers
    federation_name: String,
    /// Comma-separated list of connection certs from all peers (including ours)
    certs: Vec<String>,
    /// `bitcoind` json rpc endpoint
    bitcoind_rpc: String,
    /// Available denominations of notes issues by the federation (comma separated)
    denominations: Vec<Amount>,
    /// The password that encrypts the configs, will prompt if not passed in
    password: String,
}

pub fn create_cert(
    dir_out_path: PathBuf,
    address: String,
    guardian_name: String,
    password: String,
    port: u16,
) -> String {
    let salt: [u8; 16] = rand::random();
    println!("dir_out_path: {:?}", dir_out_path);
    fs::write(dir_out_path.join(SALT_FILE), hex::encode(salt)).expect("write error");
    let key = get_key(password, dir_out_path.join(SALT_FILE));
    gen_tls(&dir_out_path, address, port, guardian_name, &key)
}

// #[tokio::main]
// pub async fn distributedgen() {
//     let mut task_group = TaskGroup::new();

//     let command: Command = Cli::parse().command;
//     match command {
//         Command::CreateCert {
//             dir_out_path,
//             address,
//             base_port,
//             name,
//             password,
//         } => {
//             let salt: [u8; 16] = rand::random();
//             fs::write(dir_out_path.join(SALT_FILE), hex::encode(salt)).expect("write error");
//             let key = get_key(password, dir_out_path.join(SALT_FILE));
//             let config_str = gen_tls(&dir_out_path, address, base_port, name, &key);
//             println!("{}", config_str);
//         }
//         Command::Run {
//             dir_out_path,
//             federation_name,
//             certs,
//             bitcoind_rpc,
//             denominations,
//             password,
//         } => {
//             let key = get_key(password, dir_out_path.join(SALT_FILE));
//             let (pk_bytes, nonce) = encrypted_read(&key, dir_out_path.join(TLS_PK));
//             let (server, client) = if let Ok(v) = run_dkg(
//                 &dir_out_path,
//                 denominations,
//                 federation_name,
//                 certs,
//                 bitcoind_rpc,
//                 rustls::PrivateKey(pk_bytes),
//                 &mut task_group,
//             )
//             .await
//             {
//                 v
//             } else {
//                 info!("Canceled");
//                 return;
//             };

//             let server_path = dir_out_path.join(CONFIG_FILE);
//             let config_bytes = serde_json::to_string(&server).unwrap().into_bytes();
//             encrypted_write(config_bytes, &key, nonce, server_path);

//             let client_path: PathBuf = dir_out_path.join("client.json");
//             let client_file = fs::File::create(client_path).expect("Could not create cfg file");
//             serde_json::to_writer_pretty(client_file, &client).unwrap();
//         }
//         Command::VersionHash => {
//             println!("{}", env!("GIT_HASH"));
//         }
//     }
// }

pub async fn run_dkg(
    dir_out_path: &Path,
    denominations: Vec<Amount>,
    federation_name: String,
    certs: Vec<String>,
    bitcoind_rpc: String,
    pk: rustls::PrivateKey,
    task_group: &mut TaskGroup,
) -> Cancellable<(ServerConfig, ClientConfig)> {
    let peers: BTreeMap<PeerId, PeerServerParams> = certs
        .into_iter()
        .sorted()
        .enumerate()
        .map(|(idx, cert)| (PeerId::from(idx as u16), parse_peer_params(cert)))
        .collect();

    let cert_string = fs::read_to_string(dir_out_path.join(TLS_CERT)).expect("Can't read file.");

    let our_params = parse_peer_params(cert_string);
    let our_id = peers
        .iter()
        .find(|(_peer, params)| params.cert == our_params.cert)
        .map(|(peer, _)| *peer)
        .expect("could not find our cert among peers");
    let params = ServerConfigParams::gen_params(
        pk,
        our_id,
        denominations,
        &peers,
        federation_name,
        bitcoind_rpc,
    );
    let param_map = HashMap::from([(our_id, params.clone())]);
    let peer_ids: Vec<PeerId> = peers.keys().cloned().collect();
    let mut server_conn =
        fedimint_server::config::connect(params.server_dkg, params.tls, task_group).await;
    let rng = OsRng;
    ServerConfig::distributed_gen(
        &mut server_conn,
        &our_id,
        &peer_ids,
        &param_map,
        rng,
        task_group,
    )
    .await
    .expect("failed to run DKG to generate configs")
}

fn parse_peer_params(url: String) -> PeerServerParams {
    tracing::info!("peer params: {:?}", url);
    let split: Vec<&str> = url.split(':').collect();
    assert_eq!(split.len(), 4, "Cannot parse cert string");
    let base_port = split[1].parse().expect("could not parse base port");
    let hex_cert = hex::decode(split[3]).expect("cert was not hex encoded");
    PeerServerParams {
        cert: rustls::Certificate(hex_cert),
        address: split[0].to_string(),
        base_port,
        name: split[2].to_string(),
    }
}

fn gen_tls(
    dir_out_path: &Path,
    address: String,
    base_port: u16,
    name: String,
    key: &LessSafeKey,
) -> String {
    let (cert, pk) = fedimint_server::config::gen_cert_and_key(&name).expect("TLS gen failed");
    encrypted_write(pk.0, key, zero_nonce(), dir_out_path.join(TLS_PK));

    tracing::info!("server name: {:?}", name);
    rustls::ServerName::try_from(name.as_str()).expect("Valid DNS name");
    // TODO Base64 encode name, hash fingerprint cert_string
    let cert_url = format!("{}:{}:{}:{}", address, base_port, name, hex::encode(cert.0));
    fs::write(dir_out_path.join(TLS_CERT), &cert_url).unwrap();
    cert_url
}
