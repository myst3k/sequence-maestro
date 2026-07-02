//! `maestro discover` — onboarding: detect each income source's pay cadence and
//! typical amount from its deposit history, plus the pool(s) income flows into,
//! then write the state file the engine reads. Re-run to re-onboard.
//!
//! Detection is heuristic but conservative: it classifies a source as `regular`
//! only when its deposits land on a consistent rhythm, otherwise `irregular`
//! (you can't schedule against money that might not show up).

use crate::allocator::FundingStrategy;
use crate::config::Config;
use crate::fetch;
use crate::money::dollars;
use crate::schedule::{last_day_of_month, PaySchedule};
use crate::state::{Classification, IncomeSource, PoolRef, State};
use chrono::{Datelike, NaiveDate, Utc, Weekday};
use sequence_rs::model::account::AccountType;
use sequence_rs::model::transfer::{TransferDirection, TransferParticipantType};

fn median(mut xs: Vec<i64>) -> i64 {
    xs.sort_unstable();
    let n = xs.len();
    if n == 0 {
        0
    } else if n % 2 == 1 {
        xs[n / 2]
    } else {
        (xs[n / 2 - 1] + xs[n / 2]) / 2
    }
}

/// Most common value in `xs` (first-seen wins ties).
fn mode(xs: &[i64]) -> Option<i64> {
    let mut best = None;
    let mut best_count = 0;
    for &x in xs {
        let c = xs.iter().filter(|&&y| y == x).count();
        if c > best_count {
            best_count = c;
            best = Some(x);
        }
    }
    best
}

/// A day in the last 3 days of its month is treated as "last day" (-1).
fn month_day_token(d: NaiveDate) -> i64 {
    let last = last_day_of_month(d.year(), d.month());
    if (last.day() - d.day()) <= 2 {
        -1
    } else {
        d.day() as i64
    }
}

/// Cluster day tokens into pay anchors (`-1` = month-end, else grouped within 4
/// days). Anchor is the MAX day, since weekend-shifting only moves paydays
/// earlier. Returns `(anchor_day, count)` sorted by count descending.
fn cluster_days(tokens: &[i64]) -> Vec<(i32, usize)> {
    let mut out = Vec::new();
    let end = tokens.iter().filter(|&&t| t == -1).count();
    if end > 0 {
        out.push((-1, end));
    }
    let mut pos: Vec<i64> = tokens.iter().filter(|&&t| t > 0).copied().collect();
    pos.sort_unstable();
    let mut i = 0;
    while i < pos.len() {
        let start = i;
        while i + 1 < pos.len() && pos[i + 1] - pos[i] <= 4 {
            i += 1;
        }
        out.push((pos[i] as i32, i - start + 1)); // anchor = max in cluster
        i += 1;
    }
    out.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
    out
}

/// Detect a cadence from deposit dates (ascending). Returns the schedule and
/// classification; `None`/`Irregular` when there's no clear rhythm.
fn detect(dates: &[NaiveDate]) -> (Option<PaySchedule>, Classification) {
    if dates.len() < 3 {
        return (None, Classification::Irregular);
    }
    let n = dates.len();
    let gaps: Vec<i64> = dates.windows(2).map(|w| (w[1] - w[0]).num_days()).collect();
    let avg = gaps.iter().sum::<i64>() as f64 / gaps.len() as f64;
    let var = gaps.iter().map(|&g| (g as f64 - avg).powi(2)).sum::<f64>() / gaps.len() as f64;
    let cv = if avg > 0.0 {
        var.sqrt() / avg
    } else {
        f64::MAX
    };

    // Erratic gaps -> can't schedule against it.
    if cv > 0.6 {
        return (None, Classification::Irregular);
    }

    let tokens: Vec<i64> = dates.iter().map(|&d| month_day_token(d)).collect();
    let clusters = cluster_days(&tokens);

    let sched = if avg <= 9.0 {
        let wd = mode(
            &dates
                .iter()
                .map(|d| d.weekday().num_days_from_monday() as i64)
                .collect::<Vec<_>>(),
        )
        .unwrap_or(4);
        PaySchedule::Weekly {
            weekday: weekday_from(wd),
        }
    } else if avg <= 20.0 {
        // Semi-monthly sticks to <=2 day-anchors; biweekly drifts through the month
        let covered: usize = clusters.iter().take(2).map(|&(_, c)| c).sum();
        if clusters.len() >= 2 && covered as f64 >= 0.70 * n as f64 {
            let mut days: Vec<i32> = clusters.iter().take(2).map(|&(d, _)| d).collect();
            days.sort_unstable();
            PaySchedule::SemiMonthly { days }
        } else {
            PaySchedule::Biweekly {
                anchor: *dates.last().unwrap(),
            }
        }
    } else if avg <= 45.0 {
        PaySchedule::Monthly {
            day: clusters.first().map(|&(d, _)| d).unwrap_or(1),
        }
    } else {
        return (None, Classification::Irregular);
    };
    (Some(sched), Classification::Regular)
}

fn weekday_from(n: i64) -> Weekday {
    match n {
        0 => Weekday::Mon,
        1 => Weekday::Tue,
        2 => Weekday::Wed,
        3 => Weekday::Thu,
        4 => Weekday::Fri,
        5 => Weekday::Sat,
        _ => Weekday::Sun,
    }
}

