//! Trace backends: destinations for span data.
//!
//! Each backend implements [`TraceBackend`] and is fully independent: a slow
//! or failing backend must not block or break other backends or the run.
//!
//! The mapping from the `tracing` config section to a backend list lives at
//! the wiring layer (`agent_loop`), keeping this module free of any config
//! dependency; construction from `TraceConfig` happens there.

mod file;
mod otel;

pub use file::FileBackend;
pub use otel::OtelBackend;

#[cfg(test)]
mod tests;
