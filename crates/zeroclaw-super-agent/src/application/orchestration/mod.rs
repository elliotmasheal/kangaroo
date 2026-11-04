//! M3 orchestration: routing (Decision), assignment supervision (Assignment),
//! and the watchdog tick. The dispatcher is an observer, not an executor —
//! worker execution stays on the runtime's `spawn_subagent` / `delegate`
//! tools, which the main agent calls inside its turn.

pub mod board;
pub mod router;
pub mod watchdog;

pub use board::AssignmentBoard;
pub use router::Router;

use crate::domain::orchestration::{Ticket, TicketIntent};

/// The in-turn instruction appended to a worker-bound message. It is a
/// prompt-side constraint, not an execution: the agent still decides to call
/// the worker tool. `None` for `Inline`, where the message is passed through
/// unchanged.
pub fn dispatch_directive(ticket: &Ticket) -> Option<&'static str> {
    match ticket.intent {
        TicketIntent::Inline => None,
        TicketIntent::FocusedSubtask => Some(
            "This turn is one focused subtask. Hand the scoped work to a worker \
             (spawn_subagent or delegate), then report the result in your own words. \
             Every claim taken from the worker must cite a concrete evidence reference \
             (file path, command output, or trace id). If the worker gives none, say \
             that the report is unevidenced instead of presenting it as fact.",
        ),
        TicketIntent::Delegate => Some(
            "This turn is a full task for a worker. Use spawn_subagent or delegate for \
             the execution, do not perform the work inline. Report back with the outcome \
             and a concrete evidence reference (file path, command output, or trace id). \
             Never present an unevidenced worker report as fact.",
        ),
        TicketIntent::Sop => Some(
            "This turn follows a procedure. If a matching SOP or runbook exists, follow \
             it and report each step's outcome with its evidence reference; otherwise do \
             the work inline.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_messages_get_no_directive() {
        assert_eq!(dispatch_directive(&Ticket::inline()), None);
    }

    #[test]
    fn worker_tickets_carry_an_evidence_constraint() {
        for intent in [
            TicketIntent::FocusedSubtask,
            TicketIntent::Delegate,
            TicketIntent::Sop,
        ] {
            let ticket = Ticket {
                intent,
                matched_phrase: None,
            };
            let directive = dispatch_directive(&ticket).expect("worker intent has a directive");
            assert!(
                directive.contains("evidence reference"),
                "the directive must demand evidence: {intent:?}"
            );
        }
    }
}
