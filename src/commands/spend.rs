//! `maestro spend [filter] [--days N] [--vs-budget]` — what a pod actually spends:
//! its money OUT, which the API reports through two functions — card transactions
//! (debit) and outgoing transfers (ACH). Inter-pod moves don't count; that's money
//! staying in your pods, not spend. Read-only. `--vs-budget` compares real monthly
//! spend to the pod's declared amount (from its name).

use chrono::{Duration, Utc};
use colored::Colorize;
use sequence_rs::model::account::{AccountSummary, AccountType};
use sequence_rs::model::transaction::{CardTransactionSubtype, Transaction};
use sequence_rs::model::transfer::{Transfer, TransferDirection};
use sequence_rs::prelude::*;
use sequence_rs::{
    ListAccountTransfersParams, ListAccountsParams, ListCardTransactionsParams, Sequence,
};

use crate::cards::{
    ach_outflow_cents, declared_monthly_cents, monthly_run_rate_cents, native_from_monthly_cents,
    net_spend_cents, round_up_to_dollar,
};
use crate::config::Config;
use crate::derive::{self, Frequency, Parsed};
use crate::money::dollars;

/// One outflow row, normalized across card + ACH for display.
struct Entry {
    date: String,
    /// Positive = money out; negative = a card refund.
    cents: i64,
    label: String,
    channel: &'static str,
}

/// A pod's spend over the window, split by channel.
struct PodSpend<'a> {
    pod: &'a AccountSummary,
    card_cents: i64,
    ach_cents: i64,
    entries: Vec<Entry>,
}

impl PodSpend<'_> {
    fn total(&self) -> i64 {
        self.card_cents + self.ach_cents
    }
}

pub async fn run(
    cfg: &Config,
    filter: Option<&str>,
    days: u32,
    vs_budget: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = cfg.client();
    let page = client
        .accounts(&ListAccountsParams {
            page_size: Some(100),
            ..Default::default()
        })
        .await?;
    let needle = filter.unwrap_or("").to_lowercase();
    let pods: Vec<&AccountSummary> = page
        .items
        .iter()
        .filter(|a| a.account_type == AccountType::Pod && a.name.to_lowercase().contains(&needle))
        .collect();

    // Both spend channels in the window, fanned out per pod (concurrent).
    let from = (Utc::now() - Duration::days(days as i64)).to_rfc3339();
    let cref = &client;
    let fetched = futures::future::join_all(pods.iter().map(|a| {
        let pod_id = a.id.clone();
        let from = from.clone();
        async move {
            (
                fetch_card(cref, &pod_id, &from).await,
                fetch_ach(cref, &pod_id, &from).await,
            )
        }
    }))
    .await;

    let mut spends: Vec<PodSpend> = pods
        .into_iter()
        .zip(fetched)
        .map(|(pod, (card, ach))| build(pod, card, ach))
        .filter(|s| !s.entries.is_empty())
        .collect();

    if spends.is_empty() {
        println!("no spend in the last {days}d for the matched pods.");
        return Ok(());
    }
    spends.sort_by_key(|s| std::cmp::Reverse(s.total())); // biggest spenders first

    if vs_budget {
        report_vs_budget(cfg, &spends, days);
    } else {
        report_spend(&spends, days);
    }
    Ok(())
}

/// Combine a pod's card + ACH rows into one channel-tagged spend picture.
fn build(pod: &AccountSummary, card: Vec<Transaction>, ach: Vec<Transfer>) -> PodSpend<'_> {
    let card_cents = net_spend_cents(&card);
    let ach_cents = ach_outflow_cents(&ach);
    let mut entries = Vec::new();
    for t in &card {
        let refund = matches!(t.subtype, CardTransactionSubtype::Refund);
        let label = t.description.split(" | ").next().unwrap_or(&t.description);
        entries.push(Entry {
            date: t.created_at.get(..10).unwrap_or("").to_string(),
            cents: if refund {
                -t.amount_in_cents
            } else {
                t.amount_in_cents
            },
            label: label.to_string(),
            channel: "card",
        });
    }
    for t in &ach {
        if t.direction != TransferDirection::MoneyOut {
            continue;
        }
        let dest = t.destination.as_ref().map_or("—", |d| d.name.as_str());
        let label = dest.split(" | ").next().unwrap_or(dest);
        entries.push(Entry {
            date: t.created_at.get(..10).unwrap_or("").to_string(),
            cents: t.amount_in_cents,
            label: label.to_string(),
            channel: "ACH",
        });
    }
    entries.sort_by(|a, b| b.date.cmp(&a.date));
    PodSpend {
        pod,
        card_cents,
        ach_cents,
        entries,
    }
}

async fn fetch_card(client: &Sequence, pod_id: &str, from: &str) -> Vec<Transaction> {
    let id = pod_id.to_string();
    let mut all = Vec::new();
    let mut p = 1u32;
    loop {
        let params = ListCardTransactionsParams {
            from: Some(from.to_string()),
            page: Some(p),
            page_size: Some(100),
            ..Default::default()
        };
        match client.card_transactions(&id, &params).await {
            Ok(r) => {
                let more = r.pagination.has_next_page && !r.items.is_empty();
                all.extend(r.items);
                if !more {
                    break;
                }
                p += 1;
            }
            Err(_) => break,
        }
    }
    all
}