pub async fn run(cfg: &Config) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = cfg.client();
    let today = Utc::now().date_naive();
    let cutoff = today - chrono::Duration::days(730); // ~2 years for cadence

    let accounts = fetch::accounts(&client).await?;

    let mut sources: Vec<IncomeSource> = Vec::new();
    let mut best_regular: Option<(i64, PaySchedule)> = None; // (typical, schedule)
                                                             // Where income flows: pod id -> (name, times seen). The top one is the pool.
    let mut pool_votes: std::collections::HashMap<String, (String, u32)> = Default::default();

    for s in &accounts {
        if !matches!(s.account_type, AccountType::IncomeSource) {
            continue;
        }
        // every transfer, all pages — a single page truncates 2 years of cadence data
        let transfers = fetch::transfers(&client, &s.id, Default::default()).await?;

        // Trace outbound destinations — that pool is what the engine funds bills from
        for t in &transfers {
            if t.direction == TransferDirection::MoneyIn || !fetch::settled(t) {
                continue;
            }
            if let Some(dest) = &t.destination {
                if dest.participant_type != TransferParticipantType::Pod {
                    continue; // a pool is a pod, not an external account
                }
                if let Some(id) = &dest.id {
                    let e = pool_votes
                        .entry(id.clone())
                        .or_insert((dest.name.clone(), 0));
                    e.1 += 1;
                }
            }
        }

        // Settled deposits only, within the cadence window, ascending by date.
        let mut deposits: Vec<(NaiveDate, i64)> = transfers
            .iter()
            .filter(|t| t.direction == TransferDirection::MoneyIn && fetch::settled(t))
            .filter_map(|t| fetch::parse_date(&t.created_at).map(|d| (d, t.amount_in_cents)))
            .filter(|(d, _)| *d >= cutoff)
            .collect();
        deposits.sort_by_key(|(d, _)| *d);

        let dates: Vec<NaiveDate> = deposits.iter().map(|(d, _)| *d).collect();
        let (schedule, classification) = detect(&dates);

        // Median of the most recent 12 deposits (reflects raises), regular only
        let typical = if classification == Classification::Regular {
            let recent: Vec<i64> = deposits.iter().rev().take(12).map(|(_, a)| *a).collect();
            Some(median(recent))
        } else {
            None
        };

        println!(
            "{:<24} {:>3} deposits  ->  {}  {}",
            s.name.chars().take(24).collect::<String>(),
            dates.len(),
            match &schedule {
                Some(sc) => format!("{sc:?}"),
                None => "IRREGULAR".into(),
            },
            typical.map(dollars).unwrap_or_else(|| "—".into()),
        );

        if let (Classification::Regular, Some(sc), Some(t)) = (classification, &schedule, typical) {
            if best_regular.as_ref().is_none_or(|(bt, _)| t > *bt) {
                best_regular = Some((t, sc.clone()));
            }
        }

        sources.push(IncomeSource {
            name: s.name.clone(),
            classification,
            schedule,
            typical_amount_cents: typical,
        });
    }

    let pay_schedule = match best_regular {
        Some((_, sc)) => sc,
        None => {
            println!("\n[!] no regular income detected — defaulting the pay schedule");
            PaySchedule::default()
        }
    };

    // Rank the pods income flowed into; these are the pool(s).
    let mut pools: Vec<PoolRef> = pool_votes
        .into_iter()
        .map(|(pod_id, (name, deposits_seen))| PoolRef {
            pod_id,
            name,
            deposits_seen,
        })
        .collect();
    pools.sort_by_key(|p| std::cmp::Reverse(p.deposits_seen));

    println!("\nincome pool(s) — where your paychecks land:");
    if pools.is_empty() {
        println!("    [!] none detected — income may flow to an external account");
    }
    for p in &pools {
        println!("    {:<24} ({} deposits)", p.name, p.deposits_seen);
    }

    let funding_strategy = prompt_strategy();

    // rebalance target: pod named like "emergency" (falls back to "savings")
    let reclaim_pod = accounts
        .iter()
        .find(|a| a.name.to_lowercase().contains("emergency"))
        .or_else(|| {
            accounts
                .iter()
                .find(|a| a.name.to_lowercase().contains("savings"))
        })
        .map(|a| a.name.clone());
    println!(
        "reclaim pod (rebalance target): {}",
        reclaim_pod.as_deref().unwrap_or("(none — pass --to)")
    );

    let file = State {
        discovered_at: today,
        pay_schedule: pay_schedule.clone(),
        funding_strategy,
        pools,
        reclaim_pod,
        income_sources: sources,
    };
    file.save(&cfg.state_path)?;

    println!("\npay schedule:     {pay_schedule:?}");
    println!("funding strategy: {funding_strategy:?}");
    println!("wrote {}", cfg.state_path);
    Ok(())
}

/// Ask once which strategy to use when the pool is short. Enter (or no TTY)
/// takes the safe default.
fn prompt_strategy() -> FundingStrategy {
    use std::io::Write;
    print!(
        "\nWhen the pool is short, how should bills be funded?\n  \
         [1] soonest-due — protect nearest due dates (default)\n  \
         [2] strict envelope — fully fund top bills first\n  \
         [3] proportional — split by need\n> "
    );
    std::io::stdout().flush().ok();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return FundingStrategy::default();
    }
    match line.trim() {
        "2" => FundingStrategy::StrictEnvelope,
        "3" => FundingStrategy::Proportional,
        _ => FundingStrategy::SoonestDue,
    }
}
