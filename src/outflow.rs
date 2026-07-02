//! Fetch a pod's money OUT — card transactions (debit) + outgoing transfers
//! (ACH `MONEY_OUT`), paginated. Shared by `commands::spend` (which needs the
//! rows for display) and the engine's pending-debit reservation (which needs
//! only the summed total). Inter-pod `INTERNAL` moves are excluded by the
//! `MoneyOut` filter — those aren't spend.
//!
//! Each fetch returns `None` on any API error, so callers can tell "nothing went
//! out" from "we couldn't load it". The reservation logic relies on that: a
//! failed fetch must not be read as a missing debit (that would falsely reserve).

use sequence_rs::model::transaction::Transaction;
use sequence_rs::model::transfer::{Transfer, TransferDirection};
use sequence_rs::prelude::*;
use sequence_rs::{ListAccountTransfersParams, ListCardTransactionsParams, Sequence};

use crate::cards::{ach_outflow_cents, net_spend_cents};

/// Every card transaction for `pod_id` at/after `from` (RFC3339), across pages.
/// `None` if any page request fails (an incomplete list can't be trusted).
pub async fn fetch_card(client: &Sequence, pod_id: &str, from: &str) -> Option<Vec<Transaction>> {
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
        let r = client.card_transactions(&id, &params).await.ok()?;
        let more = r.pagination.has_next_page && !r.items.is_empty();
        all.extend(r.items);
        if !more {
            break;
        }
        p += 1;
    }
    Some(all)
}

/// Every outgoing (`MONEY_OUT`) transfer for `pod_id` at/after `from`, across
/// pages. `None` if any page request fails.
pub async fn fetch_ach(client: &Sequence, pod_id: &str, from: &str) -> Option<Vec<Transfer>> {
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
        let r = client.account_transfers(&id, &params).await.ok()?;
        let more = r.pagination.has_next_page && !r.items.is_empty();
        all.extend(r.items);
        if !more {
            break;
        }
        p += 1;
    }
    Some(all)
}

/// Total money OUT of `pod_id` at/after `from`: net card spend + ACH outflow.
/// `None` if either listing couldn't be loaded.
pub async fn outflow_since_cents(client: &Sequence, pod_id: &str, from: &str) -> Option<i64> {
    let card = fetch_card(client, pod_id, from).await?;
    let ach = fetch_ach(client, pod_id, from).await?;
    Some(net_spend_cents(&card) + ach_outflow_cents(&ach))
}
