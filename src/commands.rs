/// Result of processing a user slash command.
pub struct CommandResult {
    /// Whether this was a recognized command.
    pub is_command: bool,
    /// Which tier handles this command.
    pub tier: CommandTier,
}

/// Which system layer handles the command.
#[derive(Debug, PartialEq)]
pub enum CommandTier {
    /// Channel layer: intercepts before entering MessageBus (e.g. /quit).
    Channel,
    /// AgentLoop layer: handled by inbound loop, does not spawn AgentRunner.
    AgentLoop,
    /// AgentRunner layer: flows through normal ReAct task pipeline.
    AgentRunner,
}

/// Check if input starts with '/' and dispatch to the appropriate tier.
/// Returns CommandResult with is_command=false for non-command input.
pub fn classify_command(input: &str) -> CommandResult {
    if !input.starts_with('/') {
        return CommandResult {
            is_command: false,
            tier: CommandTier::AgentRunner,
        };
    }

    match input {
        // Channel layer: immediate shutdown, bypass MessageBus
        "/quit" | "/exit" => CommandResult {
            is_command: true,
            tier: CommandTier::Channel,
        },
        // AgentLoop layer: lifecycle management
        "/stop" => CommandResult {
            is_command: true,
            tier: CommandTier::AgentLoop,
        },
        "/clear" | "/new" => CommandResult {
            is_command: true,
            tier: CommandTier::AgentLoop,
        },
        "/status" => CommandResult {
            is_command: true,
            tier: CommandTier::AgentLoop,
        },
        // Everything else: let the model handle it (e.g. /help, /explain)
        _ => CommandResult {
            is_command: false,
            tier: CommandTier::AgentRunner,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_non_command_input() {
        let result = classify_command("hello world");
        assert!(!result.is_command);
        assert_eq!(result.tier, CommandTier::AgentRunner);
    }

    #[test]
    fn test_quit_is_channel_tier() {
        let result = classify_command("/quit");
        assert!(result.is_command);
        assert_eq!(result.tier, CommandTier::Channel);
    }

    #[test]
    fn test_exit_is_channel_tier() {
        let result = classify_command("/exit");
        assert!(result.is_command);
        assert_eq!(result.tier, CommandTier::Channel);
    }

    #[test]
    fn test_stop_is_agent_loop_tier() {
        let result = classify_command("/stop");
        assert!(result.is_command);
        assert_eq!(result.tier, CommandTier::AgentLoop);
    }

    #[test]
    fn test_clear_is_agent_loop_tier() {
        let result = classify_command("/clear");
        assert!(result.is_command);
        assert_eq!(result.tier, CommandTier::AgentLoop);
    }

    #[test]
    fn test_new_is_agent_loop_tier() {
        let result = classify_command("/new");
        assert!(result.is_command);
        assert_eq!(result.tier, CommandTier::AgentLoop);
    }

    #[test]
    fn test_status_is_agent_loop_tier() {
        let result = classify_command("/status");
        assert!(result.is_command);
        assert_eq!(result.tier, CommandTier::AgentLoop);
    }

    #[test]
    fn test_unknown_goes_to_agent_runner() {
        let result = classify_command("/help");
        assert!(!result.is_command);
        assert_eq!(result.tier, CommandTier::AgentRunner);
    }

    #[test]
    fn test_regular_slash_path_goes_to_agent_runner() {
        let result = classify_command("/some/path");
        assert!(!result.is_command);
        assert_eq!(result.tier, CommandTier::AgentRunner);
    }
}
