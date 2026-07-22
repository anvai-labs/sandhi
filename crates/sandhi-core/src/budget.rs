//! Budgets + rate limits — the **enforcement mechanism** (AnvaiOps ADR-0047 D4). Caps are in
//! neutral **tokens**, not dollars; pricing is a downstream commercial concern.
//!
//! A [`BudgetLedger`] tracks spend per scope (a virtual-key id or a group id) and answers "would
//! this call exceed the cap?" before the call, then records the actual usage after.

use std::collections::HashMap;

/// A per-scope cap.
#[derive(Debug, Clone, Copy, Default)]
pub struct Budget {
    /// Max billable tokens over the ledger's window. `None` = unlimited.
    pub token_limit: Option<u64>,
}

impl Budget {
    pub fn tokens(limit: u64) -> Self {
        Self {
            token_limit: Some(limit),
        }
    }
}

/// Raised when a call would exceed a scope's budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetExceeded {
    pub scope: String,
    pub limit: u64,
    pub spent: u64,
    pub requested: u64,
}

/// Tracks spend per scope and enforces caps. In-memory (a durable/windowed ledger is later).
#[derive(Debug, Default)]
pub struct BudgetLedger {
    limits: HashMap<String, Budget>,
    spent: HashMap<String, u64>,
    reserved: HashMap<String, u64>,
}

impl BudgetLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_limit(&mut self, scope: impl Into<String>, budget: Budget) {
        self.limits.insert(scope.into(), budget);
    }

    pub fn spent(&self, scope: &str) -> u64 {
        self.spent.get(scope).copied().unwrap_or(0)
    }

    /// Tokens held by in-flight requests for this scope.
    pub fn reserved(&self, scope: &str) -> u64 {
        self.reserved.get(scope).copied().unwrap_or(0)
    }

    /// Would `add` billable tokens exceed the scope's cap? An unset/unlimited scope always
    /// passes. Enforcement only — no pricing.
    pub fn check(&self, scope: &str, add: u64) -> Result<(), BudgetExceeded> {
        if let Some(limit) = self.limits.get(scope).and_then(|b| b.token_limit) {
            let spent = self.spent(scope);
            if spent
                .saturating_add(self.reserved(scope))
                .saturating_add(add)
                > limit
            {
                return Err(BudgetExceeded {
                    scope: scope.to_string(),
                    limit,
                    spent,
                    requested: add,
                });
            }
        }
        Ok(())
    }

    /// Atomically check and reserve an upper bound for one in-flight request.
    pub fn reserve(&mut self, scope: &str, tokens: u64) -> Result<(), BudgetExceeded> {
        self.check(scope, tokens)?;
        *self.reserved.entry(scope.to_string()).or_insert(0) += tokens;
        Ok(())
    }

    /// Replace an in-flight reservation with the actual finalized billable usage.
    pub fn reconcile(&mut self, scope: &str, reserved: u64, actual: u64) {
        let held = self.reserved.entry(scope.to_string()).or_insert(0);
        *held = held.saturating_sub(reserved);
        self.record(scope, actual);
    }

    /// Release a reservation when a call fails before any billable usage is reported.
    pub fn release(&mut self, scope: &str, reserved: u64) {
        let held = self.reserved.entry(scope.to_string()).or_insert(0);
        *held = held.saturating_sub(reserved);
    }

    /// Record actual usage after a call (call once the real token counts are known).
    pub fn record(&mut self, scope: &str, tokens: u64) {
        *self.spent.entry(scope.to_string()).or_insert(0) += tokens;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_scope_is_unlimited() {
        let ledger = BudgetLedger::new();
        assert!(ledger.check("vk_1", 1_000_000).is_ok());
    }

    #[test]
    fn enforces_cap_and_tracks_spend() {
        let mut ledger = BudgetLedger::new();
        ledger.set_limit("group:team", Budget::tokens(100));

        assert!(ledger.check("group:team", 60).is_ok());
        ledger.record("group:team", 60);
        assert_eq!(ledger.spent("group:team"), 60);

        // 60 spent + 50 would be 110 > 100 → blocked.
        let err = ledger.check("group:team", 50).unwrap_err();
        assert_eq!(err.spent, 60);
        assert_eq!(err.limit, 100);
        assert_eq!(err.requested, 50);

        // 60 + 40 == 100 is allowed (cap is inclusive).
        assert!(ledger.check("group:team", 40).is_ok());
    }

    #[test]
    fn reservations_prevent_concurrent_oversubscription_and_reconcile() {
        let mut ledger = BudgetLedger::new();
        ledger.set_limit("group:team", Budget::tokens(100));
        ledger.reserve("group:team", 60).unwrap();
        assert_eq!(ledger.reserved("group:team"), 60);
        assert!(ledger.reserve("group:team", 50).is_err());

        ledger.reconcile("group:team", 60, 40);
        assert_eq!(ledger.reserved("group:team"), 0);
        assert_eq!(ledger.spent("group:team"), 40);
        ledger.reserve("group:team", 60).unwrap();
        ledger.release("group:team", 60);
        assert_eq!(ledger.reserved("group:team"), 0);
    }
}
