//! Watchdog tick: reads the board and turns stalled assignments into one
//! attention each. The verdict itself is the domain's pure function; this
//! adapter only logs the decision, so the caller can render the attention on
//! whatever surface it owns.

use zeroclaw_log::{Action, Event, EventOutcome, record};

use crate::application::orchestration::AssignmentBoard;
use crate::domain::TimestampMs;
use crate::domain::orchestration::{Assignment, DEFAULT_WATCHDOG_THRESHOLD_MS};

/// One tick. Returns the assignments that just crossed the threshold and had
/// not raised attention before; an empty vector means nothing to report.
pub fn tick(
    board: &AssignmentBoard,
    now_ms: TimestampMs,
    threshold_ms: TimestampMs,
) -> Vec<Assignment> {
    let due = board.due_attention(now_ms, threshold_ms);
    for assignment in &due {
        record!(
            WARN,
            Event::new(module_path!(), Action::Note)
                .with_outcome(EventOutcome::Failure)
                .with_attrs(serde_json::json!({
                    "assignment": assignment.id,
                    "intent": assignment.intent.as_str(),
                    "age_ms": now_ms.saturating_sub(assignment.created_at_ms),
                })),
            "assignment stalled; attention due"
        );
    }
    due
}

/// The tick with the planning default threshold (pending decision #2).
pub fn tick_with_default_threshold(
    board: &AssignmentBoard,
    now_ms: TimestampMs,
) -> Vec<Assignment> {
    tick(board, now_ms, DEFAULT_WATCHDOG_THRESHOLD_MS)
}
