//! Sandboxed JavaScript execution, shared by data-source collectors, trigger
//! conditions, agent programs and executor action scripts.
//!
//! Nothing in this module is user-visible on its own: it is the layer that lets
//! model-written code run unattended without a shell, without a network of its
//! own, and without ever reaching the process environment.

pub mod act;
pub mod agent;
pub mod connector;
pub mod error;
pub mod host;
pub mod http;
pub mod redact;
pub mod sandbox;

// Consumed by the trigger layer (data-source collection and conditions) and by
// the executor tool; they reach the sandbox through this facade rather than its
// submodules.
pub(crate) use act::ActHost;
#[allow(unused_imports)]
pub use error::{ScriptError, ScriptErrorKind};
#[allow(unused_imports)]
pub use host::{HostBindings, HostEnv, LogSink, NoHost};
pub use redact::Redactor;
#[allow(unused_imports)]
pub use sandbox::{
    ENTRY_ACT, ENTRY_AGENT, ENTRY_COLLECT, ENTRY_JUDGE, Limits, RunRequest, execute, execute_async,
};
