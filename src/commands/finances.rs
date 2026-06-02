//! `maestro accounts` — read-only financial snapshot: all accounts with balances
//! + APR, plus recent income history.

use crate::config::Config;
use crate::money::dollars;
use sequence_rs::model::account::AccountType;
use sequence_rs::prelude::*;
use sequence_rs::{ListAccountTransfersParams, ListAccountsParams};

pub async fn run(cfg: &Config) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = cfg.client();

    let page = client
        .accounts(&ListAccountsParams {
            page_size: Some(100),
            ..Default::default()
        })
        .await?;

    let mut pods_total = 0i64;
    let mut income_ids = Vec::new();

    // Fetch all account details concurrently (serial was ~30s over ~50 pods).
    let cref = &client;
    let details =
        futures::future::join_all(page.items.iter().map(|s| cref.account(&s.id))).await;

    println!("===== ACCOUNTS =====");
    for (s, acct_res) in page.items.iter().zip(details) {
        let acct = match acct_res {
            Ok(a) => a,
            Err(e) => {
                println!("  {}  [{:?}]  (detail error: {e})", s.name, s.account_type);
                continue;
            }
        };
        let bal = acct.balance.as_ref();
        let balance = bal.and_then(|b| b.balance_in_cents);
        let apr = bal.and_then(|b| b.interest_rate_percentage);
        let bal_str = balance.map(dollars).unwrap_or_else(|| "—".into());
        let apr_str = apr.map(|a| format!("  APR {a:.2}%")).unwrap_or_default();
        let min_str = match (
            bal.and_then(|b| b.next_payment_minimum_in_cents),
            bal.and_then(|b| b.next_payment_due_date.as_deref()),
        ) {
            (Some(m), Some(d)) => format!("  min {} due {}", dollars(m), d.get(..10).unwrap_or("")),
            (Some(m), _) => format!("  min {}", dollars(m)),
            _ => String::new(),
        };
        println!(
            "  [{:?}] {}  =>  {}{}{}",
            s.account_type, s.name, bal_str, apr_str, min_str
        );
        if matches!(s.account_type, AccountType::Pod) {
            pods_total += balance.unwrap_or(0);
        }
        if matches!(s.account_type, AccountType::IncomeSource) {
            income_ids.push((s.id.clone(), s.name.clone()));
        }
    }
    println!("\nTotal across pods: {}", dollars(pods_total));

    println!("\n===== RECENT INCOME (per income source) =====");
    // Income-history fetches run concurrently too (was serial per source).
    let income_tx = futures::future::join_all(income_ids.iter().map(|(id, _)| async move {
        cref.account_transfers(
            id,
            &ListAccountTransfersParams {
                page_size: Some(15),
                ..Default::default()
            },
        )
        .await
    }))
    .await;
    for ((_, name), tx_res) in income_ids.iter().zip(income_tx) {
        println!("-- {name} --");
        let transfers = match tx_res {
            Ok(t) => t,
            Err(e) => {
                println!("   (error: {e})");
                continue;
            }
        };
        for t in transfers.items.iter().take(15) {
            println!(
                "   {}  {:?}  {}",
                t.created_at,
                t.direction,
                dollars(t.amount_in_cents)
            );
        }
    }
    Ok(())
}
