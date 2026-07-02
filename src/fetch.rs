//! One hardened data-access layer over the Sequence API: every maestro read goes
//! through here, so pagination, burst-throttling, retries, and status semantics
//! are decided once. (Four balance fetchers and six transfer fetchers once lived
//! across the commands, each missing a different hard-won lesson.)

use std::collections::HashMap;

use chrono::NaiveDate;
use futures::TryStreamExt;
use sequence_rs::model::account::{Account, AccountSummary};
use sequence_rs::model::transaction::Transaction;
use sequence_rs::model::transfer::{
    CreateTransferRequest, Transfer, TransferDirection, TransferStatus,
};
use sequence_rs::prelude::*;
use sequence_rs::{
    ListAccountTransfersParams, ListAccountsParams, ListCardTransactionsParams, Sequence,
};

use crate::cards::{ach_outflow_cents, net_spend_cents};

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Milliseconds to stagger per-account detail GETs. Firing ~50 at once stampedes
/// the API — calls fail and used to be read as $0, flip-flopping decisions run to
/// run. A stagger keeps only a handful in flight, so rate limiting + retries hold.
const DETAIL_STAGGER_MS: u64 = 100;

/// Parse the leading `YYYY-MM-DD` of an API timestamp.
pub fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok()
}

/// A transfer that actually moved money. Everything else — pending, failed,
/// bounced, cancelled — is an intention, not spend or income.
pub fn settled(t: &Transfer) -> bool {
    t.status == TransferStatus::Complete
}

/// Every account, across all pages (never truncates at one page).
pub async fn accounts(client: &Sequence) -> Result<Vec<AccountSummary>, Error> {
    let params = ListAccountsParams::default();
    Ok(client.accounts_stream(&params).try_collect().await?)
}

/// Full details (balance, APR, liability info) for each id, in order. Staggered
/// launch, stragglers retried once serially, and a detail we still can't read is
/// a hard error — acting on a phantom $0 moves real money.
pub async fn account_details(client: &Sequence, ids: &[String]) -> Result<Vec<Account>, Error> {
    let fetched = futures::future::join_all(ids.iter().enumerate().map(|(i, id)| async move {
        tokio::time::sleep(std::time::Duration::from_millis(
            i as u64 * DETAIL_STAGGER_MS,
        ))
        .await;
        client.account(id).await
    }))
    .await;
    let mut out = Vec::with_capacity(ids.len());
    for (id, res) in ids.iter().zip(fetched) {
        match res {
            Ok(a) => out.push(a),
            Err(_) => out.push(
                client
                    .account(id)
                    .await
                    .map_err(|e| format!("could not read account {id}: {e}"))?,
            ),
        }
    }
    Ok(out)
}

/// Per-account balances in cents. A missing balance field is $0 (an empty pod);
/// an unreadable account is a hard error, never a silent $0.
pub async fn balances(client: &Sequence, ids: &[String]) -> Result<HashMap<String, i64>, Error> {
    let details = account_details(client, ids).await?;
    Ok(ids
        .iter()
        .cloned()
        .zip(
            details
                .into_iter()
                .map(|a| a.balance.and_then(|b| b.balance_in_cents).unwrap_or(0)),
        )
        .collect())
}

/// Every transfer for `account_id` matching `params`, across all pages.
pub async fn transfers(
    client: &Sequence,
    account_id: &str,
    params: ListAccountTransfersParams,
) -> Result<Vec<Transfer>, Error> {
    let id = account_id.to_string();
    Ok(client
        .account_transfers_stream(&id, &params)
        .try_collect()
        .await?)
}

/// One page of the most recent transfers — for display lists.
pub async fn recent_transfers(
    client: &Sequence,
    account_id: &str,
    limit: u32,
) -> Result<Vec<Transfer>, Error> {
    let page = client
        .account_transfers(
            &account_id.to_string(),
            &ListAccountTransfersParams {
                page_size: Some(limit),
                ..Default::default()
            },
        )
        .await?;
    Ok(page.items)
}

/// Every card transaction for `pod_id` at/after `from` (RFC3339), across pages.
pub async fn card_transactions_since(
    client: &Sequence,
    pod_id: &str,
    from: &str,
) -> Result<Vec<Transaction>, Error> {
    let id = pod_id.to_string();
    let params = ListCardTransactionsParams {
        from: Some(from.to_string()),
        ..Default::default()
    };
    Ok(client
        .card_transactions_stream(&id, &params)
        .try_collect()
        .await?)
}

/// Every outgoing (`MONEY_OUT`) transfer for `pod_id` at/after `from`, across
/// pages. Inter-pod `INTERNAL` moves are excluded by the filter — those aren't spend.
pub async fn outgoing_transfers_since(
    client: &Sequence,
    pod_id: &str,
    from: &str,
) -> Result<Vec<Transfer>, Error> {
    transfers(
        client,
        pod_id,
        ListAccountTransfersParams {
            direction: Some(TransferDirection::MoneyOut),
            from: Some(from.to_string()),
            ..Default::default()
        },
    )
    .await
}

/// The ONLY money-moving call in maestro: one transfer, `from` → `to`. The
/// idempotency key is left `None` so the client generates a uuidv7 — a retried
/// request can't double-move money.
pub async fn create_transfer(
    client: &Sequence,
    from: &str,
    to: &str,
    amount_cents: i64,
) -> Result<Transfer, Error> {
    let req = CreateTransferRequest {
        source_account_id: from.to_string(),
        destination_account_id: to.to_string(),
        amount_in_cents: amount_cents,
        description: None,
    };
    Ok(client.create_transfer(&req, None).await?)
}

/// Total money OUT of `pod_id` at/after `from`: net card spend + settled ACH.
/// (The summers in `cards` count only COMPLETE rows.)
pub async fn outflow_since_cents(
    client: &Sequence,
    pod_id: &str,
    from: &str,
) -> Result<i64, Error> {
    Ok(
        net_spend_cents(&card_transactions_since(client, pod_id, from).await?)
            + ach_outflow_cents(&outgoing_transfers_since(client, pod_id, from).await?),
    )
}
