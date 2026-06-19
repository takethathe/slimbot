pub(crate) mod agent_loop;
pub(crate) mod bootstrap;
pub(crate) mod channel;
pub(crate) mod cli;
pub(crate) mod commands;
pub(crate) mod config;
pub(crate) mod config_defs;
pub(crate) mod config_macro;
pub(crate) mod config_normalize;
pub(crate) mod consolidate;
pub(crate) mod context;
pub(crate) mod cron;
pub(crate) mod embed;
pub(crate) mod gateway;
pub(crate) mod heartbeat;
pub(crate) mod io_scheduler;
pub(crate) mod log;
pub(crate) mod macros;
pub(crate) mod memory;
pub(crate) mod message_bus;
pub(crate) mod path;
pub(crate) mod provider;
pub(crate) mod runner;
pub(crate) mod session;
pub(crate) mod setup;
pub(crate) mod snip;
pub(crate) mod tool;
pub(crate) mod tools;
pub(crate) mod utils;
pub(crate) mod worker;

// Re-export key types for integration tests and binary usage.
pub use channel::{Channel, ChannelFactory, ChannelManager};
pub use config::{
    ChannelConfig, Config, ConfigChange, ConfigValue, CronConfig, GatewayConfig, HeartbeatConfig,
    ToolEntry,
};
pub use config_defs::{AgentConfig, ProviderConfig};
pub use config_macro::{FieldMeta, Normalizable};
pub use consolidate::Consolidator;
pub use context::ContextBuilder;
pub use cron::{CronJob, CronPayload, CronSchedule, CronService};
pub use log::{LogLevel, init as log_init, log, should_log};
pub use memory::{MemoryStore, SharedMemoryStore};
pub use message_bus::{BusRequest, BusResult, MessageBus};
pub use path::PathManager;
pub use path::expand_home;
pub use provider::{FinishReason, LLMResponse, OpenAIProvider, Provider, Usage};
pub use runner::AgentRunner;
pub use session::{
    AgentEvent, Content, Message, Session, SessionManager, SharedSessionManager, TaskHook,
    TaskState,
};
pub use tool::{
    Tool, ToolCall, ToolDefinition, ToolManager, ensure_nonempty_tool_result, format_tool_error,
    persist_tool_result,
};
pub use tools::create_tool;
pub use utils::{
    TOOL_RESULT_PREVIEW_CHARS, TOOL_RESULTS_DIR, build_persisted_reference,
    truncate_text_head_tail, write_file_atomic,
};

// Re-export for binary usage.
pub use agent_loop::AgentLoop;
pub use cli::{CliArgs, Commands, run_agent_session};
pub use gateway::run_gateway;
pub use log as log_module;
pub use setup::run_setup;
pub use worker::WorkerPool;
