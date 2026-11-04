//! The in-memory assignment board (architecture §9 #7): the supervisor
//! records what a worker-bound turn opened, and the watchdog reads it. Restart
//! loses only the supervision window, never data.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::domain::TimestampMs;
use crate::domain::orchestration::{Assignment, StallVerdict, Ticket, stall_verdict};

#[derive(Default)]
struct BoardState {
    seq: u64,
    assignments: HashMap<String, Assignment>,
}

pub struct AssignmentBoard {
    state: Mutex<BoardState>,
}

impl AssignmentBoard {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(BoardState::default()),
        }
    }

    /// Open one assignment for a worker-bound ticket. An `Inline` ticket
    /// opens nothing (`None`) — inline turns are answered in place and have
    /// no lifecycle to supervise.
    pub fn dispatch(&self, ticket: &Ticket, now_ms: TimestampMs) -> Option<String> {
        if !ticket.intent.needs_worker() {
            return None;
        }
        let mut state = self.lock();
        state.seq += 1;
        let id = format!("asg-{}-{}", now_ms, state.seq);
        let assignment = Assignment::dispatched(id.clone(), ticket, now_ms);
        state.assignments.insert(id.clone(), assignment);
        Some(id)
    }

    pub fn mark_running(&self, id: &str) -> bool {
        let mut state = self.lock();
        match state.assignments.get_mut(id) {
            Some(assignment) => assignment.mark_running(),
            None => false,
        }
    }

    pub fn settle(&self, id: &str, evidence_ref: Option<String>) -> bool {
        let mut state = self.lock();
        match state.assignments.get_mut(id) {
            Some(assignment) => assignment.settle(evidence_ref),
            None => false,
        }
    }

    /// Every non-settled assignment that has crossed the stall threshold and
    /// has not raised attention yet; marking happens here so the same
    /// assignment raises at most one attention.
    pub fn due_attention(&self, now_ms: TimestampMs, threshold_ms: TimestampMs) -> Vec<Assignment> {
        let mut state = self.lock();
        let mut due = Vec::new();
        for assignment in state.assignments.values_mut() {
            if assignment.attention_emitted {
                continue;
            }
            if stall_verdict(
                assignment.status,
                assignment.created_at_ms,
                now_ms,
                threshold_ms,
            ) == StallVerdict::Stalled
            {
                assignment.attention_emitted = true;
                due.push(assignment.clone());
            }
        }
        due
    }

    pub fn get(&self, id: &str) -> Option<Assignment> {
        self.lock().assignments.get(id).cloned()
    }

    pub fn len(&self) -> usize {
        self.lock().assignments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BoardState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for AssignmentBoard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::orchestration::TicketIntent;

    fn delegate_ticket() -> Ticket {
        Ticket {
            intent: TicketIntent::Delegate,
            matched_phrase: Some("写代码".to_owned()),
        }
    }

    #[test]
    fn inline_tickets_open_no_assignment() {
        let board = AssignmentBoard::new();
        assert_eq!(board.dispatch(&Ticket::inline(), 1), None);
        assert!(board.is_empty());
    }

    #[test]
    fn worker_tickets_settle_with_the_evidence_reference() {
        let board = AssignmentBoard::new();
        let id = board
            .dispatch(&delegate_ticket(), 100)
            .expect("worker ticket opens an assignment");
        assert!(board.mark_running(&id));
        assert!(board.settle(&id, Some("cli-1-000010".to_owned())));
        let settled = board.get(&id).expect("assignment present");
        assert_eq!(settled.evidence_ref.as_deref(), Some("cli-1-000010"));
    }

    #[test]
    fn a_stalled_assignment_raises_attention_exactly_once() {
        let board = AssignmentBoard::new();
        let id = board
            .dispatch(&delegate_ticket(), 0)
            .expect("assignment opened");
        assert!(board.due_attention(30_000, 60_000).is_empty(), "not yet");
        let due = board.due_attention(61_000, 60_000);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, id);
        assert!(
            board.due_attention(121_000, 60_000).is_empty(),
            "the same assignment never raises twice"
        );
    }

    #[test]
    fn settled_work_never_raises_attention() {
        let board = AssignmentBoard::new();
        let id = board
            .dispatch(&delegate_ticket(), 0)
            .expect("assignment opened");
        assert!(board.settle(&id, None));
        assert!(board.due_attention(120_000, 60_000).is_empty());
    }
}
