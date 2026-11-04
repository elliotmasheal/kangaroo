//! The M3 fast path: deterministic routing, no model call. The rule table
//! lives in the domain ([`DEFAULT_ROUTE_RULES`]); this adapter only applies
//! it. Slow-path (small-model) classification stays off by default (pending
//! decision #1): an unmatched message is `Inline`.

use crate::domain::orchestration::{DEFAULT_ROUTE_RULES, RouteRule, Ticket, route};

pub struct Router {
    rules: &'static [RouteRule],
}

impl Router {
    pub const fn new() -> Self {
        Self {
            rules: DEFAULT_ROUTE_RULES,
        }
    }

    pub fn with_rules(rules: &'static [RouteRule]) -> Self {
        Self { rules }
    }

    pub fn route(&self, message: &str) -> Ticket {
        route(message, self.rules)
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::orchestration::{OutboxClass, TicketIntent};

    #[test]
    fn the_router_applies_the_default_table() {
        let router = Router::new();
        assert_eq!(
            router.route("帮我实现这个功能").intent,
            TicketIntent::Delegate
        );
        assert_eq!(router.route("查一下").intent, TicketIntent::FocusedSubtask);
        assert_eq!(router.route("你好").intent, TicketIntent::Inline);
    }

    #[test]
    fn an_explicit_empty_table_never_delegates() {
        let router = Router::with_rules(&[]);
        assert_eq!(router.route("写代码").intent, TicketIntent::Inline);
        assert_eq!(router.route("写代码").outbox_class(), OutboxClass::Answer);
    }
}
