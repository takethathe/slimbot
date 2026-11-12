use std::collections::HashSet;

use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::session::{Message, MessageMeta, TurnKind};
use crate::tool::ToolCall;

fn record(id: &str, tool: &str, created_ms: u64) -> PendingApproval {
    PendingApproval {
        approval_id: id.to_string(),
        owner_id: "owner".into(),
        session_id: "s1".into(),
        run_id: "run".into(),
        turn_kind: TurnKind::User,
        tool: tool.to_string(),
        args: serde_json::json!({"command": "ls"}),
        args_hash: args_hash(&serde_json::json!({"command": "ls"})),
        risk: RiskLevel::Exec,
        created_ms,
    }
}

fn store_with_clock(start_ms: u64) -> (Arc<ApprovalStore>, Arc<AtomicU64>) {
    let now = Arc::new(AtomicU64::new(start_ms));
    let clock_now = now.clone();
    let store = Arc::new(ApprovalStore::with_clock(
        ApprovalConfig::default(),
        Arc::new(move || clock_now.load(Ordering::SeqCst)),
    ));
    (store, now)
}

#[test]
fn test_approval_config_default_exec_and_external_send_are_ask() {
    let config = ApprovalConfig::default();
    assert_eq!(
        config.control_for("shell", RiskLevel::Exec),
        ControlLevel::Ask
    );
    assert_eq!(
        config.control_for("message", RiskLevel::ExternalSend),
        ControlLevel::Ask
    );
    assert_eq!(
        config.control_for("file_reader", RiskLevel::ReadOnly),
        ControlLevel::Allow
    );
    assert_eq!(
        config.control_for("file_writer", RiskLevel::WorkspaceWrite),
        ControlLevel::Allow
    );
    assert_eq!(
        config.control_for("web_fetch", RiskLevel::NetworkEgress),
        ControlLevel::Allow
    );
    assert_eq!(config.timeout_seconds, DEFAULT_APPROVAL_TIMEOUT_SECONDS);
}

#[test]
fn test_approval_config_per_tool_override_beats_level() {
    let mut config = ApprovalConfig::default();
    config.tools.insert("shell".into(), ControlLevel::Deny);
    config.tools.insert("message".into(), ControlLevel::Allow);
    assert_eq!(
        config.control_for("shell", RiskLevel::Exec),
        ControlLevel::Deny
    );
    assert_eq!(
        config.control_for("message", RiskLevel::ExternalSend),
        ControlLevel::Allow
    );
}

#[test]
fn test_approval_config_timeout_is_clamped() {
    let mut config = ApprovalConfig {
        timeout_seconds: 0,
        ..Default::default()
    };
    config.clamp();
    assert_eq!(config.timeout_seconds, DEFAULT_APPROVAL_TIMEOUT_SECONDS);

    let mut config = ApprovalConfig {
        timeout_seconds: MAX_APPROVAL_TIMEOUT_SECONDS + 10,
        ..Default::default()
    };
    config.clamp();
    assert_eq!(config.timeout_seconds, MAX_APPROVAL_TIMEOUT_SECONDS);
}

#[test]
fn test_store_begin_turn_expires_records_past_ttl() {
    let (store, now) = store_with_clock(1_000);
    store.propose(record("a", "shell", 1_000));
    now.store(
        1_000 + DEFAULT_APPROVAL_TIMEOUT_SECONDS * 1000 + 1,
        Ordering::SeqCst,
    );

    let expired = store.begin_turn("s1");
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].approval_id, "a");
    assert!(store.pending_for("s1").is_empty());
}

#[test]
fn test_store_end_turn_clears_unredeemed_carried_records() {
    let (store, _now) = store_with_clock(1_000);
    store.propose(record("a", "shell", 1_000));

    store.begin_turn("s1");
    let cleared = store.end_turn("s1");
    assert_eq!(cleared.len(), 1);
    assert_eq!(cleared[0].approval_id, "a");
    assert!(!store.has_pending("s1"));
}

#[test]
fn test_store_end_turn_keeps_record_proposed_in_same_turn() {
    let (store, _now) = store_with_clock(1_000);
    store.begin_turn("s1");
    store.propose(record("a", "shell", 1_000));

    assert!(store.end_turn("s1").is_empty());
    assert_eq!(store.pending_for("s1").len(), 1);

    // Next turn it is carried, and unredeemed means cleared.
    store.begin_turn("s1");
    assert_eq!(store.end_turn("s1").len(), 1);
    assert!(store.pending_for("s1").is_empty());
}

