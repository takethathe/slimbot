use clap::{Parser, Subcommand};
use std::path::PathBuf;

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
