//! `maestro run` — run one funding cycle against live Sequence data. Reports the
//! plan and, when not a dry run, executes the transfers. Dry run is the default;
//! the `--execute` flag is the only difference.

use colored::Colorize;

use crate::config::Config;
use crate::engine::{render, run_cycle};
use crate::money::dollars;

pub async fn run(cfg: &Config) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let report = run_cycle(cfg).await?;

    let mode = if report.executed {
        "EXECUTED — money moved".red().bold()
    } else {
        "DRY RUN — moves nothing".green()
    };
    let (sched, onboarded) = cfg.pay_schedule();
    let pool = report
        .names
        .get(&report.pool_pod_id)
        .map(String::as_str)
        .unwrap_or(&report.pool_pod_id);

    println!("{}  [{mode}]", format!("MAESTRO — {}", report.today).bold());
    println!(
        "pay schedule: {sched:?}   strategy: {:?}{}",
        cfg.funding_strategy(),
        if onboarded {
            ""
        } else {
            "   [!] no state file — run `discover`"
        }
    );
    println!(
        "pool: {} = {}   buffer: {}%   phase: {:?}\n",
        pool,
        dollars(report.pool_balance),
        cfg.buffer_pct,
        cfg.phase,
    );

    print!("{}", render(&report));
    Ok(())
}
