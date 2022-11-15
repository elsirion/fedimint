use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockWriteGuard};

use askama::Template;
use axum::extract::{Extension, Form};
use axum::response::Redirect;
use axum::{
    routing::{get, post},
    Router,
};
use axum_macros::debug_handler;
use fedimint_api::config::BitcoindRpcCfg;
use fedimint_api::task::TaskGroup;
use fedimint_api::Amount;
use fedimint_core::config::ClientConfig;
use fedimint_server::config::ServerConfig;
use http::StatusCode;
use mint_client::api::WsFederationConnect;
use qrcode_generator::QrCodeEcc;
use rand::rngs::OsRng;
use ring::aead::{LessSafeKey, Nonce};
use serde::Deserialize;
use tokio::sync::mpsc::Sender;
use tokio_rustls::rustls;

use crate::encrypt::{encrypted_read, encrypted_write, get_key, CONFIG_FILE, SALT_FILE, TLS_PK};
use crate::ui::configgen::configgen;
use crate::ui::distributedgen::{create_cert, run_dkg};
mod configgen;
mod distributedgen;

// fn run_fedimint(state: &mut RwLockWriteGuard<State>) {
//     let sender = state.sender.clone();
//     tokio::task::spawn(async move {
//         // Tell fedimintd that setup is complete
//         sender
//             .send(UiMessage::SetupComplete)
//             .await
//             .expect("failed to send over channel");
//     });
//     state.running = true;
// }

// fn run_dkg(state: &mut RwLockWriteGuard<State>, msg: RunDkgMessage) {
//     let sender = state.sender.clone();
//     tokio::task::spawn(async move {
//         // Tell fedimintd that setup is complete
//         sender.send(msg).await.expect("failed to send over channel");
//     });
// }

#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)]
pub struct Guardian {
    name: String,
    config_string: String,
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomeTemplate {
    federation_name: String,
    running: bool,
    federation_connection_string: String,
}

async fn home_page(Extension(state): Extension<MutableState>) -> HomeTemplate {
    let state = state.read().unwrap();
    let federation_connection_string = match state.client_config.clone() {
        Some(client_config) => {
            let connect_info = WsFederationConnect::from(&client_config);
            serde_json::to_string(&connect_info).unwrap()
        }
        None => "".into(),
    };

    HomeTemplate {
        federation_name: state.federation_name.clone(),
        running: state.running,
        federation_connection_string,
    }
}

#[derive(Template)]
#[template(path = "add_guardians.html")]
struct AddGuardiansTemplate {
    federation_name: String,
    guardians: Vec<Guardian>,
}

async fn add_guardians_page(Extension(state): Extension<MutableState>) -> AddGuardiansTemplate {
    let state = state.read().unwrap();
    AddGuardiansTemplate {
        federation_name: state.federation_name.clone(),
        guardians: state.guardians.clone(),
    }
}

fn parse_name_from_connection_string(connection_string: &String) -> String {
    let parts = connection_string.split(":").collect::<Vec<&str>>();
    parts[2].to_string()
}

fn parse_cert_from_connection_string(connection_string: &String) -> String {
    let parts = connection_string.split(":").collect::<Vec<&str>>();
    parts[3].to_string()
}

#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)]
pub struct GuardiansForm {
    connection_strings: String,
}

#[debug_handler]
async fn post_guardians(
    Extension(state): Extension<MutableState>,
    Form(form): Form<GuardiansForm>,
) -> Result<Redirect, (StatusCode, String)> {
    let connection_strings: Vec<String> =
        serde_json::from_str(&form.connection_strings).expect("not json");
    {
        let mut state = state.write().unwrap();
        let mut guardians = state.guardians.clone();
        for (i, connection_string) in connection_strings.clone().into_iter().enumerate() {
            guardians[i] = Guardian {
                name: parse_name_from_connection_string(&connection_string),
                config_string: connection_string,
            };
        }
        state.guardians = guardians;
    };
    let msg = {
        let state = state.read().unwrap();

        //
        // Actually run DKG
        //

        // let certs = connection_strings
        //     .iter()
        //     .map(|s| parse_cert_from_connection_string(s))
        //     .collect();
        let key = get_key(
            state.password.clone().unwrap(),
            state.cfg_path.join(SALT_FILE),
        );
        let (pk_bytes, nonce) = encrypted_read(&key, state.cfg_path.join(TLS_PK));
        let denominations = (1..12)
            .map(|amount| Amount::from_sat(10 * amount))
            .collect();
        let bitcoind_rpc = "127.0.0.118443".into();
        let mut task_group = TaskGroup::new();
        tracing::info!("running dkg");
        let msg = RunDkgMessage {
            dir_out_path: state.cfg_path.clone(),
            denominations,
            federation_name: state.federation_name.clone(),
            certs: connection_strings,
            bitcoind_rpc,
            pk: rustls::PrivateKey(pk_bytes),
            task_group,
            nonce,
            key,
        };
        // .await
        // {
        //     tracing::info!("DKG succeeded");
        //     v
        // } else {
        //     tracing::info!("Canceled");
        //     return Ok(Redirect::to("/post_guardisn".parse().unwrap()));
        // };
        msg
    };
    // tokio::task::spawn(async move {
    //     // Tell fedimintd that setup is complete
    //     sender.send(msg).await.expect("failed to send over channel");
    // })
    // .await
    // .expect("couldn't send over channel");

    // tokio::task::spawn(async move {
    // let (send, recv) = tokio::sync::oneshot::channel();
    let handle = tokio::runtime::Handle::current();

    let (sender, receive) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        // futures::executor::block_on(async move {
        tracing::info!("=dkg");
        handle.block_on(async move {
            let mut task_group = TaskGroup::new();
            match run_dkg(
                &msg.dir_out_path,
                msg.denominations,
                msg.federation_name,
                msg.certs,
                msg.bitcoind_rpc,
                msg.pk,
                &mut task_group,
            )
            .await
            {
                Ok((server, client)) => {
                    tracing::info!("DKG succeeded");
                    let server_path = msg.dir_out_path.join(CONFIG_FILE);
                    let config_bytes = serde_json::to_string(&server).unwrap().into_bytes();
                    encrypted_write(config_bytes, &msg.key, msg.nonce, server_path);

                    let client_path: PathBuf = msg.dir_out_path.join("client.json");
                    let client_file =
                        std::fs::File::create(client_path).expect("Could not create cfg file");
                    serde_json::to_writer_pretty(client_file, &client).unwrap();
                    sender.send("/confirm").unwrap();
                }
                Err(e) => {
                    tracing::info!("Canceled {:?}", e);
                    sender.send("/post_guardians").unwrap();
                }
            };
        });
    });
    let url = receive.blocking_recv().unwrap();
    Ok(Redirect::to(url.parse().unwrap()))
}

