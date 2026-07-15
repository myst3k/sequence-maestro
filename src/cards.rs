//! Spend aggregation: a pod's total money OUT, summed across the two functions the
//! API reports it through — card transactions (debit) and outgoing transfers (ACH).
//! Also normalizes a declared bill amount to monthly (and back) for right-sizing.
//! Pure (no I/O) so the money math is unit-tested here; fetching lives in
//! `commands::spend`.

use sequence_rs::model::transaction::Transaction;
use sequence_rs::model::transfer::{Transfer, TransferDirection, TransferStatus};

use crate::derive::Frequency;

/// Net CARD spend in cents from *completed* transactions: purchases (`MONEY_OUT`)
/// minus refunds (`MONEY_IN`). Pending/declined/failed don't count — money that
/// didn't actually move. Negative in a refund-heavy window.
pub fn net_spend_cents(txns: &[Transaction]) -> i64 {
    txns.iter()
        .filter(|t| t.status == TransferStatus::Complete)
        .map(|t| match t.direction {
            TransferDirection::MoneyOut => t.amount_in_cents,
            _ => -t.amount_in_cents,
        })
        .sum()
}

/// Total external ACH/transfer outflow in cents from *completed* transfers leaving
/// the pod to the outside world (`MONEY_OUT`). Excludes inter-pod (`INTERNAL`) moves
/// and inflows, and crucially anything not `Complete` — a bounced/returned/pending
/// payment never left the pod, so counting it would overstate spend and fool the
/// debit-cleared check into thinking a bill was paid when it failed.
pub fn ach_outflow_cents(transfers: &[Transfer]) -> i64 {
    transfers
        .iter()
        .filter(|t| {
            t.direction == TransferDirection::MoneyOut && t.status == TransferStatus::Complete
        })
        .map(|t| t.amount_in_cents)
        .sum()
}

/// Project a window's net spend to a monthly run-rate (avg 30.44 days/month).
pub fn monthly_run_rate_cents(net_cents: i64, window_days: i64) -> i64 {
    if window_days <= 0 {
        return net_cents;
    }
    ((net_cents as i128 * 3044) / (window_days as i128 * 100)) as i64
}

/// Monthly-equivalent cost of a declared bill. `paydays_per_month` matters only
/// for per-paycheck top-ups.
pub fn declared_monthly_cents(amount_cents: i64, freq: &Frequency, paydays_per_month: i64) -> i64 {
    match freq {
        Frequency::Month => amount_cents,
        Frequency::Quarter => amount_cents / 3,
        Frequency::Year => amount_cents / 12,
        Frequency::Drawdown { period_months, .. } if *period_months > 0 => {
            amount_cents / *period_months as i64
        }
        Frequency::Drawdown { .. } => amount_cents,
        Frequency::Paycheck => amount_cents * paydays_per_month.max(1),
        // a kept level refills at most its own amount in a month
        Frequency::Hold => amount_cents,
    }
}

/// Inverse of [`declared_monthly_cents`]: a monthly figure expressed back in the
/// bill's native unit — i.e. what to put in the pod name.
pub fn native_from_monthly_cents(
    monthly_cents: i64,
    freq: &Frequency,
    paydays_per_month: i64,
) -> i64 {
    match freq {
        Frequency::Month => monthly_cents,
        Frequency::Quarter => monthly_cents * 3,
        Frequency::Year => monthly_cents * 12,
        Frequency::Drawdown { period_months, .. } => monthly_cents * *period_months as i64,
        Frequency::Paycheck => monthly_cents / paydays_per_month.max(1),
        Frequency::Hold => monthly_cents,
    }
}

/// Round a (positive) cents amount UP to a whole dollar — for a clean suggested
/// pod amount that never under-budgets the bill.
pub fn round_up_to_dollar(cents: i64) -> i64 {
    ((cents + 99) / 100) * 100
}

/// Cents of a bill's scheduled debit not yet cleared this cycle, given how much
/// has actually gone out since the due date. If at least half the bill has left,
/// treat the debit as cleared (0); otherwise reserve the remainder so the pod
/// isn't mistaken for "full" before a slow/weekend debit hits.
fn uncleared_debit_cents(bill_amount: i64, seen_outflow: i64) -> i64 {
    if seen_outflow * 2 >= bill_amount {
        0
    } else {
        (bill_amount - seen_outflow).max(0)
    }
}