async fn fetch_ach(client: &Sequence, pod_id: &str, from: &str) -> Vec<Transfer> {
    let id = pod_id.to_string();
    let mut all = Vec::new();
    let mut p = 1u32;
    loop {
        let params = ListAccountTransfersParams {
            direction: Some(TransferDirection::MoneyOut),
            from: Some(from.to_string()),
            page: Some(p),
            page_size: Some(100),
            ..Default::default()
        };
        match client.account_transfers(&id, &params).await {
            Ok(r) => {
                let more = r.pagination.has_next_page && !r.items.is_empty();
                all.extend(r.items);
                if !more {
                    break;
                }
                p += 1;
            }
            Err(_) => break,
        }
    }
    all
}

fn report_spend(spends: &[PodSpend], days: u32) {
    for s in spends {
        let header = format!(
            "=== {}  ·  spend ({days}d): {}   (card {} · ACH {}) ===",
            s.pod.name,
            dollars(s.total()),
            dollars(s.card_cents),
            dollars(s.ach_cents),
        );
        println!("\n{}", header.cyan().bold());
        for e in s.entries.iter().take(15) {
            let amt = format!("{:>9}", dollars(e.cents.abs()));
            let (tag, amt_c) = if e.cents < 0 {
                ("refund".green(), amt.green())
            } else if e.channel == "ACH" {
                ("ACH   ".yellow(), amt.normal())
            } else {
                ("card  ".normal(), amt.normal())
            };
            println!("  {}  {tag} {amt_c}  {}", e.date.dimmed(), e.label);
        }
        if s.entries.len() > 15 {
            println!("  … {} more", s.entries.len() - 15);
        }
    }
}

fn report_vs_budget(cfg: &Config, spends: &[PodSpend], days: u32) {
    // Paydays per month, from the discovered schedule (per-paycheck top-ups need it).
    let (sched, _) = cfg.pay_schedule();
    let today = Utc::now().date_naive();
    let ppm = sched
        .paydays_in(today - Duration::days(30), today)
        .len()
        .max(1) as i64;

    println!(
        "{}\n",
        format!("spend vs. declared budget (monthly, from a {days}d window):").bold()
    );
    for s in spends {
        let actual_m = monthly_run_rate_cents(s.total(), days as i64);
        match declared_amount(&s.pod.name) {
            Some((name, amt, freq)) => {
                // A `topup` pod's declared amount is a per-paycheck CAP (the top-up
                // is gap-aware, funding only up to it), not a fixed bill — so spending
                // under it is normal headroom, and "set the amount to X" doesn't apply.
                let is_cap = matches!(freq, Frequency::Paycheck);
                let decl_m = declared_monthly_cents(amt, &freq, ppm);
                let drift = actual_m - decl_m;
                let drift_s = format!("{:>10}", dollars(drift));
                let (drift_c, flag) = if drift.abs() < 500 {
                    (drift_s.green(), String::new())
                } else if drift > 0 {
                    let msg = if is_cap {
                        "  <-- over the cap"
                    } else {
                        "  <-- spends more than budgeted"
                    };
                    (drift_s.red().bold(), msg.red().to_string())
                } else if is_cap {
                    (drift_s.dimmed(), String::new()) // under a cap is benign headroom
                } else {
                    (
                        drift_s.blue(),
                        "  <-- budgeted more than spent".blue().to_string(),
                    )
                };
                let label = if is_cap { "cap     " } else { "budgeted" };
                println!(
                    "  {:<20} {label} {:>10}/mo   actual {:>10}/mo   drift {drift_c}{flag}",
                    name.chars().take(20).collect::<String>(),
                    dollars(decl_m),
                    dollars(actual_m),
                );
                // The "set the amount to X" suggestion fits a real bill, not a cap.
                if !is_cap && drift.abs() >= 500 {
                    let suggest =
                        round_up_to_dollar(native_from_monthly_cents(actual_m, &freq, ppm));
                    println!(
                        "{}",
                        format!(
                            "      → set the pod amount to ~{} (currently {})",
                            dollars(suggest),
                            dollars(amt),
                        )
                        .dimmed()
                    );
                }
            }
            None => println!(
                "{}",
                format!(
                    "  {:<20} actual {:>10}/mo   (not a budgeted bill)",
                    s.pod.name.chars().take(20).collect::<String>(),
                    dollars(actual_m),
                )
                .dimmed()
            ),
        }
    }
    let footer = if days < 90 {
        format!("(card + ACH spend; rates projected from a {days}d window — use `--days 90` for a steadier figure)")
    } else {
        format!("(card + ACH spend; rates projected from a {days}d window)")
    };
    println!("\n{}", footer.dimmed());
}

/// The declared bill name + amount + frequency from a pod name (new scheme, then old).
fn declared_amount(pod_name: &str) -> Option<(String, i64, Frequency)> {
    if let Some(b) = derive::parse_scheme(pod_name) {
        return Some((b.name, b.amount_cents, b.frequency));
    }
    match derive::parse(pod_name) {
        Parsed::Bill {
            name, amount_cents, ..
        }
        | Parsed::BillNoDueDay { name, amount_cents } => {
            Some((name, amount_cents, Frequency::Month))
        }
        _ => None,
    }
}
