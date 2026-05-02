use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::session::TaskHook;

/// Capacity for inbound/outbound mpsc channels. Large enough to absorb bursts,
/// small enough to bound memory. If inbound fills, the channel's I/O loop
/// blocks on send_inbound — it stops reading user input until space frees up.
const INBOUND_CAPACITY: usize = 32;
const OUTBOUND_CAPACITY: usize = 32;

pub struct BusRequest {
    pub session_id: String,
    pub content: String,
    pub channel_inject: Option<String>,
    pub hook: TaskHook,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusResult {
    pub session_id: String,
    pub task_id: String,
    pub content: String,
}

/// MessageBus: pure async channel endpoints. No background tasks.
/// AgentLoop owns the inbound listener; ChannelManager owns the outbound listener.
pub struct MessageBus {
    inbound_tx: mpsc::Sender<BusRequest>,
    inbound_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<BusRequest>>>,
    outbound_tx: mpsc::Sender<BusResult>,
    outbound_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<BusResult>>>,
}

impl MessageBus {
    pub fn new() -> Self {
        let (inbound_tx, inbound_rx) = mpsc::channel::<BusRequest>(INBOUND_CAPACITY);
        let (outbound_tx, outbound_rx) = mpsc::channel::<BusResult>(OUTBOUND_CAPACITY);

        Self {
            inbound_tx,
            inbound_rx: Arc::new(tokio::sync::Mutex::new(inbound_rx)),
            outbound_tx,
            outbound_rx: Arc::new(tokio::sync::Mutex::new(outbound_rx)),
        }
    }

    /// Sender for channels to submit inbound requests
    pub fn inbound_tx(&self) -> mpsc::Sender<BusRequest> {
        self.inbound_tx.clone()
    }

    /// Receiver for AgentLoop to listen on for inbound requests
    pub fn inbound_rx(&self) -> Arc<tokio::sync::Mutex<mpsc::Receiver<BusRequest>>> {
        self.inbound_rx.clone()
    }

    /// Sender for AgentLoop to publish results
    pub fn outbound_tx(&self) -> mpsc::Sender<BusResult> {
        self.outbound_tx.clone()
    }

    /// Receiver for ChannelManager to listen on for outbound results
    pub fn outbound_rx(&self) -> Arc<tokio::sync::Mutex<mpsc::Receiver<BusResult>>> {
        self.outbound_rx.clone()
    }
}
