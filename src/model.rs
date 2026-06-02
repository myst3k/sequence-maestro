//! The budget model the allocator plans against: groups of bills. Built in code
//! from the live Sequence pods (see `derive`) — not loaded from a config file.

use crate::derive::Frequency;

#[derive(Debug, Clone)]
pub struct Budget {
    /// Where funds are pulled from (the income/pool pod).
    pub pool_pod_id: String,
    pub categories: Vec<Category>,
}

#[derive(Debug, Clone)]
pub struct Category {
    pub name: String,
    pub pod_id: String,
    pub bills: Vec<Bill>,
}

#[derive(Debug, Clone)]
pub struct Bill {
    pub name: String,
    pub pod_id: String,
    pub amount_cents: i64,
    /// `None` = no due date; fund by even accumulation over the frequency period.
    pub due_day: Option<u32>,
    pub frequency: Frequency,
}
