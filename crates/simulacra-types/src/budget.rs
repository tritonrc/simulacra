use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Resource budget assigned to an agent at creation.
/// Checked before each operation, not after.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceBudget {
    pub max_tokens: u64,
    pub max_turns: u32,
    pub max_cost: Decimal,
    pub max_sub_agents: u32,
    pub max_vfs_bytes: u64,
    pub used_tokens: u64,
    pub used_turns: u32,
    pub used_cost: Decimal,
    pub used_sub_agents: u32,
    pub used_vfs_bytes: u64,
    pub max_fuel: u64,
    pub used_fuel: u64,
}

#[derive(Debug, Clone, Error)]
#[error("budget exhausted: {resource} — used {used}, limit {limit}")]
pub struct BudgetExhausted {
    pub resource: String,
    pub used: String,
    pub limit: String,
}

impl ResourceBudget {
    pub fn new(max_tokens: u64, max_turns: u32, max_cost: Decimal, max_sub_agents: u32) -> Self {
        Self {
            max_tokens,
            max_turns,
            max_cost,
            max_sub_agents,
            max_vfs_bytes: 0,
            used_tokens: 0,
            used_turns: 0,
            used_cost: Decimal::ZERO,
            used_sub_agents: 0,
            used_vfs_bytes: 0,
            max_fuel: 0,
            used_fuel: 0,
        }
    }