#[test]
fn test_store_matching_requires_tool_and_args_hash() {
    let (store, _now) = store_with_clock(1_000);
    let mut rec = record("a", "shell", 1_000);
    rec.args_hash = args_hash(&serde_json::json!({"command": "ls"}));
    store.propose(rec);
    // Only a record carried into this turn is redeemable.
    store.begin_turn("s1");

    assert!(
        store
            .matching(
                "s1",
                "shell",
                &args_hash(&serde_json::json!({"command": "ls"}))
            )
            .is_some()
    );
    assert!(
        store
            .matching(
                "s1",
                "shell",
                &args_hash(&serde_json::json!({"command": "rm"}))
            )
            .is_none()
    );
    assert!(
        store
            .matching(
                "s1",
                "message",
                &args_hash(&serde_json::json!({"command": "ls"}))
            )
            .is_none()
    );
}

#[test]
fn test_store_same_turn_proposal_is_not_redeemable() {
    // The approval-authority bug: a proposal made during this turn must not be
    // redeemed by a replay inside the same turn, or a batch of two identical
    // calls (or a script calling twice) would approve itself.
    let (store, _now) = store_with_clock(1_000);
    store.begin_turn("s1");
    let rec = record("a", "shell", 1_000);
    let hash = rec.args_hash.clone();
    store.propose(rec);

    assert!(store.matching("s1", "shell", &hash).is_none());
    assert!(store.mismatching("s1", "shell", "other").is_empty());

    // Next turn the same record is redeemable.
    store.begin_turn("s1");
    assert!(store.matching("s1", "shell", &hash).is_some());
}

#[test]
fn test_store_propose_dedupes_same_tool_and_args() {
    let (store, _now) = store_with_clock(1_000);
    store.propose(record("a", "shell", 1_000));
    let second = store.propose(record("b", "shell", 1_001));
    assert_eq!(second.approval_id, "a");
    assert_eq!(store.pending_for("s1").len(), 1);
}

#[test]
fn test_args_hash_is_stable_and_args_sensitive() {
    let a = serde_json::json!({"a": 1, "b": [true, null]});
    assert_eq!(args_hash(&a), args_hash(&a.clone()));
    assert_ne!(args_hash(&a), args_hash(&serde_json::json!({"a": 1})));
}

#[test]
fn test_format_pending_list_is_plain_text_with_args() {
    let text = format_pending_list(&[record("a", "shell", 1_000)]);
    assert!(text.contains("Pending approval"));
    assert!(text.contains("shell"));
    assert!(text.contains("exec"));
    assert!(text.contains("\"command\":\"ls\""));
    assert!(text.contains("approval: a"));
    assert!(!text.contains("button"));
}

#[test]
fn test_merge_approval_rounds_folds_redeemed_replay_onto_original_call() {
    let merged = merge_approval_rounds(&proposal_round("file list"), &HashSet::new());

    // user, assistant(call), tool(real result) — one completed round.
    assert_eq!(merged.len(), 3);
    match &merged[2] {
        Message::Tool {
            content,
            tool_call_id,
            meta,
            ..
        } => {
            assert_eq!(content, "file list");
            assert_eq!(tool_call_id, "call-1");
            assert!(
                meta.approval_id.is_none(),
                "the folded result is a normal result again"
            );
        }
        other => panic!("expected tool result, got {other:?}"),
    }
}

#[test]
fn test_merge_approval_rounds_keeps_live_guidance_visible() {
    let messages = one_round("call-1", "NOT EXECUTED");
    let live: HashSet<String> = ["a".to_string()].into_iter().collect();
    let merged = merge_approval_rounds(&messages, &live);

    assert_eq!(merged.len(), messages.len());
    assert!(merged[2].approval_id().is_some());
    assert!(matches!(&merged[2], Message::Tool { content, .. } if content == "NOT EXECUTED"));
}

