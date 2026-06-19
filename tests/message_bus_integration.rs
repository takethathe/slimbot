use slimbot::{BusRequest, BusResult, MessageBus, TaskHook};

// ── MessageBus creation and endpoints ──

#[tokio::test]
async fn test_message_bus_new_has_all_endpoints() {
    let (bus, receivers) = MessageBus::new();
    let inbound_tx = bus.inbound_tx();
    let outbound_tx = bus.outbound_tx();

    // Senders should have positive capacity
    assert!(inbound_tx.capacity() > 0);
    assert!(outbound_tx.capacity() > 0);
    // Receivers are owned directly (no Arc<Mutex> wrapping) — verify they exist.
    let _ = receivers.inbound;
    let _ = receivers.outbound;
}

// ── Inbound message flow ──

#[tokio::test]
async fn test_inbound_request_flow() {
    let (bus, receivers) = MessageBus::new();
    let tx = bus.inbound_tx();
    let mut rx = receivers.inbound;

    let request = BusRequest {
        session_id: "test:chat1".to_string(),
        content: "hello".to_string(),
        channel_inject: None,
        hook: TaskHook::new("test:chat1"),
    };

    tx.send(request).await.unwrap();

    // Receive on the other end — no lock, owned receiver
    let received = rx.recv().await.unwrap();
    assert_eq!(received.session_id, "test:chat1");
    assert_eq!(received.content, "hello");
    assert!(received.channel_inject.is_none());
}

// ── Outbound result flow ──

#[tokio::test]
async fn test_outbound_result_flow() {
    let (bus, receivers) = MessageBus::new();
    let tx = bus.outbound_tx();
    let mut rx = receivers.outbound;

    let result = BusResult {
        session_id: "test:chat1".to_string(),
        task_id: "task-123".to_string(),
        content: "final answer".to_string(),
    };

    tx.send(result).await.unwrap();

    let received = rx.recv().await.unwrap();
    assert_eq!(received.session_id, "test:chat1");
    assert_eq!(received.task_id, "task-123");
    assert_eq!(received.content, "final answer");
}

// ── Channel capacity and backpressure ──

#[tokio::test]
async fn test_inbound_channel_fills_up() {
    let (bus, _receivers) = MessageBus::new();
    let tx = bus.inbound_tx();

    // Fill up the channel (capacity is 32)
    for i in 0..32 {
        let request = BusRequest {
            session_id: format!("test:{}", i),
            content: format!("msg {}", i),
            channel_inject: None,
            hook: TaskHook::new("test:chat1"),
        };
        // Use try_send to avoid blocking
        let result = tx.try_send(request);
        assert!(result.is_ok(), "should accept up to capacity");
    }

    // Next try_send should fail (channel full)
    let request = BusRequest {
        session_id: "overflow".to_string(),
        content: "overflow".to_string(),
        channel_inject: None,
        hook: TaskHook::new("test:chat1"),
    };
    let result = tx.try_send(request);
    assert!(result.is_err(), "should reject when full");
}

// ── Concurrent send/recv ──

#[tokio::test]
async fn test_concurrent_inbound_outbound() {
    let (bus, receivers) = MessageBus::new();
    let in_tx = bus.inbound_tx();
    let mut in_rx = receivers.inbound;
    let out_tx = bus.outbound_tx();
    let mut out_rx = receivers.outbound;

    // Spawn a task that receives inbound and sends outbound
    let producer = tokio::spawn(async move {
        let req = in_rx.recv().await.unwrap();
        let result = BusResult {
            session_id: req.session_id,
            task_id: "task-1".to_string(),
            content: format!("processed: {}", req.content),
        };
        out_tx.send(result).await.unwrap();
    });

    // Send a request
    in_tx
        .send(BusRequest {
            session_id: "test:1".to_string(),
            content: "do something".to_string(),
            channel_inject: None,
            hook: TaskHook::new("test:1"),
        })
        .await
        .unwrap();

    // Receive the result — no lock, owned receiver
    let result = out_rx.recv().await.unwrap();
    assert_eq!(result.content, "processed: do something");

    producer.await.unwrap();
}

// ── BusResult serialization ──

#[test]
fn test_bus_result_serde_round_trip() {
    let result = BusResult {
        session_id: "cli:abc".to_string(),
        task_id: "task-42".to_string(),
        content: "some result".to_string(),
    };

    let json = serde_json::to_string(&result).unwrap();
    let deserialized: BusResult = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.session_id, "cli:abc");
    assert_eq!(deserialized.task_id, "task-42");
    assert_eq!(deserialized.content, "some result");
}
