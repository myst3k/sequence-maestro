//! `maestro rebalance [--to POD] [--execute]` — level sinking-fund pods to pace
//! via `engine::assess`: skim those ahead, fill those behind, park the net in POD.
//! Dry run unless `--execute`. MANUAL-ONLY by design: a one-off setup tool, NEVER
//! called from the daemon or any cycle and never to be automated.

use crate::config::Config;
use crate::engine::{assess, rebalance_plan};
use crate::money::dollars as d;

pub async fn run(
    cfg: &Config,
    to: Option<&str>,
    execute: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Same read-only assessment `run` uses — guarantees identical numbers.
    let report = assess(cfg).await?;

    let to = to
        .map(String::from)
        .or_else(|| cfg.state_file().and_then(|f| f.reclaim_pod))
        .ok_or("no reclaim pod set — run `discover` or pass --to <pod>")?;
    let hub_id = report
        .names
        .iter()
        .find(|(_, n)| n.as_str() == to)
        .map(|(id, _)| id.clone())
        .ok_or_else(|| format!("destination pod '{to}' not found"))?;

    // Same computation `run`'s summary shows — one source of truth (engine).
    let plan = rebalance_plan(&report);
    let skims = &plan.skims;
    let fills = &plan.fills;
    let boundary = plan.boundary;
    let total_skim = plan.skim_total();
    let total_fill = plan.fill_total();

    println!(
        "REBALANCE → {to}   [{}]",
        if execute {
            "EXECUTING — moves money"
        } else {
            "DRY RUN — moves nothing"
        }
    );
    println!("\nskim ahead → {to}:");
    for (n, _, amt) in skims {
        println!(
            "  {:<22} {:>9}",
            n.chars().take(22).collect::<String>(),
            d(*amt)
        );
    }
    println!("  ── total skim {}", d(total_skim));
    println!("\nfill behind ← {to}:");
    for (n, _, amt) in fills {
        println!(
            "  {:<22} {:>9}",
            n.chars().take(22).collect::<String>(),
            d(*amt)
        );
    }
    println!("  ── total fill {}", d(total_fill));
    println!("\nnet into {to}: {}", d(total_skim - total_fill));
    if boundary > 0 {
        println!("({boundary} pod(s) skipped — at a cycle boundary, holding for a just-due bill)");
    }

    if execute {
        let client = cfg.client();
        for (n, pod, amt) in skims {
            crate::fetch::create_transfer(&client, pod, &hub_id, *amt).await?;
            tracing::info!(amount_cents = *amt, from = %n, to = %to, "rebalance: skimmed");
            println!("  skimmed {} from {n}", d(*amt));
        }
        for (n, pod, amt) in fills {
            crate::fetch::create_transfer(&client, &hub_id, pod, *amt).await?;
            tracing::info!(amount_cents = *amt, from = %to, to = %n, "rebalance: filled");
            println!("  filled {} into {n}", d(*amt));
        }
        println!("\ndone — pods leveled to pace, net parked in {to}.");
    }
    Ok(())
}
