//! Card generation for the WebUI (json-render).
//!
//! Cards are a presentation-layer artifact produced by an *independent* model
//! call fired after a Run completes — they are not session messages, do not
//! enter the agent context, and are WebUI-only. See `docs/adr/0005` for the
//! rationale and `CONTEXT.md` for the vocabulary.
//!
//! Building blocks:
//! - [`spec`]: the streaming Card spec compiler (JSONL patch → Card spec),
//! - [`transcript`]: Run transcript assembly under a byte budget,
//! - [`registry`]: per-chat Catalog registration (the fail-closed switch),
//! - [`store`]: the append-only card sidecar (replay without polluting JSONL),
//! - [`event`]: the card lifecycle events sent over a dedicated mpsc.

pub mod event;
pub mod registry;
pub mod service;
pub mod spec;
pub mod store;
pub mod transcript;

pub use event::CardEvent;
pub use registry::{CardRegistry, MAX_PROMPT_BYTES, RegisterError};
pub use service::{CARD_CHANNEL_CAPACITY, CardPolicy, CardService, spawn_listener};
pub use spec::SpecCompiler;
pub use store::{CardRecord, CardStore};
pub use transcript::{TranscriptLimits, build_transcript};