// #[derive(Template)] #[template(path = "confirm.html")]
// struct ConfirmTemplate {
//     federation_name: String,
//     guardians: Vec<Guardian>,
// }

// async fn confirm_page(Extension(state): Extension<MutableState>) -> ConfirmTemplate {
//     let state = state.read().unwrap();

//     ConfirmTemplate {
//         federation_name: state.federation_name.clone(),
//         guardians: state.guardians.clone(),
//     }
// }

#[derive(Template)]
#[template(path = "params.html")]
struct UrlConnection {}

async fn params_page(Extension(_state): Extension<MutableState>) -> UrlConnection {
    UrlConnection {}
}

#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)]
pub struct ParamsForm {
    guardian_name: String,
    federation_name: String,
    ip_addr: String,
    bitcoin_rpc: String,
    password: String,
    guardians_count: u32,
}

#[debug_handler]
async fn post_federation_params(
    Extension(state): Extension<MutableState>,
    Form(form): Form<ParamsForm>,
) -> Result<Redirect, (StatusCode, String)> {
    let mut state = state.write().unwrap();

    let port = portpicker::pick_unused_port().expect("No ports free");

    let config_string = create_cert(
        state.cfg_path.clone(),
        form.ip_addr,
        form.guardian_name.clone(),
        form.password.clone(),
        port,
    );

    let mut guardians = vec![Guardian {
        name: form.guardian_name,
        config_string,
    }];

    for i in 1..form.guardians_count {
        guardians.push(Guardian {
            name: format!("Guardian-{}", i + 1),
            config_string: "".into(),
        });
    }
    // update state
    state.guardians = guardians;
    state.federation_name = form.federation_name;
    state.password = Some(form.password);

    Ok(Redirect::to("/add_guardians".parse().unwrap()))
}

async fn qr(Extension(state): Extension<MutableState>) -> impl axum::response::IntoResponse {
    let client_config = state.read().unwrap().client_config.clone().unwrap();
    let connect_info = WsFederationConnect::from(&client_config);
    let string = serde_json::to_string(&connect_info).unwrap();
    let png_bytes: Vec<u8> = qrcode_generator::to_png_to_vec(string, QrCodeEcc::Low, 1024).unwrap();
    (
        axum::response::Headers([(axum::http::header::CONTENT_TYPE, "image/png")]),
        png_bytes,
    )
}

struct State {
    federation_name: String,
    guardians: Vec<Guardian>,
    running: bool,
    cfg_path: PathBuf,
    config_string: String,
    sender: Sender<UiMessage>,
    server_configs: Option<Vec<(Guardian, ServerConfig)>>,
    client_config: Option<ClientConfig>,
    password: Option<String>,
    btc_rpc: Option<String>,
}
type MutableState = Arc<RwLock<State>>;

pub struct RunDkgMessage {
    dir_out_path: PathBuf,
    denominations: Vec<Amount>,
    federation_name: String,
    certs: Vec<String>,
    bitcoind_rpc: String,
    pk: rustls::PrivateKey,
    task_group: TaskGroup,
    nonce: Nonce,
    key: LessSafeKey,
}

// #[derive(Debug)]
pub enum UiMessage {
    SetupComplete,
    // RunDkg(RunDkgMessage),
}

pub async fn run_ui(cfg_path: PathBuf, sender: Sender<UiMessage>, port: u32) {
    let mut rng = OsRng;
    let secp = bitcoin::secp256k1::Secp256k1::new();
    let config_string = "".to_string();
    let guardians = vec![Guardian {
        config_string: config_string.clone(),
        name: "You".into(),
    }];

    // Default federation name
    let federation_name = "Cypherpunk".into();

    let state = Arc::new(RwLock::new(State {
        federation_name,
        guardians,
        running: false,
        cfg_path,
        config_string,
        sender,
        server_configs: None,
        client_config: None,
        btc_rpc: None,
        password: None,
    }));

    let app = Router::new()
        .route("/", get(home_page))
        .route("/federation_params", get(params_page))
        .route("/post_federation_params", post(post_federation_params))
        .route("/add_guardians", get(add_guardians_page))
        .route("/post_guardians", post(post_guardians))
        // .route("/confirm", get(confirm_page))
        // .route("/distributed_key_gen", post(distributed_key_gen))
        .route("/qr", get(qr))
        .layer(Extension(state));

    let bind_addr: SocketAddr = format!("0.0.0.0:{}", port).parse().unwrap();
    axum::Server::bind(&bind_addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}