#[test]
fn test_merge_approval_rounds_rewrites_unredeemed_guidance_to_failure() {
    let messages = one_round("call-1", "NOT EXECUTED");
    let merged = merge_approval_rounds(&messages, &HashSet::new());

    // Nothing is deleted: every tool_call still has a result.
    assert_eq!(merged.len(), messages.len());
    match &merged[2] {
        Message::Tool {
            meta,
            content,
            tool_call_id,
            ..
        } => {
            assert_eq!(tool_call_id, "call-1", "the pairing survives the rewrite");
            assert!(meta.approval_id.is_none(), "the marker is consumed");
            assert!(content.contains("not redeemed"), "got: {content}");
            assert!(content.contains("Nothing ran"), "got: {content}");
        }
        other => panic!("expected tool result, got {other:?}"),
    }
}

#[test]
fn test_merge_approval_rounds_keeps_mismatched_replay_as_own_round() {
    // The owner approved `shell ls`; the model replayed `shell rm` instead.
    let messages = vec![
        Message::user("list files".into()),
        Message::Assistant {
            meta: MessageMeta::default(),
            content: None,
            tool_calls: Some(vec![call("call-1", "shell", json!({"command": "ls"}))]),
            reasoning_content: None,
            thinking_blocks: None,
        },
        Message::tool_marked(
            "NOT EXECUTED".into(),
            "call-1".into(),
            Some("shell".into()),
            "a".into(),
        ),
        Message::user("确认".into()),
        Message::Assistant {
            meta: MessageMeta::default(),
            content: None,
            tool_calls: Some(vec![call("call-2", "shell", json!({"command": "rm"}))]),
            reasoning_content: None,
            thinking_blocks: None,
        },
        Message::tool("DENIED".into(), "call-2".into(), Some("shell".into())),
    ];

    let merged = merge_approval_rounds(&messages, &HashSet::new());

    // The mismatched replay keeps its own call/result pair; the proposal is
    // rewritten to a failure, so both tool_calls have a real result.
    assert_eq!(merged.len(), messages.len());
    assert!(
        matches!(&merged[2], Message::Tool { content, .. } if content.contains("not redeemed"))
    );
    assert!(matches!(&merged[5], Message::Tool { content, .. } if content == "DENIED"));
}

#[test]
fn test_merge_approval_rounds_folds_multiple_approvals_sharing_one_replay() {
    let messages = vec![
        Message::user("do two things".into()),
        Message::Assistant {
            meta: MessageMeta::default(),
            content: None,
            tool_calls: Some(vec![
                call("call-1", "shell", json!({"command": "ls"})),
                call("call-2", "message", json!({"text": "hi"})),
            ]),
            reasoning_content: None,
            thinking_blocks: None,
        },
        Message::tool_marked(
            "NOT EXECUTED 1".into(),
            "call-1".into(),
            Some("shell".into()),
            "a".into(),
        ),
        Message::tool_marked(
            "NOT EXECUTED 2".into(),
            "call-2".into(),
            Some("message".into()),
            "b".into(),
        ),
        Message::user("确认".into()),
        Message::Assistant {
            meta: MessageMeta::default(),
            content: None,
            tool_calls: Some(vec![
                call("call-3", "shell", json!({"command": "ls"})),
                call("call-4", "message", json!({"text": "hi"})),
            ]),
            reasoning_content: None,
            thinking_blocks: None,
        },
        Message::tool("out 1".into(), "call-3".into(), Some("shell".into())),
        Message::tool("out 2".into(), "call-4".into(), Some("message".into())),
    ];

    let merged = merge_approval_rounds(&messages, &HashSet::new());

    // user, assistant(original two calls), two real results.
    assert_eq!(merged.len(), 4, "got {merged:#?}");
    assert!(matches!(&merged[2], Message::Tool { content, .. } if content == "out 1"));
    assert!(matches!(&merged[3], Message::Tool { content, .. } if content == "out 2"));
}

#[test]
fn test_merge_approval_rounds_is_idempotent() {
    let once = merge_approval_rounds(&proposal_round("file list"), &HashSet::new());
    let twice = merge_approval_rounds(&once, &HashSet::new());
    assert_eq!(once.len(), twice.len());
    assert!(matches!(&twice[2], Message::Tool { content, .. } if content == "file list"));
}

