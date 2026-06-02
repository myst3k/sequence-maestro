//! `maestro simulate --deposit <DOLLARS> --date <YYYY-MM-DD>` — reads REAL pod
//! balances, pretends today is `--date`, injects the deposit, prints the funding
//! plan. Moves nothing — pure read-only what-if.

use colored::Colorize;

use crate::config::Config;
use crate::engine::{assess_with, render, SimInput};
use crate::money::dollars;

pub async fn run(
    cfg: &Config,
    deposit_dollars: f64,
    date: chrono::NaiveDate,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let deposit_cents = (deposit_dollars * 100.0).round() as i64;
    let report = assess_with(
        cfg,
        SimInput {
            today: Some(date),
            deposit_cents,
        },
    )
    .await?;

    let (sched, _) = cfg.pay_schedule();
    let pool = report
        .names
        .get(&report.pool_pod_id)
        .map(String::as_str)
        .unwrap_or(&report.pool_pod_id);

    println!(
        "{}  [{}]",
        format!(
            "SIMULATION — deposit {} on {}",
            dollars(deposit_cents),
            date
        )
        .bold(),
        "WHAT-IF — moves nothing".green(),
    );
    println!(
        "pay schedule: {sched:?}   strategy: {:?}",
        cfg.funding_strategy()
    );
    println!(
        "pool {} after deposit: {}   buffer: {}%\n",
        pool,
        dollars(report.pool_balance),
        cfg.buffer_pct,
    );

    print!("{}", render(&report));
    Ok(())
}
