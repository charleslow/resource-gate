// Budget enforcement: per-job caps, daily limits, agent ceiling

use crate::config::BudgetConfig;
use crate::store::Store;

pub struct BudgetEnforcer {
    config: BudgetConfig,
    store: Store,
}

impl BudgetEnforcer {
    pub fn new(config: BudgetConfig, store: Store) -> Self {
        Self { config, store }
    }

    /// Check if a new proposal with the given budget cap can be accepted
    /// without exceeding the daily limit. A daily_limit_usd of 0 means unlimited.
    pub fn can_accept(&self, budget_cap_usd: f64) -> anyhow::Result<BudgetCheck> {
        // 0 means unlimited — always accept
        if self.config.daily_limit_usd <= 0.0 {
            return Ok(BudgetCheck::Accepted);
        }

        let daily_spent = self.store.daily_spend()?;
        let remaining = self.config.daily_limit_usd - daily_spent;

        if budget_cap_usd > remaining {
            Ok(BudgetCheck::Rejected {
                reason: format!(
                    "budget cap ${:.2} exceeds daily remaining ${:.2} (limit ${:.2}, spent ${:.2})",
                    budget_cap_usd, remaining, self.config.daily_limit_usd, daily_spent
                ),
            })
        } else {
            Ok(BudgetCheck::Accepted)
        }
    }

    /// Compute current cost for a running sprint.
    pub fn compute_current_cost(
        &self,
        started_at: f64,
        gpu_count: u32,
        rate_per_gpu_second: f64,
    ) -> f64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let elapsed = now - started_at;
        elapsed * gpu_count as f64 * rate_per_gpu_second
    }

    /// Check if a running sprint has exceeded its per-sprint budget cap.
    pub fn exceeds_cap(&self, current_cost: f64, budget_cap_usd: f64) -> bool {
        current_cost >= budget_cap_usd
    }

    pub fn daily_limit(&self) -> f64 {
        self.config.daily_limit_usd
    }

    pub fn alert_threshold_pct(&self) -> u32 {
        self.config.alert_threshold_pct
    }

    pub fn daily_spent(&self) -> anyhow::Result<f64> {
        self.store.daily_spend()
    }
}

pub enum BudgetCheck {
    Accepted,
    Rejected { reason: String },
}