#[test]
fn test_protect_start_pins_the_turn_carrying_a_live_approval() {
    let messages = vec![
        Message::user("older".into()),
        Message::Assistant {
            meta: MessageMeta::default(),
            content: Some("done".into()),
            tool_calls: None,
            reasoning_content: None,
            thinking_blocks: None,
        },
        Message::user("list files".into()),
        Message::Assistant {
            meta: MessageMeta::default(),
            content: None,
            tool_calls: Some(vec![call("call-1", "shell", json!({"command": "ls"}))]),
            reasoning_content: None,
            thinking_blocks: None,
        },
        Message::tool_marked(
            "NOT EXECUTED".into(),
            "call-1".into(),
            Some("shell".into()),
            "a".into(),
        ),
    ];

    let live: HashSet<String> = ["a".to_string()].into_iter().collect();
    assert_eq!(protect_start(&messages, &live), Some(2));
    assert_eq!(protect_start(&messages, &HashSet::new()), None);
}

#[test]
fn test_pin_rules_share_one_boundary_function() {
    // Two live pins in one history. The whole-history trim must take the
    // *earliest* legal boundary; the chunk-local summary clamp must take only
    // the first pin inside its chunk and never walk back below its floor.
    let messages = vec![
        Message::user("u0".into()),                                            // 0
        Message::assistant(Some("a0".into()), None, None, None),               // 1
        Message::user("u1".into()),                                            // 2
        Message::assistant(Some("a1".into()), None, None, None),               // 3
        Message::tool_marked("g1".into(), "t1".into(), None, "appr-1".into()), // 4
        Message::user("u2".into()),                                            // 5
        Message::assistant(Some("a2".into()), None, None, None),               // 6
        Message::user("u3".into()),                                            // 7
        Message::tool_marked("g2".into(), "t2".into(), None, "appr-2".into()), // 8
    ];
    let live: HashSet<String> = ["appr-1", "appr-2"]
        .iter()
        .map(|id| id.to_string())
        .collect();

    // Whole history: min(2, 7) = 2 — both pinned turns stay in the tail.
    assert_eq!(protect_start(&messages, &live), Some(2));
    // Chunk-local: the first pin in [5, 9) is at 8, whose user boundary is 7.
    assert_eq!(clamp_boundary_for_pins(&messages, 5, 9, &live), Some(7));
    // No live pin: the chosen boundary stands untouched.
    assert_eq!(
        clamp_boundary_for_pins(&messages, 0, 9, &HashSet::new()),
        Some(9)
    );
    // A chunk whose only boundary is its floor is entirely pinned: skip it.
    assert_eq!(clamp_boundary_for_pins(&messages, 7, 9, &live), None);

    // Both rules are expressed through the same predicate and the same ruler,
    // so changing what "pinned" means touches one function, not four call sites.
    let src = include_str!("mod.rs");
    for rule in [
        "fn protect_start(messages: &[Message], live: &HashSet<String>)",
        "fn clamp_boundary_for_pins(",
    ] {
        let start = src.find(rule).unwrap();
        let end = start + src[start..].find("\n}\n").unwrap() + 2;
        let body = &src[start..end];
        assert!(
            body.contains("is_live_pin("),
            "{rule} lost the shared predicate"
        );
        assert!(
            body.contains("user_boundary_before("),
            "{rule} lost the shared user-boundary ruler"
        );
    }
}

#[test]
fn test_is_live_pin_ignores_unmarked_and_unknown_ids() {
    let marked = Message::tool_marked("g".into(), "t".into(), None, "appr-1".into());
    let unmarked = Message::user("plain".into());
    let live = HashSet::from(["appr-1".to_string()]);

    assert!(is_live_pin(&marked, &live));
    assert!(!is_live_pin(&marked, &HashSet::new()));
    assert!(!is_live_pin(&unmarked, &live));
    assert!(!is_live_pin(
        &marked,
        &HashSet::from(["appr-2".to_string()])
    ));

    // The ruler never crosses below its floor, even when a user turn is there.
    let history = vec![
        Message::user("u0".into()),
        Message::user("u1".into()),
        Message::assistant(Some("a".into()), None, None, None),
    ];
    assert_eq!(user_boundary_before(&history, 0, 2), Some(1));
    assert_eq!(user_boundary_before(&history, 1, 2), Some(1));
    assert_eq!(user_boundary_before(&history, 2, 2), None);
}

