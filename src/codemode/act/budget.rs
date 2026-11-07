//! The act entry's per-run outward-call budget.
//!
//! One counter for everything that leaves the process: `ctx.http` and the
//! connector surface — reads and writes alike — draw on the same budget (spec
//! D, taken on the stricter side). Charging happens below the shims, so a
//! script cannot skip it by calling the raw `ctx.http_get` primitive, and
//! whoever spends the last call stops the run through the run's abort switch
//! instead of handing the script a catchable error.
//!
//! Attempts count, not successes: a refused target was still an attempt, and
//! counting late would let a script retry its way past the ceiling.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::codemode::sandbox::AbortHandle;

/// Ceiling on outward calls in one act run: connector calls and `ctx.http`
/// share this one counter (spec D). A fixed host constant, deliberately not a
/// meta field and not a config knob — a hand-edited `meta.json` must not be able
/// to lift a host gate.
pub(crate) const MAX_EXTERNAL_CALLS: usize = 20;

/// How many outward calls one act run has spent, and what to stop when the
/// budget is gone.
///
/// Cheap to clone (the counter and the abort switch are shared), so each path
/// that spends a call can hold its own handle and still charge the same budget.
#[derive(Clone, Default)]
pub(crate) struct CallBudget {
    calls: Arc<AtomicUsize>,
    max_calls: usize,
    /// Flipped when the call budget runs out, so the VM stops the script
    /// instead of letting it catch the error and carry on.
    abort: Option<Arc<AbortHandle>>,
}

impl CallBudget {
    pub(crate) fn new() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            max_calls: MAX_EXTERNAL_CALLS,
            abort: None,
        }
    }

    /// Share the run's abort switch, so a spent budget stops the script
    /// uncatchably.
    pub(crate) fn with_abort(mut self, abort: Arc<AbortHandle>) -> Self {
        self.abort = Some(abort);
        self
    }

    /// The run's abort switch, for the gates that stop the script for a reason
    /// other than the budget (a refused call, a write outside the ceiling).
    pub(crate) fn abort(&self) -> Option<&Arc<AbortHandle>> {
        self.abort.as_ref()
    }

    /// How many outward calls the run has attempted.
    pub(crate) fn external_calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Charge one outward attempt, stopping the script when the budget is
    /// spent. `Some(reason)` means the call must not go out.
    pub(crate) fn charge(&self) -> Option<String> {
        let attempt = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt <= self.max_calls {
            return None;
        }
        let reason = format!(
            "act() asked for more than {} external calls (connector calls and ctx.http together) \
             in one run; the run was stopped",
            self.max_calls
        );
        if let Some(abort) = &self.abort {
            abort.stop(reason.clone());
        }
        Some(reason)
    }
}
