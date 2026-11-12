use anyhow::Result;
use async_trait::async_trait;

use crate::runner::AgentRunner;
use crate::runner::traits::PreChatHook;
use crate::runner::types::{HookAction, RunSpec, RunState};

/// Applies message governance to the built context: drops orphan tool results,
/// backfills missing tool results, microcompacts, applies the tool-result budget,
/// and snips messages to fit the token window.
pub struct GovernanceHook;

#[async_trait]
impl PreChatHook for GovernanceHook {
    async fn call(&self, spec: &RunSpec<'_>, state: &mut RunState) -> Result<HookAction> {
        let mut all_messages = std::mem::take(&mut state.messages);

        // Every length limit below derives from the single window anchor,
        // measured with this session's estimation ratio.
        let limits = spec
            .pressure()
            .limits(super::char_per_token_ratio(spec).await);
        // One self-report decides what may be trimmed: the same predicate
        // gates both the full-result cleanup and the length truncation.
        let exempt = |name: &str| spec.tool_manager().is_context_exempt(name);

        // A live pending approval pins its turn to the tail: the model must keep
        // seeing what it is waiting for, so every trim stage below only ever
        // touches the part before the pin (ADR 0011).
        let live = crate::approval::LiveApprovals::collect(spec.approvals(), &spec.session_id);
        let pinned_from = live.protect_start(&all_messages);
        let mut pinned_tail = Vec::new();
        if let Some(start) = pinned_from
            && start < all_messages.len()
        {
            pinned_tail = all_messages.split_off(start);
        }

        // History governance: clean orphans, backfill, microcompact, budget tool results.
        AgentRunner::drop_orphan_tool_results(&mut all_messages);
        AgentRunner::backfill_missing_tool_results(&mut all_messages);
        all_messages =
            crate::snip::microcompact(all_messages, &limits, spec.workspace_dir(), &exempt);
        all_messages = crate::snip::apply_tool_result_budget(
            all_messages,
            &limits,
            spec.workspace_dir(),
            &exempt,
        );

        // Snip: truncate messages to fit within token budget (stateless, no persistence).
        all_messages = crate::snip::snip_messages(all_messages, &limits, spec.max_tokens());

        // Post-snip cleanup: snipping may have created new orphans.
        AgentRunner::drop_orphan_tool_results(&mut all_messages);
        AgentRunner::backfill_missing_tool_results(&mut all_messages);

        // Re-attach the pinned tail untouched.
        all_messages.extend(pinned_tail);

        state.messages = all_messages;
        Ok(HookAction::Continue)
    }
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use super::*;
    use crate::runner::test_support::RunFixture;

    use crate::session::Message;

    /// A live pending approval pins the turn that carries it: no trim stage may
    /// compact or drop the guidance result, even when it is old and large
    /// enough to be a compaction candidate (ADR 0011).
    fn pending_approval() -> crate::approval::PendingApproval {
        crate::approval::PendingApproval {
            approval_id: "appr-1".into(),
            owner_id: "owner".into(),
            session_id: "s1".into(),
            run_id: "run".into(),
            turn_kind: crate::session::TurnKind::User,
            tool: "shell".into(),
            args: serde_json::json!({"command": "deploy"}),
            args_hash: crate::approval::args_hash(&serde_json::json!({"command": "deploy"})),
            risk: crate::approval::RiskLevel::Exec,
            created_ms: 1_000,
        }
    }

    #[tokio::test]
    async fn test_governance_pins_the_turn_carrying_a_live_approval() {
        // A window small enough that the tail alone blows the budget, so the
        // unpinned case really does drop the pending turn.
        let fixture = RunFixture::builder()
            .max_tokens(256)
            .pressure(crate::pressure::PressureAnchor::from_window(1_024))
            .build();
        let spec = fixture.spec();

        let guidance = format!(
            "NOT EXECUTED: shell needs owner approval. {}",
            "x".repeat(600)
        );
        // Bloat first, then the pending turn, then later turns that push the
        // guidance out of the "recent tool results" window.
        let mut messages = vec![
            Message::system("sys".into()),
            Message::user("earlier".into()),
        ];
        for i in 0..4 {
            messages.push(Message::assistant(
                Some("thinking".into()),
                Some(vec![crate::tool::ToolCall {
                    id: format!("old-{i}"),
                    name: "read".into(),
                    args: serde_json::json!({}),
                }]),
                None,
                None,
            ));
            messages.push(Message::tool(
                "y".repeat(600),
                format!("old-{i}"),
                Some("read".into()),
            ));
        }
        // The pending turn.
        messages.push(Message::user("deploy it".into()));
        messages.push(Message::assistant(
            Some("calling".into()),
            Some(vec![crate::tool::ToolCall {
                id: "tc-1".into(),
                name: "shell".into(),
                args: serde_json::json!({"command": "deploy"}),
            }]),
            None,
            None,
        ));
        messages.push(Message::tool_marked(
            guidance.clone(),
            "tc-1".into(),
            Some("shell".into()),
            "appr-1".into(),
        ));
        // Later traffic.
        for i in 0..6 {
            messages.push(Message::user(format!("later {i}")));
            messages.push(Message::assistant(
                Some("ok".into()),
                Some(vec![crate::tool::ToolCall {
                    id: format!("new-{i}"),
                    name: "read".into(),
                    args: serde_json::json!({}),
                }]),
                None,
                None,
            ));
            messages.push(Message::tool(
                "z".repeat(600),
                format!("new-{i}"),
                Some("read".into()),
            ));
        }

        // Without a live record, the guidance is compacted away.
        let no_live_fixture = RunFixture::builder()
            .max_tokens(256)
            .pressure(crate::pressure::PressureAnchor::from_window(1_024))
            .build();
        let no_live = no_live_fixture.spec();
        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        state.messages = messages.clone();
        GovernanceHook.call(&no_live, &mut state).await.unwrap();
        let unpinned = state.messages.clone();
        assert!(
            !unpinned
                .iter()
                .any(|m| matches!(m, Message::Tool { content, .. } if content == &guidance)),
            "sanity: this list must compact the guidance without a pin"
        );

        // With it live, the whole pending turn survives verbatim.
        spec.approvals().propose(pending_approval());
        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        state.messages = messages;
        GovernanceHook.call(&spec, &mut state).await.unwrap();
        let pinned = state.messages;

        let kept = pinned
            .iter()
            .find(|m| matches!(m, Message::Tool { content, .. } if content == &guidance));
        assert!(
            kept.is_some(),
            "pinned guidance must keep its full text: {pinned:#?}"
        );
        assert_eq!(kept.unwrap().approval_id(), Some("appr-1"));
        assert!(
            pinned.iter().any(|m| matches!(m, Message::User { .. })),
            "the pinned turn keeps its user line"
        );
    }

    #[tokio::test]
    async fn test_governance_preserves_user_and_system_messages() {
        let fixture = RunFixture::new();
        let spec = fixture.spec();
        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        // Simulate a built context: system + user + orphan tool result.
        state.messages = vec![
            Message::system("sys".into()),
            Message::user("u".into()),
            Message::tool("orphan".into(), "tc-999".into(), Some("echo".into())),
        ];

        let action = GovernanceHook.call(&spec, &mut state).await.unwrap();
        assert_eq!(action, HookAction::Continue);
        // The orphan tool result should have been dropped.
        assert_eq!(state.messages.len(), 2);
        assert!(matches!(state.messages[1], Message::User { .. }));
    }
}
