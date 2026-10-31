//! Subagent runtime: agent definitions, per-run tool assembly, concurrency
//! gate and the `subagent` tool.
//!
//! An agent is a markdown file under `agents/<name>.md` whose frontmatter
//! declares the extra tools it needs and whose body is its system prompt. A
//! run is one-shot: it gets its own session (memory only) and its own
//! `ToolManager`, assembled from the process-level [`ToolCatalog`].

pub mod catalog;
pub mod definition;
pub mod runtime;
pub mod tool;

pub use catalog::{ToolCatalog, known_tool_names};
pub use definition::{
    AgentDefinition, AgentRegistry, FORBIDDEN_AGENT_TOOLS, default_tool_set, load_registry,
    load_workspace_registry,
};
pub use runtime::{
    AcquireMode, Origin, StartReject, StartRequest, StartedRun, SubagentRuntime, compose_result,
};
pub use tool::SubagentTool;
