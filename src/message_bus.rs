use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::session::TaskHook;

/// Capacity for inbound/outbound mpsc channels. Large enough to absorb bursts,
/// small enough to bound memory. If inbound fills, the channel's I/O loop
/// blocks on publish_inbound — it stops reading user input until space frees up.
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

/// The two mpsc receivers, handed off by ownership to their single consumers:
/// `inbound` → `AgentLoop`, `outbound` → `ChannelManager`.
pub struct MessageBusReceivers {
    pub inbound: mpsc::Receiver<BusRequest>,
    pub outbound: mpsc::Receiver<BusResult>,
}

/// MessageBus: shared send endpoints + owned-receiver handoff. No background
/// tasks. AgentLoop owns the inbound listener; ChannelManager owns the outbound
/// listener. Each receiver has exactly one consumer, so it is moved out of the
/// bus at construction rather than shared behind a lock.
pub struct MessageBus {
    inbound_tx: mpsc::Sender<BusRequest>,
    outbound_tx: mpsc::Sender<BusResult>,
}

impl MessageBus {
    /// Construct the shared bus and return the two receivers by ownership.
    /// Callers split the receivers along the construction chain: `inbound` to
    /// `AgentLoop`, `outbound` to `ChannelManager`.
    pub fn new() -> (Arc<MessageBus>, MessageBusReceivers) {
        let (inbound_tx, inbound_rx) = mpsc::channel::<BusRequest>(INBOUND_CAPACITY);
        let (outbound_tx, outbound_rx) = mpsc::channel::<BusResult>(OUTBOUND_CAPACITY);

        (
            Arc::new(Self {
                inbound_tx,
                outbound_tx,
            }),
            MessageBusReceivers {
                inbound: inbound_rx,
                outbound: outbound_rx,
            },
        )
    }

    /// Publish an inbound request (channels → AgentLoop). Ignores a closed
    /// channel (all consumers dropped). Use the bare `inbound_tx()` accessor
    /// only when a `'static` clone or `try_send` is required.
    pub async fn publish_inbound(&self, req: BusRequest) {
        let _ = self.inbound_tx.send(req).await;
    }

    /// Publish an outbound result (AgentLoop / tools → channels). Ignores a
    /// closed channel. Use the bare `outbound_tx()` accessor only when a
    /// `'static` clone is required (e.g. message tool callback).
    pub async fn publish_outbound(&self, res: BusResult) {
        let _ = self.outbound_tx.send(res).await;
    }

    /// Sender for channels to submit inbound requests.
    pub fn inbound_tx(&self) -> mpsc::Sender<BusRequest> {
        self.inbound_tx.clone()
    }

    /// Sender for AgentLoop / tools to publish results.
    pub fn outbound_tx(&self) -> mpsc::Sender<BusResult> {
        self.outbound_tx.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_new_returns_connected_senders_and_receivers() {
        let (bus, mut receivers) = MessageBus::new();

        bus.publish_inbound(BusRequest {
            session_id: "s1".to_string(),
            content: "hello".to_string(),
            channel_inject: None,
            hook: TaskHook::new("s1"),
        })
        .await;
        bus.publish_outbound(BusResult {
            session_id: "s1".to_string(),
            task_id: "t1".to_string(),
            content: "reply".to_string(),
        })
        .await;

        let inbound = receivers.inbound.recv().await.unwrap();
        assert_eq!(inbound.session_id, "s1");
        assert_eq!(inbound.content, "hello");

        let outbound = receivers.outbound.recv().await.unwrap();
        assert_eq!(outbound.content, "reply");
    }

    #[tokio::test]
    #[should_panic(expected = "outbound_rx already consumed")]
    async fn test_outbound_rx_single_consumer() {
        // Simulate the single-consumer contract: take the receiver from an
        // Option, then take again — the second take panics.
        let (_bus, receivers) = MessageBus::new();
        let mut opt = Some(receivers.outbound);
        let _first = opt
            .take()
            .expect("outbound_rx already consumed by a previous run/run_with_shutdown call");
        // Second take triggers the panic we assert on.
        let _second = opt
            .take()
            .expect("outbound_rx already consumed by a previous run/run_with_shutdown call");
    }
}
