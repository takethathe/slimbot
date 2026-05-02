use std::io::{self, Write};

use clap::{Parser, Subcommand};
use std::path::PathBuf;

use crate::agent_loop::AgentLoop;
use crate::session::{SessionManager, TaskHook};

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

/// Run an interactive CLI agent session.
/// Reads user input from stdin, submits tasks to the AgentLoop, and prints results.
pub async fn run_agent_session(agent_loop: &AgentLoop, session_id: Option<&str>) -> anyhow::Result<()> {
    let session_id_owned: Option<String>;
    let session_id = match session_id {
        Some(s) => s,
        None => {
            session_id_owned = Some(SessionManager::create_id());
            session_id_owned.as_deref().unwrap()
        }
    };
    eprintln!("SlimBot CLI agent session: {}", session_id);
    eprintln!("Type your message (Ctrl+D to exit):\n");

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

    Ok(())
}
