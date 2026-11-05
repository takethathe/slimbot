//! Sandboxed JavaScript execution, shared by data-source collectors and
//! trigger conditions.
//!
//! Nothing in this module is user-visible on its own: it is the layer that lets
//! model-written code run unattended without a shell, without a network of its
//! own, and without ever reaching the process environment.

pub mod agent;
pub mod error;
pub mod host;
pub mod redact;
pub mod sandbox;

// Consumed by the trigger layer (data-source collection and conditions), which
// reaches the sandbox through this facade rather than its submodules.
#[allow(unused_imports)]
pub use error::{ScriptError, ScriptErrorKind};
#[allow(unused_imports)]
pub use host::{HostBindings, HostEnv, LogSink, NoHost};
#[allow(unused_imports)]
pub use redact::Redactor;
#[allow(unused_imports)]
pub use sandbox::{
    ENTRY_AGENT, ENTRY_COLLECT, ENTRY_JUDGE, Limits, RunRequest, execute, execute_async,
};
