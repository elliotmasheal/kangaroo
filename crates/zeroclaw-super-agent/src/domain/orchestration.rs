//! Orchestration vocabulary (M3): the router's decision (a `Ticket`), the
//! assignment lifecycle it opens, and the watchdog's stall verdict. Pure rules
//! only — the application layer owns the board and the callers.
//!
//! The four intents are the architecture document's Super-agent mapping:
//! `Inline` answers in the same turn, `FocusedSubtask` / `Delegate` / `Sop`
//! name work a worker performs. Nothing here executes a worker: the runtime
//! `spawn_subagent` / `delegate` tools stay the only execution path, and this
//! layer only records and supervises.

use super::TimestampMs;

/// What the router decided one user turn is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketIntent {
    /// No worker: the turn is answered directly.
    Inline,
    /// One scoped subtask handed to a worker.
    FocusedSubtask,
    /// A full task delegated to a worker.
    Delegate,
    /// A procedure the agent should follow (SOP-driven).
    Sop,
}

impl TicketIntent {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::FocusedSubtask => "focused_subtask",
            Self::Delegate => "delegate",
            Self::Sop => "sop",
        }
    }

    /// Whether this intent names work a worker performs. The supervisor only
    /// opens assignments for these.
    pub const fn needs_worker(self) -> bool {
        !matches!(self, Self::Inline)
    }
}

/// The four reply classes the architecture maps onto existing surfaces. The
/// router labels a turn; the entry adapter decides where it lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxClass {
    /// Fast entry confirmation for a turn that will take a worker.
    Acknowledgement,
    /// The normal reply.
    Answer,
    /// A worker result transcribed back to the user (carries evidence).
    Result,
    /// Something needs the user: an approval or a stalled assignment.
    Attention,
}

impl OutboxClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Acknowledgement => "acknowledgement",
            Self::Answer => "answer",
            Self::Result => "result",
            Self::Attention => "attention",
        }
    }
}

/// One routing decision. `matched_phrase` is the rule that fired, kept for
/// the audit trail; `None` means the default rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    pub intent: TicketIntent,
    pub matched_phrase: Option<String>,
}

impl Ticket {
    pub fn inline() -> Self {
        Self {
            intent: TicketIntent::Inline,
            matched_phrase: None,
        }
    }

    /// The reply class this ticket's *entry* uses: a worker-bound turn
    /// acknowledges first, an inline turn answers directly.
    pub const fn outbox_class(&self) -> OutboxClass {
        if self.intent.needs_worker() {
            OutboxClass::Acknowledgement
        } else {
            OutboxClass::Answer
        }
    }
}

/// One rule in the fast path: a phrase and the intent it selects. Rules
/// are ordered by the table that owns them; first match wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteRule {
    pub phrase: &'static str,
    pub intent: TicketIntent,
}

/// The deterministic fast path (no model call). Ordered: heavier intents
/// first so "帮我实现并调研" routes as `Delegate`, not `FocusedSubtask`.
pub const DEFAULT_ROUTE_RULES: &[RouteRule] = &[
    RouteRule {
        phrase: "帮我实现",
        intent: TicketIntent::Delegate,
    },
    RouteRule {
        phrase: "帮我改",
        intent: TicketIntent::Delegate,
    },
    RouteRule {
        phrase: "写代码",
        intent: TicketIntent::Delegate,
    },
    RouteRule {
        phrase: "改代码",
        intent: TicketIntent::Delegate,
    },
    RouteRule {
        phrase: "implement this",
        intent: TicketIntent::Delegate,
    },
    RouteRule {
        phrase: "fix this",
        intent: TicketIntent::Delegate,
    },
    RouteRule {
        phrase: "refactor",
        intent: TicketIntent::Delegate,
    },
    RouteRule {
        phrase: "按流程",
        intent: TicketIntent::Sop,
    },
    RouteRule {
        phrase: "按 sop",
        intent: TicketIntent::Sop,
    },
    RouteRule {
        phrase: "runbook",
        intent: TicketIntent::Sop,
    },
    RouteRule {
        phrase: "查一下",
        intent: TicketIntent::FocusedSubtask,
    },
    RouteRule {
        phrase: "搜一下",
        intent: TicketIntent::FocusedSubtask,
    },
    RouteRule {
        phrase: "研究一下",
        intent: TicketIntent::FocusedSubtask,
    },
    RouteRule {
        phrase: "调研",
        intent: TicketIntent::FocusedSubtask,
    },
    RouteRule {
        phrase: "look up",
        intent: TicketIntent::FocusedSubtask,
    },
    RouteRule {
        phrase: "research",
        intent: TicketIntent::FocusedSubtask,
    },
];