    /// Check all budget limits. Returns Ok(()) if under all limits,
    /// or Err with which resource was exhausted. A limit of 0 means unlimited.
    pub fn check_budget(&self) -> Result<(), BudgetExhausted> {
        if self.max_tokens > 0 && self.used_tokens >= self.max_tokens {
            return Err(BudgetExhausted {
                resource: "tokens".into(),
                used: self.used_tokens.to_string(),
                limit: self.max_tokens.to_string(),
            });
        }
        if self.max_turns > 0 && self.used_turns >= self.max_turns {
            return Err(BudgetExhausted {
                resource: "turns".into(),
                used: self.used_turns.to_string(),
                limit: self.max_turns.to_string(),
            });
        }
        if !self.max_cost.is_zero() && self.used_cost >= self.max_cost {
            return Err(BudgetExhausted {
                resource: "cost".into(),
                used: self.used_cost.to_string(),
                limit: self.max_cost.to_string(),
            });
        }
        if self.max_sub_agents > 0 && self.used_sub_agents >= self.max_sub_agents {
            return Err(BudgetExhausted {
                resource: "sub_agents".into(),
                used: self.used_sub_agents.to_string(),
                limit: self.max_sub_agents.to_string(),
            });
        }
        if self.max_vfs_bytes > 0 && self.used_vfs_bytes >= self.max_vfs_bytes {
            return Err(BudgetExhausted {
                resource: "vfs_bytes".into(),
                used: self.used_vfs_bytes.to_string(),
                limit: self.max_vfs_bytes.to_string(),
            });
        }
        if self.max_fuel > 0 && self.used_fuel >= self.max_fuel {
            return Err(BudgetExhausted {
                resource: "fuel".into(),
                used: self.used_fuel.to_string(),
                limit: self.max_fuel.to_string(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_budget_passes_when_under_all_limits() {
        let mut budget = ResourceBudget::new(100, 10, Decimal::new(250, 2), 4);
        budget.used_tokens = 99;
        budget.used_turns = 9;
        budget.used_cost = Decimal::new(249, 2);
        budget.used_sub_agents = 3;

        assert!(budget.check_budget().is_ok());
    }

    #[test]
    fn check_budget_returns_error_when_any_single_limit_is_exhausted() {
        let cases = [
            (
                "tokens",
                ResourceBudget {
                    max_tokens: 100,
                    max_turns: 10,
                    max_cost: Decimal::new(250, 2),
                    max_sub_agents: 4,
                    max_vfs_bytes: 0,
                    used_tokens: 100,
                    used_turns: 0,
                    used_cost: Decimal::ZERO,
                    used_sub_agents: 0,
                    used_vfs_bytes: 0,
                    max_fuel: 0,
                    used_fuel: 0,
                },
            ),
            (
                "turns",
                ResourceBudget {
                    max_tokens: 100,
                    max_turns: 10,
                    max_cost: Decimal::new(250, 2),
                    max_sub_agents: 4,
                    max_vfs_bytes: 0,
                    used_tokens: 0,
                    used_turns: 10,
                    used_cost: Decimal::ZERO,
                    used_sub_agents: 0,
                    used_vfs_bytes: 0,
                    max_fuel: 0,
                    used_fuel: 0,
                },
            ),
            (
                "cost",
                ResourceBudget {
                    max_tokens: 100,
                    max_turns: 10,
                    max_cost: Decimal::new(250, 2),
                    max_sub_agents: 4,
                    max_vfs_bytes: 0,
                    used_tokens: 0,
                    used_turns: 0,
                    used_cost: Decimal::new(250, 2),
                    used_sub_agents: 0,
                    used_vfs_bytes: 0,
                    max_fuel: 0,
                    used_fuel: 0,
                },
            ),
            (
                "sub_agents",
                ResourceBudget {
                    max_tokens: 100,
                    max_turns: 10,
                    max_cost: Decimal::new(250, 2),
                    max_sub_agents: 4,
                    max_vfs_bytes: 0,
                    used_tokens: 0,
                    used_turns: 0,
                    used_cost: Decimal::ZERO,
                    used_sub_agents: 4,
                    used_vfs_bytes: 0,
                    max_fuel: 0,
                    used_fuel: 0,
                },
            ),
        ];

        for (expected_resource, budget) in cases {
            let err = budget
                .check_budget()
                .expect_err("the exhausted resource should fail the budget check");
            assert_eq!(err.resource, expected_resource);
        }
    }

    #[test]
    fn check_budget_error_includes_resource_usage_and_limit_details() {
        let mut budget = ResourceBudget::new(100, 10, Decimal::new(250, 2), 4);
        budget.used_cost = Decimal::new(251, 2);

        let err = budget
            .check_budget()
            .expect_err("exhausted budgets should return structured details");

        assert_eq!(err.resource, "cost");
        assert_eq!(err.used, Decimal::new(251, 2).to_string());
        assert_eq!(err.limit, Decimal::new(250, 2).to_string());
    }

    #[test]
    fn limit_zero_means_unlimited_not_already_exhausted() {
        let mut budget = ResourceBudget::new(0, 0, Decimal::ZERO, 0);
        budget.used_tokens = 999_999;
        budget.used_turns = 999;
        budget.used_cost = Decimal::new(99999, 2);
        budget.used_sub_agents = 999;
        budget.used_vfs_bytes = 999_999;
        budget.used_fuel = 999_999;

        assert!(
            budget.check_budget().is_ok(),
            "limit of 0 should mean unlimited — high usage must not trigger exhaustion"
        );
    }

    #[test]
    fn fuel_budget_exhausted_returns_error() {
        let mut budget = ResourceBudget::new(0, 0, Decimal::ZERO, 0);
        budget.max_fuel = 1000;
        budget.used_fuel = 1000;

        let err = budget
            .check_budget()
            .expect_err("fuel at limit should fail budget check");
        assert_eq!(err.resource, "fuel");
        assert_eq!(err.used, "1000");
        assert_eq!(err.limit, "1000");
    }

    #[test]
    fn fuel_zero_means_unlimited() {
        let mut budget = ResourceBudget::new(0, 0, Decimal::ZERO, 0);
        budget.max_fuel = 0;
        budget.used_fuel = 999_999;

        assert!(
            budget.check_budget().is_ok(),
            "fuel limit of 0 should mean unlimited"
        );
    }
}
