//! `Budget`, `Usage`, `BudgetLimit` (domain.md "Budget and Usage").

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Limits of one run. `max_wall` counts only time in `Preparing` and `Running` (runs.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct Budget {
    #[serde(with = "duration_ms")]
    #[ts(type = "number")]
    pub max_wall: Duration,
    pub max_turns: u32,
    pub max_cost_micros: u64,
    pub max_gate_attempts: u8,
}

/// Accumulated usage of one run. `wall` is computed server-side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ts_rs::TS)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_micros: u64,
    pub turns: u32,
    #[serde(with = "duration_ms")]
    #[ts(type = "number")]
    pub wall: Duration,
}

/// Which budget limit was crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum BudgetLimit {
    Wall,
    Turns,
    Cost,
}

impl fmt::Display for BudgetLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Wall => "max_wall",
            Self::Turns => "max_turns",
            Self::Cost => "max_cost",
        })
    }
}

impl Usage {
    /// The first limit this usage is over, checked in the order wall, cost, turns.
    ///
    /// A limit is exceeded when usage is strictly greater than it: reaching exactly
    /// `max_turns` turns or `max_cost_micros` is still within budget.
    pub fn exceeds(&self, budget: &Budget) -> Option<BudgetLimit> {
        if self.wall > budget.max_wall {
            Some(BudgetLimit::Wall)
        } else if self.cost_micros > budget.max_cost_micros {
            Some(BudgetLimit::Cost)
        } else if self.turns > budget.max_turns {
            Some(BudgetLimit::Turns)
        } else {
            None
        }
    }

    /// Adds a usage delta (saturating).
    pub fn add(&self, delta: &Usage) -> Usage {
        Usage {
            input_tokens: self.input_tokens.saturating_add(delta.input_tokens),
            output_tokens: self.output_tokens.saturating_add(delta.output_tokens),
            cost_micros: self.cost_micros.saturating_add(delta.cost_micros),
            turns: self.turns.saturating_add(delta.turns),
            wall: self.wall.saturating_add(delta.wall),
        }
    }
}

/// `std::time::Duration` as integer milliseconds.
pub mod duration_ms {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        s.serialize_u64(ms)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_millis(u64::deserialize(d)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget() -> Budget {
        Budget { max_wall: Duration::from_secs(600), max_turns: 40, max_cost_micros: 2_000_000, max_gate_attempts: 2 }
    }

    #[test]
    fn at_limit_is_within_budget() {
        let u = Usage { input_tokens: 10, output_tokens: 10, cost_micros: 2_000_000, turns: 40, wall: Duration::from_secs(600) };
        assert_eq!(u.exceeds(&budget()), None);
    }

    #[test]
    fn one_over_each_limit() {
        let base = Usage::default();
        let wall = Usage { wall: Duration::from_millis(600_001), ..base };
        let cost = Usage { cost_micros: 2_000_001, ..base };
        let turns = Usage { turns: 41, ..base };
        assert_eq!(wall.exceeds(&budget()), Some(BudgetLimit::Wall));
        assert_eq!(cost.exceeds(&budget()), Some(BudgetLimit::Cost));
        assert_eq!(turns.exceeds(&budget()), Some(BudgetLimit::Turns));
    }

    #[test]
    fn several_over_reports_wall_then_cost_then_turns() {
        let all = Usage { input_tokens: 0, output_tokens: 0, cost_micros: 3_000_000, turns: 50, wall: Duration::from_secs(700) };
        assert_eq!(all.exceeds(&budget()), Some(BudgetLimit::Wall));
        let cost_turns = Usage { wall: Duration::ZERO, ..all };
        assert_eq!(cost_turns.exceeds(&budget()), Some(BudgetLimit::Cost));
    }

    #[test]
    fn zero_budget_any_usage_exceeds() {
        let zero = Budget { max_wall: Duration::ZERO, max_turns: 0, max_cost_micros: 0, max_gate_attempts: 0 };
        assert_eq!(Usage::default().exceeds(&zero), None);
        assert_eq!(Usage { turns: 1, ..Usage::default() }.exceeds(&zero), Some(BudgetLimit::Turns));
    }

    #[test]
    fn add_saturates() {
        let a = Usage { cost_micros: u64::MAX - 1, turns: 1, ..Usage::default() };
        let b = Usage { cost_micros: 5, turns: 2, ..Usage::default() };
        let sum = a.add(&b);
        assert_eq!(sum.cost_micros, u64::MAX);
        assert_eq!(sum.turns, 3);
    }

    #[test]
    fn budget_serde_wall_in_millis() {
        let json = serde_json::to_value(budget()).unwrap();
        assert_eq!(json["max_wall"], 600_000);
        assert_eq!(serde_json::from_value::<Budget>(json).unwrap(), budget());
    }
}
