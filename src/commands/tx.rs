//! `maestro tx <filter>` — recent transfers for every pod whose name contains
//! `<filter>` (case-insensitive). Read-only; for checking what's actually flowing
//! into your pods vs. what the engine thinks should.

use crate::config::Config;
use crate::fetch;
use crate::money::dollars;
use sequence_rs::model::account::AccountType;
use sequence_rs::model::transfer::TransferDirection;

pub async fn run(
    cfg: &Config,
    filter: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = cfg.client();
    let accounts = fetch::accounts(&client).await?;
    let needle = filter.to_lowercase();
    let pods: Vec<_> = accounts
        .iter()
        .filter(|a| a.account_type == AccountType::Pod && a.name.to_lowercase().contains(&needle))
        .collect();

    // hardened balances (no phantom $0), then recent transfers per pod concurrently
    let ids: Vec<String> = pods.iter().map(|a| a.id.clone()).collect();
    let balances = fetch::balances(&client, &ids).await?;
    let cref = &client;
    let fetched = futures::future::join_all(
        pods.iter()
            .map(|a| fetch::recent_transfers(cref, &a.id, 20)),
    )
    .await;

    for (a, tx) in pods.iter().zip(fetched) {
        let bal = balances.get(&a.id).copied().unwrap_or(0);
        println!("\n=== {}  (balance {}) ===", a.name, dollars(bal));
        let tx = match tx {
            Ok(t) => t,
            Err(e) => {
                println!("  (transfers error: {e})");
                continue;
            }
        };
        for t in &tx {
            let arrow = match t.direction {
                TransferDirection::MoneyIn => "IN  ←",
                TransferDirection::MoneyOut => "OUT →",
                TransferDirection::Internal => "in  ←",
            };
            let other = match t.direction {
                TransferDirection::MoneyOut => t.destination.as_ref(),
                _ => t.source.as_ref(),
            }
            .map(|p| p.name.as_str())
            .unwrap_or("—");
            println!(
                "  {}  {arrow} {:>9}  {}",
                t.created_at.get(..10).unwrap_or(""),
                dollars(t.amount_in_cents),
                other,
            );
        }
    }
    Ok(())
}
