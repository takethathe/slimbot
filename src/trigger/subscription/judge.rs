//! The judgment state machine: edge detection, the cooldown gate, the schedule
//! gate and the failure cadence.
//!
//! Pure functions on purpose. Every one of these rows is a user-visible
//! behaviour ("it stayed true all day, why did it tell me again?"), and a table
//! of decisions is the only honest way to test them.

use crate::cron::CronSchedule;

/// Outcome of one edge comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// `false → true`, outside cooldown: this is the fire.
    Fire,
    /// `false → true` but inside cooldown: no fire, **and the edge still
    /// advances**, so the next round must fall back to `false` first. Without
    /// this, a cooldown would turn into "send the overdue alert later".
    CooldownSuppressed,
    /// `true → true`: the condition merely remains true.
    NoRise,
    /// Anything compared against a `false` result: the state went back to calm.
    StillFalse,
}

impl Edge {
    /// The value written back into `prev`.
    pub fn next_prev(self) -> bool {
        !matches!(self, Edge::StillFalse)
    }
}

/// One row of the truth table.
///
/// `cooldown_ms` of `0` means "no cooldown", which must never be read as
/// "always in cooldown".
pub fn decide_edge(
    prev: bool,
    cur: bool,
    now_ms: i64,
    last_fired_at: Option<i64>,
    cooldown_ms: i64,
) -> Edge {
    if !cur {
        return Edge::StillFalse;
    }
    if prev {
        return Edge::NoRise;
    }
    if in_cooldown(now_ms, last_fired_at, cooldown_ms) {
        return Edge::CooldownSuppressed;
    }
    Edge::Fire
}

pub fn in_cooldown(now_ms: i64, last_fired_at: Option<i64>, cooldown_ms: i64) -> bool {
    if cooldown_ms <= 0 {
        return false;
    }
    last_fired_at.is_some_and(|fired| now_ms.saturating_sub(fired) < cooldown_ms)
}

/// Milliseconds left before the rule can fire again.
pub fn cooldown_remaining(now_ms: i64, last_fired_at: Option<i64>, cooldown_ms: i64) -> i64 {
    match last_fired_at {
        Some(fired) if cooldown_ms > 0 => (cooldown_ms - now_ms.saturating_sub(fired)).max(0),
        _ => 0,
    }
}

/// Judgment cadence for a rule that keeps failing: normal every tick, slowed
/// once the streak is long enough. Never faster than once per tick.
pub fn cadence_ticks(degraded_after: u32, degraded_every: u64, failures: u32) -> u64 {
    if failures >= degraded_after {
        degraded_every.max(1)
    } else {
        1
    }
}

/// Whether this rule is judged at this tick index. A counter, not a clock: a
/// sleeping loop would accumulate its own runtime and drift.
pub fn rise_is_due(tick_index: u64, cadence: u64) -> bool {
    cadence <= 1 || tick_index.is_multiple_of(cadence)
}

/// Schedule gate result. `next_run_at` is what the state should carry afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduleGate {
    pub is_due: bool,
    pub next_run_at: Option<i64>,
    /// A one-shot whose moment has come: it fires and is then disabled.
    pub is_last_occurrence: bool,
}

/// Is the schedule gate open?
///
/// The gate **never writes the edge state**. Treating "not due" as `false`
/// would refresh a daily rule's edge every hour and turn "tell me once" into
/// "tell me every day".
pub fn schedule_due(
    schedule: &CronSchedule,
    stored_next_run_at: Option<i64>,
    now_ms: i64,
) -> ScheduleGate {
    match schedule {
        CronSchedule::At { at_ms } => {
            let horizon = stored_next_run_at.unwrap_or(*at_ms);
            ScheduleGate {
                is_due: now_ms >= horizon,
                next_run_at: Some(horizon),
                is_last_occurrence: true,
            }
        }
        other => {
            // Periodic: a stored horizon is honoured; a missing one (first
            // round after a restart) is recomputed from now, never back-filled.
            let horizon = match stored_next_run_at {
                Some(stored) => stored,
                None => next_run_after(other, now_ms).unwrap_or(now_ms),
            };
            if now_ms >= horizon {
                ScheduleGate {
                    is_due: true,
                    next_run_at: next_run_after(other, now_ms),
                    is_last_occurrence: false,
                }
            } else {
                ScheduleGate {
                    is_due: false,
                    next_run_at: Some(horizon),
                    is_last_occurrence: false,
                }
            }
        }
    }
}

/// The occurrence after `now_ms`, in **local** time.
pub fn next_run_after(schedule: &CronSchedule, now_ms: i64) -> Option<i64> {
    match schedule {
        CronSchedule::At { at_ms } => {
            if *at_ms > now_ms {
                Some(*at_ms)
            } else {
                None
            }
        }
        CronSchedule::Every { every_ms } => {
            if *every_ms <= 0 {
                None
            } else {
                Some(now_ms + *every_ms)
            }
        }
        CronSchedule::Cron { expr, .. } => next_cron_local(expr, now_ms),
    }
}

