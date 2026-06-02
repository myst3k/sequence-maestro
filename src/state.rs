//! The generated state file: written once by `discover` (onboarding), then
//! read back by the engine each cycle. Not hand-edited — re-run `discover` to
//! refresh it. JSON so it's trivially diffable.

use std::io;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::allocator::FundingStrategy;
use crate::schedule::PaySchedule;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub discovered_at: NaiveDate,
    /// The canonical funding rhythm the allocator counts against.
    pub pay_schedule: PaySchedule,
    /// How to ration the pool on a short month (chosen at onboarding).
    #[serde(default)]
    pub funding_strategy: FundingStrategy,
    /// The pod(s) income flows into — the engine funds bills *from* these.
    /// Detected by tracing where each income source sends its money, ranked by
    /// how often money lands there.
    #[serde(default)]
    pub pools: Vec<PoolRef>,
    /// Where `rebalance` parks reclaimed surplus (the emergency / rainy-day fund).
    /// Detected at onboarding by pod name; override with `--to`.
    #[serde(default)]
    pub reclaim_pod: Option<String>,
    /// Full discovery detail, per income source.
    pub income_sources: Vec<IncomeSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolRef {
    pub pod_id: String,
    pub name: String,
    /// How many income transfers landed here (confidence / dominance signal).
    pub deposits_seen: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomeSource {
    pub name: String,
    pub classification: Classification,
    /// Detected cadence (absent for irregular sources).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule: Option<PaySchedule>,
    /// Median of recent deposits; the regular floor and windfall threshold.
    /// `null` for irregular sources — nothing reliable to expect.
    pub typical_amount_cents: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    Regular,
    Irregular,
}

impl State {
    pub fn load(path: &str) -> io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    pub fn save(&self, path: &str) -> io::Result<()> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, text)
    }
}