/// Classify one user message against a rule table. Exact-by-default: no
/// rule fires ⇒ `Inline`, which is the only intent that never opens an
/// assignment.
pub fn route(message: &str, rules: &[RouteRule]) -> Ticket {
    let text = message.to_lowercase();
    for rule in rules {
        if contains_phrase(&text, rule.phrase) {
            return Ticket {
                intent: rule.intent,
                matched_phrase: Some(rule.phrase.to_owned()),
            };
        }
    }
    Ticket::inline()
}

/// ASCII phrases match on word boundaries so "implementation" never fires
/// "implement"; CJK phrases are matched as substrings (no word delimiters).
fn contains_phrase(haystack: &str, needle: &str) -> bool {
    let ascii_phrase = needle
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == ' ');
    if !ascii_phrase {
        return haystack.contains(needle);
    }
    let bytes = haystack.as_bytes();
    let mut from = 0usize;
    while let Some(offset) = haystack[from..].find(needle) {
        let start = from + offset;
        let end = start + needle.len();
        let before_ok = start == 0 || !is_word_byte(bytes[start - 1]);
        let after_ok = end == bytes.len() || !is_word_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
        if from >= haystack.len() {
            break;
        }
    }
    false
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Where one assignment is in its lifecycle. A closed set: settled assignments
/// never move again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignmentStatus {
    /// The turn opened the assignment; the worker has not reported yet.
    Dispatched,
    /// A worker execution for this assignment is observable.
    Running,
    /// The worker reported (or the turn ended); terminal.
    Settled,
}

impl AssignmentStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dispatched => "dispatched",
            Self::Running => "running",
            Self::Settled => "settled",
        }
    }

    pub const fn is_settled(self) -> bool {
        matches!(self, Self::Settled)
    }
}

/// One supervised unit of worker work. In-memory only (architecture §9 #7):
/// losing the supervision window on restart loses no data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub id: String,
    pub intent: TicketIntent,
    pub status: AssignmentStatus,
    /// Trace to where the worker's report can be checked (`transcripts`
    /// msg id today). `None` means "reported without evidence" — the
    /// transcription side must label it as inference, never as fact.
    pub evidence_ref: Option<String>,
    pub created_at_ms: TimestampMs,
    pub attention_emitted: bool,
}

impl Assignment {
    pub fn dispatched(id: String, ticket: &Ticket, created_at_ms: TimestampMs) -> Self {
        Self {
            id,
            intent: ticket.intent,
            status: AssignmentStatus::Dispatched,
            evidence_ref: None,
            created_at_ms,
            attention_emitted: false,
        }
    }

    /// `Dispatched → Running`. Returns whether the transition happened.
    pub fn mark_running(&mut self) -> bool {
        if self.status == AssignmentStatus::Dispatched {
            self.status = AssignmentStatus::Running;
            true
        } else {
            false
        }
    }

    /// `* → Settled`, carrying the evidence reference. A settled assignment
    /// is terminal: a second settle is a no-op (returns `false`).
    pub fn settle(&mut self, evidence_ref: Option<String>) -> bool {
        if self.status.is_settled() {
            return false;
        }
        self.status = AssignmentStatus::Settled;
        self.evidence_ref = evidence_ref;
        true
    }
}

/// The watchdog's input: an assignment's non-settled age, and the verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallVerdict {
    Ok,
    Stalled,
}

/// Planning constant from the autonomous-progress default (pending decision
/// #2): ten minutes, the same order of magnitude as the dream lock's idle
/// threshold and trivially changeable.
pub const DEFAULT_WATCHDOG_THRESHOLD_MS: TimestampMs = 10 * 60 * 1000;

