use std::io::{self, Write};

use clap::{Parser, Subcommand};
use std::path::PathBuf;

use crate::agent_loop::AgentLoop;
use crate::commands::{classify_command, CommandTier};
use crate::session::TaskHook;
use crate::{debug, fatal, info};

#[derive(Parser, Debug)]
#[command(name = "slimbot", about = "SlimBot AI agent")]
pub struct CliArgs {
    /// Path to config file (positional, for backward compatibility)
    #[arg(value_name = "CONFIG")]
    pub config_positional: Option<PathBuf>,

    /// Path to config file
    #[arg(short = 'c', long = "config")]
    pub config: Option<PathBuf>,

    /// Application data directory
    #[arg(short = 'd', long = "data-dir", global = true)]
    pub data_dir: Option<PathBuf>,

    /// Workspace directory (defaults to {data-dir}/workspace)
    #[arg(short = 'w', long = "workspace-dir", global = true)]
    pub workspace_dir: Option<PathBuf>,

    /// Log level: 0=debug, 1=info, 2=warning, 3=error, 4=fatal
    #[arg(long = "log", default_value_t = 1, global = true)]
    pub log: u8,

    /// Log file path (writes to both stderr and file)
    #[arg(long = "log-file", global = true)]
    pub log_file: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Run setup wizard (create/normalize config)
    Setup {
        /// Override config path for setup
        #[arg(short = 'c', long = "config")]
        config: Option<PathBuf>,
    },
    /// Start CLI interactive agent session
    Agent {
        /// Single-turn query (run one task and exit)
        #[arg(value_name = "QUERY")]
        query: Option<String>,

        /// Session ID (auto-generated if omitted)
        #[arg(short = 's', long = "session")]
        session_id: Option<String>,
    },
}

impl CliArgs {
    /// Get the effective config path: --config > positional > None (default filled by PathManager).
    pub fn config_path(&self) -> Option<&str> {
        self.config
            .as_ref()
            .or(self.config_positional.as_ref())
            .and_then(|p| p.to_str())
    }

    /// Get the effective data directory: --data-dir > None (default filled by PathManager).
    pub fn data_dir(&self) -> Option<&str> {
        self.data_dir.as_ref().and_then(|p| p.to_str())
    }

    /// Get the effective workspace directory: --workspace-dir > None (derived by PathManager).
    pub fn workspace_dir(&self) -> Option<&str> {
        self.workspace_dir.as_ref().and_then(|p| p.to_str())
    }
}

/// Run an interactive CLI agent session, or a single-turn query if provided.
/// Reads user input from stdin, submits tasks to the AgentLoop, and prints results.
pub async fn run_agent_session(
    agent_loop: &AgentLoop,
    session_id: Option<&str>,
    query: Option<&str>,
) -> anyhow::Result<()> {
    let session_id_owned: Option<String>;
    let session_id = match session_id {
        Some(s) => s,
        None => {
            session_id_owned = Some("cli:default".to_string());
            session_id_owned.as_deref().unwrap()
        }
    };

    // Single-turn: run query and exit
    if let Some(query) = query {
        debug!("[cli] Single-turn query: {}", query);
        // Ensure session exists before running
        if let Err(e) = crate::session::ensure_session(&agent_loop.session_manager(), session_id).await {
            fatal!("Failed to create session: {}", e);
        }
        let hook = TaskHook::new(session_id);
        let result = agent_loop
            .run_task(session_id, query.to_string(), hook, None)
            .await;
        debug!("[cli] run_task returned: success={}, content_len={}", result.success, result.content.len());

        if result.success {
            println!("{}", result.content);
        } else {
            fatal!("Agent task failed: {}", result.content);
        }
        return Ok(());
    }

    // Interactive mode: existing stdin loop — all I/O on main thread.
    // Ensure session exists before the loop.
    if let Err(e) = crate::session::ensure_session(&agent_loop.session_manager(), session_id).await {
        fatal!("Failed to create session: {}", e);
    }

    eprintln!("SlimBot CLI agent session: {}", session_id);
    eprintln!("Type your message (Ctrl+D or /quit to exit):\n");

    let stdin = io::stdin();
    let mut line = String::new();

    loop {
        eprint!("> ");
        io::stderr().flush()?;

        line.clear();
        let bytes = stdin.read_line(&mut line)?;
        if bytes == 0 {
            eprintln!("\nBye!");
            break;
        }

        let input = line.trim().to_string();
        if input.is_empty() {
            continue;
        }

        // Classify slash commands on the main thread.
        let cmd = classify_command(&input);
        if cmd.is_command {
            match cmd.tier {
                CommandTier::Channel => {
                    // /quit, /exit — exit the loop.
                    break;
                }
                CommandTier::AgentLoop => {
                    // /stop, /clear, /status — handle directly.
                    handle_cli_command(agent_loop, session_id, &input).await;
                    continue;
                }
                CommandTier::AgentRunner => {
                    // Recognized as a command but let the model handle it
                    // (e.g. /help). Fall through to run_task.
                }
            }
        }

        let hook = TaskHook::new(session_id);
        let result = agent_loop
            .run_task(session_id, input, hook, None)
            .await;

        if result.success {
            println!("{}", result.content);
        } else {
            eprintln!("Error: {}", result.content);
        }
        println!();
    }

    // Graceful shutdown on Ctrl+D or /quit
    agent_loop.shutdown_for_cli().await;

    Ok(())
}

/// Handle AgentLoop-tier commands directly on the main thread.
async fn handle_cli_command(agent_loop: &AgentLoop, session_id: &str, input: &str) {
    match input {
        "/stop" => {
            let sm = agent_loop.session_manager();
            let new_token = {
                let guard = sm.lock().await;
                guard.cancel_and_reset_session(session_id)
            };
            if new_token.is_some() {
                eprintln!("Session stopped. Use /new to start fresh.");
            } else {
                eprintln!("No active session to stop.");
            }
        }
        "/clear" | "/new" => {
            let sm = agent_loop.session_manager();
            let mut guard = sm.lock().await;
            guard.clear_session(session_id);
            drop(guard);
            eprintln!("Session cleared. Starting fresh.");
        }
        "/status" => {
            let sm = agent_loop.session_manager();
            let guard = sm.lock().await;
            let msg_count = guard.message_count(session_id);
            drop(guard);
            eprintln!("Session: {}\nMessages: {}", session_id, msg_count);
        }
        _ => {
            info!("[cli] Unhandled command: {}", input);
        }
    }
}
