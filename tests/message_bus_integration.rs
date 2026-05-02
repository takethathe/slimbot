use slimbot::{MessageBus, BusRequest, BusResult, TaskHook};

// ── MessageBus creation and endpoints ──

#[tokio::test]
async fn test_message_bus_new_has_all_endpoints() {
    let bus = MessageBus::new();
    let inbound_tx = bus.inbound_tx();
    let inbound_rx = bus.inbound_rx();
    let outbound_tx = bus.outbound_tx();
    let outbound_rx = bus.outbound_rx();

    // All endpoints should be usable
    assert!(inbound_tx.capacity() > 0);
    assert!(outbound_tx.capacity() > 0);
    // rx endpoints are wrapped in Arc<Mutex> so we can't directly check,
    // but we can verify they exist and can be cloned
    let _ = inbound_rx.clone();
    let _ = outbound_rx.clone();
}

// ── Inbound message flow ──

#[tokio::test]
async fn test_inbound_request_flow() {
    let bus = MessageBus::new();
    let tx = bus.inbound_tx();
    let rx = bus.inbound_rx();

    let request = BusRequest {
        session_id: "test:chat1".to_string(),
        content: "hello".to_string(),
        channel_inject: None,
        hook: TaskHook::new("test:chat1"),
    };

    tx.send(request).await.unwrap();

    // Receive on the other end
    let mut guard = rx.lock().await;
    let received = guard.recv().await.unwrap();
    assert_eq!(received.session_id, "test:chat1");
    assert_eq!(received.content, "hello");
    assert!(received.channel_inject.is_none());
}

// ── Outbound result flow ──

#[tokio::test]
async fn test_outbound_result_flow() {
    let bus = MessageBus::new();
    let tx = bus.outbound_tx();
    let rx = bus.outbound_rx();

    let result = BusResult {
        session_id: "test:chat1".to_string(),
        task_id: "task-123".to_string(),
        content: "final answer".to_string(),
    };

    tx.send(result).await.unwrap();

    let mut guard = rx.lock().await;
    let received = guard.recv().await.unwrap();
    assert_eq!(received.session_id, "test:chat1");
    assert_eq!(received.task_id, "task-123");
    assert_eq!(received.content, "final answer");
}

// ── Channel capacity and backpressure ──

#[tokio::test]
async fn test_inbound_channel_fills_up() {
    let bus = MessageBus::new();
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
    let bus = MessageBus::new();
    let in_tx = bus.inbound_tx();
    let in_rx = bus.inbound_rx();
    let out_tx = bus.outbound_tx();
    let out_rx = bus.outbound_rx();

    // Spawn a task that receives inbound and sends outbound
    let producer = tokio::spawn(async move {
        let mut guard = in_rx.lock().await;
        let req = guard.recv().await.unwrap();
        let result = BusResult {
            session_id: req.session_id,
            task_id: "task-1".to_string(),
            content: format!("processed: {}", req.content),
        };
        drop(guard);
        out_tx.send(result).await.unwrap();
    });

    // Send a request
    in_tx.send(BusRequest {
        session_id: "test:1".to_string(),
        content: "do something".to_string(),
        channel_inject: None,
        hook: TaskHook::new("test:1"),
    }).await.unwrap();

    // Receive the result
    let mut guard = out_rx.lock().await;
    let result = guard.recv().await.unwrap();
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