/// Cents to actually reserve in a pod: the uncleared debit, but only the balance
/// held ABOVE the current paced `target`. Money up to the target is what this cycle
/// is supposed to hold for the bill — counting it as committed would double the
/// demand (target + reservation) and, on the due date itself, make the pod ask to
/// be funded twice over. Only the surplus is a just-rolled cycle's payment still
/// sitting there before its debit clears.
pub fn reserved_cents(bill_amount: i64, seen_outflow: i64, balance: i64, target: i64) -> i64 {
    let surplus = (balance - target).max(0);
    uncleared_debit_cents(bill_amount, seen_outflow).min(surplus)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use sequence_rs::model::transaction::{CardTransactionSubtype, CardType};
    use sequence_rs::model::transfer::{
        TransferAccountRef, TransferOrigin, TransferParticipantType, TransferStatus,
    };

    fn txn(amount: i64, dir: TransferDirection, sub: CardTransactionSubtype) -> Transaction {
        Transaction {
            id: "t".into(),
            card_id: "c".into(),
            card_type: CardType::DebitCard,
            account: TransferAccountRef {
                id: Some("p".into()),
                name: "Pod".into(),
                participant_type: TransferParticipantType::Pod,
                is_deleted: Some(false),
            },
            direction: dir,
            subtype: sub,
            status: TransferStatus::Complete,
            amount_in_cents: amount,
            description: "merchant".into(),
            created_at: "2026-06-01T00:00:00Z".into(),
            completed_at: "2026-06-01T00:00:00Z".into(),
        }
    }

    fn transfer(amount: i64, dir: TransferDirection) -> Transfer {
        Transfer {
            id: "x".into(),
            amount_in_cents: amount,
            direction: dir,
            origin: TransferOrigin::User,
            source: None,
            destination: None,
            status: TransferStatus::Complete,
            rule_id: None,
            rule_execution_id: None,
            error_code: None,
            created_at: "2026-06-01T00:00:00Z".into(),
            completed_at: None,
        }
    }

    #[test]
    fn net_spend_subtracts_refunds() {
        let txns = vec![
            txn(
                5000,
                TransferDirection::MoneyOut,
                CardTransactionSubtype::Purchase,
            ),
            txn(
                1500,
                TransferDirection::MoneyOut,
                CardTransactionSubtype::Purchase,
            ),
            txn(
                1000,
                TransferDirection::MoneyIn,
                CardTransactionSubtype::Refund,
            ),
        ];
        assert_eq!(net_spend_cents(&txns), 5500); // 6500 purchases − 1000 refund
    }

    #[test]
    fn outflow_excludes_failed_or_pending() {
        // a bounced ACH: attempted but never settled — must not count as spend OR
        // as the debit clearing (a retry often lands higher, with a return fee)
        let mut bounced = transfer(40_000, TransferDirection::MoneyOut);
        bounced.status = TransferStatus::Error;
        let settled = transfer(41_000, TransferDirection::MoneyOut); // Complete
        assert_eq!(ach_outflow_cents(&[bounced, settled]), 41_000);

        let mut pending = txn(
            5_000,
            TransferDirection::MoneyOut,
            CardTransactionSubtype::Purchase,
        );
        pending.status = TransferStatus::Pending;
        assert_eq!(net_spend_cents(&[pending]), 0);
    }

    #[test]
    fn ach_outflow_counts_only_external_money_out() {
        let ts = vec![
            transfer(50_000, TransferDirection::MoneyOut), // external payment
            transfer(20_000, TransferDirection::Internal), // inter-pod move — excluded
            transfer(10_000, TransferDirection::MoneyIn),  // inflow — excluded
            transfer(30_000, TransferDirection::MoneyOut),
        ];
        assert_eq!(ach_outflow_cents(&ts), 80_000);
    }

    #[test]
    fn monthly_run_rate_projects_window() {
        assert_eq!(monthly_run_rate_cents(30_000, 30), 30_440);
        assert_eq!(monthly_run_rate_cents(30_000, 90), 10_146);
        assert_eq!(monthly_run_rate_cents(100, 0), 100);
    }

    #[test]
    fn declared_monthly_by_frequency() {
        let ppm = 2;
        assert_eq!(
            declared_monthly_cents(12_000, &Frequency::Month, ppm),
            12_000
        );
        assert_eq!(declared_monthly_cents(12_000, &Frequency::Year, ppm), 1_000);
        assert_eq!(
            declared_monthly_cents(9_000, &Frequency::Quarter, ppm),
            3_000
        );
        assert_eq!(
            declared_monthly_cents(25_000, &Frequency::Paycheck, ppm),
            50_000
        );
        let dd = Frequency::Drawdown {
            target: NaiveDate::from_ymd_opt(2026, 12, 1).unwrap(),
            period_months: 6,
        };
        assert_eq!(declared_monthly_cents(60_000, &dd, ppm), 10_000);
    }

    #[test]
    fn native_inverts_monthly() {
        let ppm = 2;
        assert_eq!(
            native_from_monthly_cents(1_000, &Frequency::Year, ppm),
            12_000
        );
        assert_eq!(
            native_from_monthly_cents(50_000, &Frequency::Paycheck, ppm),
            25_000
        );
        assert_eq!(
            native_from_monthly_cents(12_000, &Frequency::Month, ppm),
            12_000
        );
    }

    #[test]
    fn round_up_to_whole_dollar() {
        assert_eq!(round_up_to_dollar(58952), 59000); // $589.52 -> $590.00
        assert_eq!(round_up_to_dollar(2869), 2900); // $28.69 -> $29.00
        assert_eq!(round_up_to_dollar(59000), 59000); // already whole
    }

    #[test]
    fn uncleared_reserves_until_debit_seen() {
        assert_eq!(uncleared_debit_cents(50_000, 0), 50_000); // nothing out -> reserve all
        assert_eq!(uncleared_debit_cents(50_000, 25_000), 0); // half out -> cleared
        assert_eq!(uncleared_debit_cents(50_000, 49_000), 0); // fully out -> cleared
        assert_eq!(uncleared_debit_cents(50_000, 10_000), 40_000); // partial -> reserve rest
    }

    #[test]
    fn reserved_only_covers_surplus_over_target() {
        // money up to the target is this cycle's — not reserved (else need doubles)
        assert_eq!(reserved_cents(50_000, 0, 50_000, 50_000), 0); // at target (due day)
        assert_eq!(reserved_cents(50_000, 0, 45_000, 50_000), 0); // below target
                                                                  // surplus above a low (just-rolled) target is the pending debit -> reserve it
        assert_eq!(reserved_cents(50_000, 0, 50_000, 0), 50_000);
        assert_eq!(reserved_cents(50_000, 0, 50_000, 8_000), 42_000);
        // debit already cleared (>= half seen) -> nothing, regardless of surplus
        assert_eq!(reserved_cents(50_000, 49_000, 50_000, 0), 0);
        // empty pod -> nothing to reserve
        assert_eq!(reserved_cents(50_000, 0, 0, 0), 0);
    }
}