/// Cron expressions resolve in the machine's local time. No timezone library is
/// pulled in for it: the user's clock is the reference, as it is for cron jobs.
pub fn next_cron_local(expr: &str, now_ms: i64) -> Option<i64> {
    use cron::Schedule;
    use std::str::FromStr;
    use std::time::SystemTime;

    let schedule = Schedule::from_str(expr).ok()?;
    let now = SystemTime::UNIX_EPOCH
        .checked_add(std::time::Duration::from_millis(now_ms.max(0) as u64))?;
    // `after` iterates in the timezone of the anchor, so the expression is
    // read on the machine's clock.
    let anchor = chrono::DateTime::<chrono::Local>::from(now);
    schedule
        .after(&anchor)
        .next()
        .map(|dt| dt.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_edge_table_rise_outside_cooldown_fires() {
        assert_eq!(
            decide_edge(false, true, 1_000, None, 0),
            Edge::Fire,
            "false -> true outside cooldown must fire"
        );
    }

    #[test]
    fn test_edge_table_rise_inside_cooldown_does_not_fire_but_advances() {
        let edge = decide_edge(false, true, 2_000, Some(1_000), 10_000);
        assert_eq!(edge, Edge::CooldownSuppressed);
        // The advance is the whole point: the next round needs a fresh rise.
        assert!(edge.next_prev());
        assert_eq!(
            decide_edge(true, true, 3_000, Some(1_000), 10_000),
            Edge::NoRise
        );
    }

    #[test]
    fn test_edge_table_still_true_does_not_fire() {
        assert_eq!(decide_edge(true, true, 1_000, None, 0), Edge::NoRise);
    }

    #[test]
    fn test_edge_table_any_to_false_resets_the_edge() {
        assert_eq!(
            decide_edge(true, false, 1_000, Some(0), 0),
            Edge::StillFalse
        );
        assert!(!Edge::StillFalse.next_prev());
        // And the next rise is a new fire, even with no cooldown configured.
        assert_eq!(decide_edge(false, true, 1_000, Some(0), 0), Edge::Fire);
    }

    #[test]
    fn test_zero_cooldown_is_never_in_cooldown() {
        assert!(!in_cooldown(i64::MAX, Some(0), 0));
        assert_eq!(cooldown_remaining(1_000, Some(0), 0), 0);
    }

    #[test]
    fn test_cooldown_remaining_counts_down() {
        assert_eq!(cooldown_remaining(5_000, Some(1_000), 10_000), 6_000);
        assert_eq!(cooldown_remaining(50_000, Some(1_000), 10_000), 0);
    }

    #[test]
    fn test_failures_slow_the_cadence_and_a_success_restores_it() {
        assert_eq!(cadence_ticks(3, 5, 0), 1);
        assert_eq!(cadence_ticks(3, 5, 2), 1);
        assert_eq!(cadence_ticks(3, 5, 3), 5);
        assert!(!rise_is_due(4, 5));
        assert!(rise_is_due(5, 5));
        assert!(rise_is_due(10, 5));
        assert!(rise_is_due(1, 1));
    }

    #[test]
    fn test_schedule_gate_holds_until_the_horizon() {
        let schedule = CronSchedule::every(3_600_000);
        let gate = schedule_due(&schedule, Some(10_000), 5_000);
        assert!(!gate.is_due);
        assert_eq!(gate.next_run_at, Some(10_000), "a held horizon is kept");

        let gate = schedule_due(&schedule, Some(10_000), 20_000);
        assert!(gate.is_due);
        // The next horizon is measured from the round that caught it, not from
        // the slot it missed, so a late check can never queue a second close fire.
        assert_eq!(gate.next_run_at, Some(3_620_000));
    }

    #[test]
    fn test_restart_recomputes_a_periodic_horizon_instead_of_back_firing() {
        let schedule = CronSchedule::every(60_000);
        // A stale stored horizon (downtime) is not consulted: nothing is due
        // the moment the process comes back.
        let gate = schedule_due(&schedule, None, 100_000);
        assert!(!gate.is_due);
        assert_eq!(gate.next_run_at, Some(160_000));
    }

    #[test]
    fn test_one_shot_is_due_once_and_is_the_last_occurrence() {
        let schedule = CronSchedule::at(10_000);
        assert!(!schedule_due(&schedule, Some(10_000), 9_000).is_due);
        let gate = schedule_due(&schedule, Some(10_000), 10_000);
        assert!(gate.is_due);
        assert!(gate.is_last_occurrence);
    }

    #[test]
    fn test_cron_horizon_is_local_wall_clock() {
        // Every minute: the next occurrence must be within the next 60 seconds,
        // which is only true if the expression resolved against a real clock.
        let now = chrono::Local::now().timestamp_millis();
        let next = next_run_after(&CronSchedule::cron("0 * * * * *".to_string(), None), now)
            .expect("expression must parse");
        assert!(next > now, "{next} vs {now}");
        assert!(next - now <= 61_000, "gap {}ms", next - now);
    }

    #[test]
    fn test_unparsable_cron_expression_yields_no_horizon() {
        assert_eq!(
            next_run_after(&CronSchedule::cron("nope".to_string(), None), 1_000),
            None
        );
    }
}
