use std::{collections::HashMap, sync::{atomic::AtomicU64, Arc}};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc::{Receiver, Sender}, oneshot, Mutex};
use tracing::{info, warn};

use crate::{core::ModuleInstanceId, encoding::{Decodable, Encodable}, PeerId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encodable, Decodable)]
pub(crate) struct P2pRpcMessage {
    module: Option<ModuleInstanceId>,
    message_id: u64,
    message: Vec<u8>,
}

type RpcReceiver = Receiver<(PeerId, P2pRpcMessage)>;
type RpcSender = Sender<(PeerId, P2pRpcMessage)>;
type ActiveCalls = Arc<Mutex<HashMap<u64, (oneshot::Sender<Vec<u8>>, PeerId)>>>;

pub struct RpcBroker {
    module: Option<ModuleInstanceId>,
    sender: RpcSender,
    next_message_id: AtomicU64,
    active_calls: ActiveCalls,
}

pub trait P2pRpcRequest: Encodable {
    type Response: Decodable;
}

impl RpcBroker {
    pub(crate) fn new(
        module: Option<ModuleInstanceId>,
        receiver: RpcReceiver,
        sender: RpcSender,
    ) -> RpcBroker {
        let next_message_id = AtomicU64::new(0);
        let active_calls = Arc::new(Mutex::new(HashMap::new()));
        
        tokio::spawn(Self::handle_messages(receiver, active_calls.clone()));
        
        RpcBroker {
            module,
            sender,
            next_message_id,
            active_calls,
        }
    }
    
    async fn handle_messages(module: Option<ModuleInstanceId>, receiver: RpcReceiver, active_calls: ActiveCalls) {
        while let Some((peer_id, message)) = receiver.recv().await {            
            assert_eq!(message.module, module, "Received message for wrong module");
            
            let mut active_calls_lock = active_calls.lock().await;
            let Some(expected_peer_id) = active_calls_lock.get(&message.message_id).map(|(_, id)| *id) else {
                warn!(%message.id, %peer_id, %expected_peer_id, "Received response to call from wrong peer");
                continue;
            };
            
            if (sender, _) = active_calls_lock.remove(&message.message_id).expect("Already verified before that entry exists") {
                let _ = sender.send(response);
            }
        }
        info!("RPC broker stopped");
    }
    
    pub fn call<R: P2pRpcRequest>(&self, )
}