#[test]
fn test_live_ids_has_one_production_reader() {
    // Every ADR 0011 protection point takes its live set from one snapshot;
    // none reads the store directly or keeps a private copy of the rule.
    let module = include_str!("mod.rs");
    assert_eq!(
        module.matches("live_ids(").count(),
        2,
        "one definition plus the single reader inside LiveApprovals::collect"
    );
    for (name, src, expected) in [
        (
            "governance.rs",
            include_str!("../runner/hooks/governance.rs"),
            "LiveApprovals::collect(",
        ),
        (
            "trigger_consolidation.rs",
            include_str!("../runner/hooks/trigger_consolidation.rs"),
            "LiveApprovals::collect(",
        ),
        (
            "approval.rs",
            include_str!("../runner/hooks/approval.rs"),
            "LiveApprovals::collect(",
        ),
        (
            "consolidate.rs",
            include_str!("../consolidate.rs"),
            "protect: &LiveApprovals",
        ),
        (
            "session.rs",
            include_str!("../session.rs"),
            "LiveApprovals::collect(",
        ),
    ] {
        assert!(
            !src.contains("live_ids("),
            "{name} must not query the store on its own"
        );
        assert!(
            src.contains(expected),
            "{name} must take the shared snapshot via `{expected}`"
        );
    }
}

#[test]
fn test_store_mismatching_only_reports_carried_records_with_other_args() {
    let (store, _now) = store_with_clock(1_000);
    store.begin_turn("s1");
    store.propose(record("a", "shell", 1_000));

    // Proposed this turn: not a replay of an earlier approval yet.
    assert!(
        store
            .mismatching("s1", "shell", &args_hash(&json!({"command": "rm"})))
            .is_empty()
    );

    // Next turn it is carried, so a different hash is a veto candidate.
    store.begin_turn("s1");
    let mismatched = store.mismatching("s1", "shell", &args_hash(&json!({"command": "rm"})));
    assert_eq!(mismatched.len(), 1);
    assert!(
        store
            .mismatching("s1", "shell", &args_hash(&json!({"command": "ls"})))
            .is_empty(),
        "the matching hash is a redemption, not a veto"
    );
}

#[test]
fn test_schema_values_flatten_levels_for_the_editor() {
    let values = ApprovalConfig::default().schema_values();
    assert_eq!(values["levels.exec"], json!("ask"));
    assert_eq!(values["levels.read_only"], json!("allow"));
    assert_eq!(
        values["timeout_seconds"],
        json!(DEFAULT_APPROVAL_TIMEOUT_SECONDS)
    );
}

// ── fixtures ──

fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        args,
    }
}

/// One proposal round: user, assistant(call), marked guidance result.
fn one_round(call_id: &str, guidance: &str) -> Vec<Message> {
    vec![
        Message::user("list files".into()),
        Message::Assistant {
            meta: MessageMeta::default(),
            content: None,
            tool_calls: Some(vec![call(call_id, "shell", json!({"command": "ls"}))]),
            reasoning_content: None,
            thinking_blocks: None,
        },
        Message::tool_marked(
            guidance.into(),
            call_id.into(),
            Some("shell".into()),
            "a".into(),
        ),
    ]
}

/// Proposal round plus the owner's confirmation and the executed replay.
fn proposal_round(real: &str) -> Vec<Message> {
    let mut messages = one_round("call-1", "NOT EXECUTED");
    messages.push(Message::user("确认".into()));
    messages.push(Message::Assistant {
        meta: MessageMeta::default(),
        content: None,
        tool_calls: Some(vec![call("call-9", "shell", json!({"command": "ls"}))]),
        reasoning_content: None,
        thinking_blocks: None,
    });
    messages.push(Message::tool(
        real.into(),
        "call-9".into(),
        Some("shell".into()),
    ));
    messages
}

#[test]
fn test_store_snapshots_the_policy_so_editing_config_needs_a_restart() {
    // The store owns a snapshot: mutating the config a caller still holds
    // cannot change the policy in effect (ADR 0011, no hot-reload).
    let mut config = ApprovalConfig::default();
    let store = ApprovalStore::new(config.clone());

    config.tools.insert("shell".into(), ControlLevel::Allow);
    config.levels.exec = ControlLevel::Allow;

    assert_eq!(
        store.control_for("shell", RiskLevel::Exec),
        ControlLevel::Ask,
        "the running process keeps the policy it started with"
    );
    assert_eq!(store.config().levels.exec, ControlLevel::Ask);
}
