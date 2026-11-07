//! Executors: a codemode action script the model stores and runs on demand.
//!
//! An executor is the operation mirror of a data source
//! ([`crate::trigger::source`]): a directory per definition holding `meta.json`
//! and an `act.js`, written by the host through one tool, with files as the
//! only truth. It mirrors the source's *shape*, not its schedule — there is no
//! tick and no runtime table, so every tool call scans the directory instead
//! (`scan-on-call`): a hand-edited meta is visible on the next call and no
//! cache can go stale.

pub mod args;
pub mod meta;
pub mod result;
pub mod run;
pub mod tool;