/// Whether an assignment has gone quiet for too long. Pure: the caller
/// supplies `now`.
pub fn stall_verdict(
    status: AssignmentStatus,
    created_at_ms: TimestampMs,
    now_ms: TimestampMs,
    threshold_ms: TimestampMs,
) -> StallVerdict {
    if status.is_settled() {
        return StallVerdict::Ok;
    }
    if now_ms.saturating_sub(created_at_ms) > threshold_ms {
        StallVerdict::Stalled
    } else {
        StallVerdict::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_table_routes_heavy_work_to_a_worker() {
        let ticket = route(
            "帮我实现登录功能，顺便研究一下现有鉴权",
            DEFAULT_ROUTE_RULES,
        );
        assert_eq!(
            ticket.intent,
            TicketIntent::Delegate,
            "the heavier intent wins the first match"
        );
        assert_eq!(ticket.matched_phrase.as_deref(), Some("帮我实现"));
    }

    #[test]
    fn every_intent_is_reachable_from_the_default_table() {
        let focused = route("查一下这个报错", DEFAULT_ROUTE_RULES);
        assert_eq!(focused.intent, TicketIntent::FocusedSubtask);
        let sop = route("按流程执行发布检查", DEFAULT_ROUTE_RULES);
        assert_eq!(sop.intent, TicketIntent::Sop);
    }

    #[test]
    fn unmatched_messages_stay_inline() {
        let ticket = route("今天心情不错", DEFAULT_ROUTE_RULES);
        assert_eq!(ticket.intent, TicketIntent::Inline);
        assert_eq!(ticket.matched_phrase, None);
        assert_eq!(ticket.outbox_class(), OutboxClass::Answer);
    }

    #[test]
    fn ascii_rules_respect_word_boundaries_and_case() {
        assert!(contains_phrase("please refactor this", "refactor"));
        assert!(
            !contains_phrase("the refactoring is done", "refactor"),
            "a suffix changes the word, so the rule must not fire"
        );
        assert!(!contains_phrase("implementation detail", "implement"));
        assert!(
            contains_phrase("我要研究一下这个库", "研究一下"),
            "CJK phrases match without word delimiters"
        );
        assert_eq!(
            route("REFACTOR THIS", DEFAULT_ROUTE_RULES).intent,
            TicketIntent::Delegate,
            "routing lowercases before matching ASCII rules"
        );
    }

    #[test]
    fn worker_intents_acknowledge_and_inline_answers() {
        let delegate = route("写代码", DEFAULT_ROUTE_RULES);
        assert!(delegate.intent.needs_worker());
        assert_eq!(delegate.outbox_class(), OutboxClass::Acknowledgement);
        assert!(!TicketIntent::Inline.needs_worker());
    }

    #[test]
    fn assignment_lifecycle_is_forward_only() {
        let ticket = Ticket {
            intent: TicketIntent::Delegate,
            matched_phrase: Some("写代码".to_owned()),
        };
        let mut assignment = Assignment::dispatched("a-1".to_owned(), &ticket, 100);
        assert_eq!(assignment.status, AssignmentStatus::Dispatched);
        assert!(assignment.mark_running());
        assert!(!assignment.mark_running(), "running is not re-entrant");
        assert!(assignment.settle(Some("cli-1-000010".to_owned())));
        assert!(!assignment.settle(None), "settled is terminal");
        assert_eq!(assignment.evidence_ref.as_deref(), Some("cli-1-000010"));
    }

    #[test]
    fn stall_verdict_uses_the_threshold_and_ignores_settled_work() {
        assert_eq!(
            stall_verdict(AssignmentStatus::Dispatched, 0, 60_000, 60_000),
            StallVerdict::Ok,
            "exactly at the threshold is not yet stalled"
        );
        assert_eq!(
            stall_verdict(AssignmentStatus::Running, 0, 60_001, 60_000),
            StallVerdict::Stalled
        );
        assert_eq!(
            stall_verdict(AssignmentStatus::Settled, 0, u64::MAX, 60_000),
            StallVerdict::Ok,
            "settled work never stalls"
        );
    }

    #[test]
    fn status_tokens_are_stable() {
        assert_eq!(TicketIntent::Inline.as_str(), "inline");
        assert_eq!(AssignmentStatus::Settled.as_str(), "settled");
        assert_eq!(OutboxClass::Attention.as_str(), "attention");
    }
}
