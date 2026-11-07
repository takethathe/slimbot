//! Feishu channel integration (ticket 27): registration through the existing
//! channel wiring, config validation, and the enabled gate. The channel-side
//! decision logic (owner binding, group-off, routing, round trip) is covered
//! by unit tests in `src/channel/feishu.rs`; this file covers what is only
//! visible from the public API: the factory, `ChannelManager::init`, and the
//! guarantee that creating a channel performs no network I/O.

use std::collections::HashMap;
use std::sync::Arc;

use slimbot::{
    ChannelConfig, ChannelFactory, ChannelManager, Config, FeishuChannelFactory, MessageBus,
};

fn feishu_extra(pairs: &[(&str, &str)]) -> HashMap<String, serde_json::Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
        .collect()
}

fn config_with_feishu(enabled: bool, pairs: &[(&str, &str)]) -> Config {
    let mut config = Config::default();
    config.channels.insert(
        "feishu".to_string(),
        ChannelConfig {
            enabled,
            extra: feishu_extra(pairs),
        },
    );
    config
}

fn manager_for(config: Config) -> ChannelManager {
    let (mb, receivers) = MessageBus::new();
    let outbound_rx = receivers.outbound;
    let mut cm = ChannelManager::new(mb.clone(), outbound_rx, Arc::new(config), None);
    cm.register_factory(
        "feishu",
        Box::new(FeishuChannelFactory::new(
            mb.clone(),
            tokio::sync::broadcast::channel(1).0,
            std::env::temp_dir(),
        )),
    );
    cm
}

const VALID: &[(&str, &str)] = &[
    ("app_id", "cli_xxx"),
    ("app_secret", "secret-value"),
    ("owner_open_id", "ou_owner"),
];

// ── the factory through the public API ──

#[test]
fn test_factory_create_yields_a_feishu_channel_with_two_segment_session() {
    let (mb, _rx) = MessageBus::new();
    let factory = FeishuChannelFactory::new(
        mb,
        tokio::sync::broadcast::channel(1).0,
        std::env::temp_dir(),
    );
    let channel = factory
        .create(&serde_json::json!({
            "app_id": "cli_xxx",
            "app_secret": "secret-value",
            "owner_open_id": "ou_owner",
        }))
        .expect("a complete config creates the channel");
    assert_eq!(channel.id(), "feishu");
    assert_eq!(channel.name(), "Feishu");
    // Two-segment session id, one per conversation; the placeholder chat is
    // replaced by each real session id at routing time.
    assert_eq!(channel.session_id(), "feishu:default");
}

#[test]
fn test_factory_error_names_the_missing_field_and_never_leaks_the_app_secret() {
    let (mb, _rx) = MessageBus::new();
    let factory = FeishuChannelFactory::new(
        mb,
        tokio::sync::broadcast::channel(1).0,
        std::env::temp_dir(),
    );
    // Owner binding missing while a real-looking secret is present: the error
    // must point at the field, not echo the credential.
    let outcome = factory.create(&serde_json::json!({
        "app_id": "cli_xxx",
        "app_secret": "ZZsecretMUST-NOT-APPEAR",
    }));
    let err = match outcome {
        Ok(_) => panic!("an unbound owner must not create a channel"),
        Err(e) => e,
    };
    let text = err.to_string();
    assert!(
        text.contains("owner_open_id"),
        "error names the field: {text}"
    );
    assert!(
        !text.contains("ZZsecretMUST-NOT-APPEAR"),
        "the app secret never appears in an error"
    );
}

#[test]
fn test_factory_create_performs_no_network_io_even_with_placeholder_credentials() {
    // Creation only builds config and client objects; nothing here can reach
    // the network, so `create` succeeding offline is the assertion. The long
    // connection is opened only by start (single holder), never by init.
    let (mb, _rx) = MessageBus::new();
    let factory = FeishuChannelFactory::new(
        mb,
        tokio::sync::broadcast::channel(1).0,
        std::env::temp_dir(),
    );
    assert!(
        factory
            .create(&serde_json::json!({
                "app_id": "cli_xxx",
                "app_secret": "not-a-real-secret",
                "owner_open_id": "ou_owner",
            }))
            .is_ok()
    );
}

// ── registration through the existing channel wiring ──

#[tokio::test]
async fn test_channel_manager_init_accepts_an_enabled_feishu_entry() {
    let mut cm = manager_for(config_with_feishu(true, VALID));
    // No error: the entry passed validation and the channel was registered
    // for outbound routing. init creates; it does not connect.
    cm.init().await.expect("init registers the channel");
}

#[tokio::test]
async fn test_channel_manager_init_skips_a_disabled_entry_without_validating_it() {
    // A disabled channel must not block gateway startup, even with a config
    // the factory would reject.
    let mut cm = manager_for(config_with_feishu(false, &[("app_id", "cli_xxx")]));
    cm.init().await.expect("a disabled entry is skipped");
}

#[tokio::test]
async fn test_channel_manager_init_fails_on_an_unregistered_channel_type() {
    // The gate is registration, not a built-in list: without the factory the
    // entry is an explicit configuration error.
    let (mb, receivers) = MessageBus::new();
    let mut cm = ChannelManager::new(
        mb,
        receivers.outbound,
        Arc::new(config_with_feishu(true, VALID)),
        None,
    );
    // Deliberately do not register the factory.
    let err = cm.init().await.unwrap_err();
    assert!(err.to_string().contains("Unregistered channel type"));
}
