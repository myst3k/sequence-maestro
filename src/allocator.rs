//! Pure, stateless funding: transfers bringing each bill pod to its point-in-cycle
//! target. A bill accrues evenly across the paydays in its window (count from the
//! declared `PaySchedule`, never hardcoded), 100% by due; `need = target−current`
//! (≥ 0). Per category: one pool→category transfer, then category→bills (nets to 0).

use std::collections::HashMap;

use chrono::{Datelike, Duration, Months, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::derive::Frequency;
use crate::model::{Bill, Budget};
use crate::schedule::{last_day_of_month, PaySchedule};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedTransfer {
    pub from_pod: String,
    pub to_pod: String,
    pub amount_cents: i64,
    pub reason: String,
}

/// How to distribute the pool (chosen once at onboarding). Differ on target
/// level — `SoonestDue`/`Proportional` fund to *pace*, `StrictEnvelope` to
/// *100%* — and, when the pool falls short, who gets rationed first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FundingStrategy {
    /// Protect due dates: in due order, catch every bill up to where it should
    /// be today (its paced target), and no further. Surplus stays in the pool.
    /// The safe default.
    #[default]
    SoonestDue,
    /// Hard priority: in due order, fully fund each bill to 100% before the next
    /// gets a cent. Starves later bills to protect earlier ones.
    StrictEnvelope,
    /// Split the available pool across bills in proportion to each one's need.
    Proportional,
}

/// Next occurrence of `due_day` on or after `today` (clamped to month length).
fn next_due(today: NaiveDate, due_day: u32) -> NaiveDate {
    let clamp = |y: i32, m: u32| {
        let last = last_day_of_month(y, m).day();
        NaiveDate::from_ymd_opt(y, m, due_day.min(last)).unwrap()
    };
    let this = clamp(today.year(), today.month());
    if this >= today {
        return this;
    }
    if today.month() == 12 {
        clamp(today.year() + 1, 1)
    } else {
        clamp(today.year(), today.month() + 1)
    }
}

/// The same day one month earlier (clamped to month length). The start of a
/// monthly bill's accrual window — the previous time it was due.
fn minus_one_month(d: NaiveDate) -> NaiveDate {
    let (y, m) = if d.month() == 1 {
        (d.year() - 1, 12)
    } else {
        (d.year(), d.month() - 1)
    };
    let last = last_day_of_month(y, m).day();
    NaiveDate::from_ymd_opt(y, m, d.day().min(last)).unwrap()
}

/// How much a monthly due-day bill *should* hold as of `today`: it accrues an
/// equal share on each payday in its window (previous due → this due), reaching
/// 100% by the due date.
pub fn target_for(amount_cents: i64, due_day: u32, today: NaiveDate, sched: &PaySchedule) -> i64 {
    let due = next_due(today, due_day);
    let window_start = minus_one_month(due);
    let funding = sched.paydays_in(window_start + Duration::days(1), due);
    let total = funding.len() as i64;
    if total == 0 {
        return amount_cents; // no paydays to spread across — fund it fully
    }
    let occurred = funding.iter().filter(|&&pd| pd <= today).count() as i64;
    amount_cents * occurred / total
}

/// What a bill should hold today — due-day bills accrue toward their date, no-due
/// bills accumulate evenly over their frequency period. The one funding rule.
pub fn bill_target(bill: &Bill, today: NaiveDate, sched: &PaySchedule) -> i64 {
    match bill.due_day {
        Some(d) => target_for(bill.amount_cents, d, today, sched),
        None => match &bill.frequency {
            // flat per-paycheck amount; satisfied-ness comes from the ledger
            Frequency::Paycheck => bill.amount_cents,
            Frequency::Drawdown {
                target,
                period_months,
            } => drawdown_slice(bill.amount_cents, *target, *period_months, today, sched),
            _ => target_accumulate(bill.amount_cents, &bill.frequency, today, sched),
        },
    }
}

/// Next drawdown occurrence on/after `today` (rolling the anchor forward by period).
fn next_drawdown(anchor: NaiveDate, period_months: u32, today: NaiveDate) -> NaiveDate {
    if period_months == 0 {
        return anchor;
    }
    let mut d = anchor;
    while d < today {
        d = d
            .checked_add_months(Months::new(period_months))
            .unwrap_or(d);
    }
    d
}

/// The paychecks in the current drawdown window: (total in the period, how many
/// have occurred by `today`).
fn drawdown_window(
    anchor: NaiveDate,
    period_months: u32,
    today: NaiveDate,
    sched: &PaySchedule,
) -> (i64, i64) {
    let next = next_drawdown(anchor, period_months, today);
    let prev = next
        .checked_sub_months(Months::new(period_months))
        .unwrap_or(next);
    let funding = sched.paydays_in(prev + Duration::days(1), next);
    let total = funding.len() as i64;
    let elapsed = funding.iter().filter(|&&pd| pd <= today).count() as i64;
    (total, elapsed)
}

/// A drawdown's flat per-paycheck contribution: `amount` spread evenly over the
/// paychecks in one period. **Constant** — it does not catch up if the pod falls
/// behind; the engine notes a shortfall instead and the user can top up.
pub fn drawdown_slice(
    amount: i64,
    anchor: NaiveDate,
    period_months: u32,
    today: NaiveDate,
    sched: &PaySchedule,
) -> i64 {
    let (total, _) = drawdown_window(anchor, period_months, today, sched);
    if total == 0 {
        amount
    } else {
        amount / total
    }
}

/// Where a drawdown pod *would* be by `today` if funded evenly from the period's
/// start — used only to report how far behind/ahead the pod is (not to fund it).
pub fn drawdown_on_track(
    amount: i64,
    anchor: NaiveDate,
    period_months: u32,
    today: NaiveDate,
    sched: &PaySchedule,
) -> i64 {
    let (total, elapsed) = drawdown_window(anchor, period_months, today, sched);
    if total == 0 {
        return amount;
    }
    amount * elapsed / total
}

fn target_cents(bill: &Bill, today: NaiveDate, sched: &PaySchedule) -> i64 {
    bill_target(bill, today, sched)
}

/// The date a bill is ordered by when rationing: its next due date, or the end of
/// its frequency period for no-due bills.
fn sort_due(bill: &Bill, today: NaiveDate) -> NaiveDate {
    match bill.due_day {
        Some(d) => next_due(today, d),
        None => match &bill.frequency {
            Frequency::Paycheck => today, // due now — fund it this payday
            Frequency::Drawdown {
                target,
                period_months,
            } => next_drawdown(*target, *period_months, today),
            _ => period_bounds(&bill.frequency, today).1,
        },
    }
}

/// Calendar bounds of the current frequency period containing `today`.
fn period_bounds(freq: &Frequency, today: NaiveDate) -> (NaiveDate, NaiveDate) {
    match freq {
        Frequency::Month => (
            NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap(),
            last_day_of_month(today.year(), today.month()),
        ),
        Frequency::Quarter => {
            let qm = ((today.month() - 1) / 3) * 3 + 1;
            (
                NaiveDate::from_ymd_opt(today.year(), qm, 1).unwrap(),
                last_day_of_month(today.year(), qm + 2),
            )
        }
        Frequency::Year => (
            NaiveDate::from_ymd_opt(today.year(), 1, 1).unwrap(),
            NaiveDate::from_ymd_opt(today.year(), 12, 31).unwrap(),
        ),
        // Handled directly in bill_target/sort_due, not via calendar periods.
        Frequency::Paycheck | Frequency::Drawdown { .. } => (today, today),
    }
}

/// Target for a bill with no due date: accumulate `amount` evenly across the
/// paydays of its frequency period (calendar-anchored), per the schedule.
pub fn target_accumulate(
    amount_cents: i64,
    freq: &Frequency,
    today: NaiveDate,
    sched: &PaySchedule,
) -> i64 {
    let (start, end) = period_bounds(freq, today);
    let per_period = sched.paydays_through(start, end) as i64;
    if per_period == 0 {
        return amount_cents;
    }
    let elapsed = (sched.paydays_through(start, today) as i64).min(per_period);
    amount_cents * elapsed / per_period
}

/// One bill's funding facts for this cycle.
struct Item<'a> {
    cat_pod: &'a str,
    bill: &'a Bill,
    current: i64,
    today_target: i64,
    full: i64,
    due: NaiveDate,
    give: i64,
}

/// Compute the funding plan: pool → category → bills, bounded by the pool's
/// actual balance. When the pool can't cover every need, `strategy` decides who
/// gets funded. `balances` maps pod id → current cents (missing = 0), and must
/// include the pool pod.
pub fn plan(
    budget: &Budget,
    balances: &HashMap<String, i64>,
    today: NaiveDate,
    sched: &PaySchedule,
    strategy: FundingStrategy,
) -> Vec<PlannedTransfer> {
    let mut items: Vec<Item> = Vec::new();
    for cat in &budget.categories {
        for bill in &cat.bills {
            let current = balances.get(&bill.pod_id).copied().unwrap_or(0);
            items.push(Item {
                cat_pod: &cat.pod_id,
                bill,
                current,
                today_target: target_cents(bill, today, sched),
                full: bill.amount_cents,
                due: sort_due(bill, today),
                give: 0,
            });
        }
    }
    let available = balances.get(&budget.pool_pod_id).copied().unwrap_or(0);
    allocate(&mut items, available, strategy);

    let mut transfers = Vec::new();
    for cat in &budget.categories {
        let cat_total: i64 = items
            .iter()
            .filter(|it| it.cat_pod == cat.pod_id)
            .map(|it| it.give)
            .sum();
        if cat_total == 0 {
            continue;
        }
        // ungrouped bills: category pod *is* the pool, so skip the pool→pool hop
        if cat.pod_id != budget.pool_pod_id {
            transfers.push(PlannedTransfer {
                from_pod: budget.pool_pod_id.clone(),
                to_pod: cat.pod_id.clone(),
                amount_cents: cat_total,
                reason: format!("fund category {}", cat.name),
            });
        }
        for it in items
            .iter()
            .filter(|it| it.cat_pod == cat.pod_id && it.give > 0)
        {
            transfers.push(PlannedTransfer {
                from_pod: cat.pod_id.clone(),
                to_pod: it.bill.pod_id.clone(),
                amount_cents: it.give,
                reason: format!("fund bill {}", it.bill.name),
            });
        }
    }
    transfers
}

/// Set each item's `give` from `available` per the chosen strategy.
fn allocate(items: &mut [Item], mut available: i64, strategy: FundingStrategy) {
    let by_due = {
        let mut o: Vec<usize> = (0..items.len()).collect();
        o.sort_by_key(|&i| items[i].due);
        o
    };
    match strategy {
        FundingStrategy::SoonestDue => {
            // fund to today's paced target, no further; surplus stays in the pool
            for &i in &by_due {
                let want = (items[i].today_target - items[i].current).max(0);
                let g = want.min(available);
                items[i].give += g;
                available -= g;
            }
        }
        FundingStrategy::StrictEnvelope => {
            for &i in &by_due {
                let want = (items[i].full - items[i].current).max(0);
                let g = want.min(available);
                items[i].give += g;
                available -= g;
            }
        }
        FundingStrategy::Proportional => {
            let needs: Vec<i64> = items
                .iter()
                .map(|it| (it.today_target - it.current).max(0))
                .collect();
            let total: i64 = needs.iter().sum();
            if total == 0 {
                return;
            }
            if total <= available {
                for (it, need) in items.iter_mut().zip(needs) {
                    it.give = need;
                }
            } else {
                for (it, need) in items.iter_mut().zip(needs) {
                    it.give = (need as i128 * available as i128 / total as i128) as i64;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Category;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn semi() -> PaySchedule {
        PaySchedule::SemiMonthly { days: vec![15, -1] }
    }

    fn bill(name: &str, amount: i64, due_day: u32) -> Bill {
        Bill {
            name: name.into(),
            pod_id: format!("pod-{name}"),
            amount_cents: amount,
            due_day: Some(due_day),
            frequency: Frequency::Month,
        }
    }

    #[test]
    fn target_is_half_after_first_payday_full_after_second() {
        // due 28th: funding paydays are Apr 30 + May 15
        let b = bill("rent", 100_000, 28);
        // May 1 — only Apr 30 passed -> 50%
        assert_eq!(target_cents(&b, d(2026, 5, 1), &semi()), 50_000);
        // May 16 — May 15 passed too -> 100%, ahead of the 28th
        assert_eq!(target_cents(&b, d(2026, 5, 16), &semi()), 100_000);
    }

    #[test]
    fn bill_due_early_in_month_funds_from_prior_month_paydays() {
        // due 10th: both funding paydays are the prior month's
        let b = bill("car", 60_000, 10);
        // after May 15 only (one of two) -> 50%
        assert_eq!(target_cents(&b, d(2026, 5, 20), &semi()), 30_000);
        // after May 29 too -> 100% before the June 10th due date
        assert_eq!(target_cents(&b, d(2026, 6, 5), &semi()), 60_000);
    }

    #[test]
    fn weekly_pay_spreads_a_bill_across_more_paydays() {
        // weekly pay: a bill due the 28th accrues across ~4 Fridays, not a hardcoded half
        let s = PaySchedule::Weekly {
            weekday: chrono::Weekday::Fri,
        };
        let b = bill("rent", 100_000, 28);
        // window Apr 28 -> May 28: Fridays May 1, 8, 15, 22; on May 2 one passed -> 25%
        assert_eq!(target_cents(&b, d(2026, 5, 2), &s), 25_000);
    }

    fn auto_budget(bills: Vec<Bill>) -> Budget {
        Budget {
            pool_pod_id: "pool".into(),
            categories: vec![Category {
                name: "auto".into(),
                pod_id: "cat-auto".into(),
                bills,
            }],
        }
    }

    #[test]
    fn drawdown_is_a_constant_per_paycheck_slice() {
        let b = Bill {
            name: "lump".into(),
            pod_id: "pod-lump".into(),
            amount_cents: 80_000,
            due_day: None,
            frequency: Frequency::Drawdown {
                target: d(2026, 8, 26),
                period_months: 6,
            },
        };
        // 12 paychecks in the 6-month window -> a flat slice, SAME every cycle (no catch-up)
        let slice = 80_000 / 12;
        assert_eq!(bill_target(&b, d(2026, 5, 1), &semi()), slice);
        assert_eq!(bill_target(&b, d(2026, 5, 29), &semi()), slice);
        assert_eq!(bill_target(&b, d(2026, 8, 26), &semi()), slice);
        // on_track reflects how far along the window we are (for the "behind" note)
        assert_eq!(
            drawdown_on_track(80_000, d(2026, 8, 26), 6, d(2026, 8, 26), &semi()),
            80_000
        );
    }

    #[test]
    fn plan_funds_pool_to_category_to_bills_with_no_leftover() {
        let budget = auto_budget(vec![bill("car", 60_000, 10), bill("insurance", 20_000, 10)]);
        let mut balances = HashMap::new();
        balances.insert("pool".to_string(), 80_000); // pool covers both
        let plan = plan(
            &budget,
            &balances,
            d(2026, 6, 5),
            &semi(),
            FundingStrategy::SoonestDue,
        );
        // pool->category (80k) then two category->bill (60k + 20k)
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0].from_pod, "pool");
        assert_eq!(plan[0].to_pod, "cat-auto");
        assert_eq!(plan[0].amount_cents, 80_000);
        let to_bills: i64 = plan[1..].iter().map(|t| t.amount_cents).sum();
        assert_eq!(to_bills, 80_000); // category nets to zero
    }

    #[test]
    fn already_funded_pod_needs_nothing() {
        let budget = auto_budget(vec![bill("car", 60_000, 10)]);
        let mut balances = HashMap::new();
        balances.insert("pool".to_string(), 100_000);
        balances.insert("pod-car".to_string(), 60_000); // already at target
        let plan = plan(
            &budget,
            &balances,
            d(2026, 6, 5),
            &semi(),
            FundingStrategy::SoonestDue,
        );
        assert!(plan.is_empty());
    }

    #[test]
    fn plan_bounded_by_pool_balance() {
        // pool can't cover both fully-due bills; only what's there moves
        let budget = auto_budget(vec![bill("car", 60_000, 10), bill("insurance", 20_000, 10)]);
        let mut balances = HashMap::new();
        balances.insert("pool".to_string(), 50_000); // short of the 80k needed
        let plan = plan(
            &budget,
            &balances,
            d(2026, 6, 5),
            &semi(),
            FundingStrategy::SoonestDue,
        );
        let moved: i64 = plan
            .iter()
            .filter(|t| t.from_pod == "pool")
            .map(|t| t.amount_cents)
            .sum();
        assert_eq!(moved, 50_000); // never overdraws the pool
    }

    #[test]
    fn soonest_due_protects_the_nearer_bill_when_short() {
        // rent due 5th, sub due 20th; pool covers only one -> soonest-due funds rent first
        let budget = auto_budget(vec![bill("rent", 100_000, 5), bill("sub", 5_000, 20)]);
        let mut balances = HashMap::new();
        balances.insert("pool".to_string(), 100_000);
        let plan = plan(
            &budget,
            &balances,
            d(2026, 6, 5),
            &semi(),
            FundingStrategy::SoonestDue,
        );
        let rent = plan.iter().find(|t| t.to_pod == "pod-rent").unwrap();
        assert_eq!(rent.amount_cents, 100_000); // nearer due date wins
        assert!(plan.iter().all(|t| t.to_pod != "pod-sub")); // sub starved
    }

    #[test]
    fn flush_month_funds_to_pace_not_to_100_percent() {
        // due 28th on May 1, pace = 50%; a flush pool must fund to pace, not 100%
        let budget = auto_budget(vec![bill("rent", 100_000, 28)]);
        let mut balances = HashMap::new();
        balances.insert("pool".to_string(), 200_000); // far more than the bill needs
        let plan = plan(
            &budget,
            &balances,
            d(2026, 5, 1),
            &semi(),
            FundingStrategy::SoonestDue,
        );
        let rent = plan.iter().find(|t| t.to_pod == "pod-rent").unwrap();
        assert_eq!(rent.amount_cents, 50_000); // paced, not 100k
        let moved: i64 = plan
            .iter()
            .filter(|t| t.from_pod == "pool")
            .map(|t| t.amount_cents)
            .sum();
        assert_eq!(moved, 50_000); // surplus stays in the pool
    }

    #[test]
    fn strict_envelope_funds_to_100_percent_by_design() {
        // same bill/date as the pace test, but StrictEnvelope funds to 100% by design
        let budget = auto_budget(vec![bill("rent", 100_000, 28)]);
        let mut balances = HashMap::new();
        balances.insert("pool".to_string(), 200_000);
        let plan = plan(
            &budget,
            &balances,
            d(2026, 5, 1),
            &semi(),
            FundingStrategy::StrictEnvelope,
        );
        let rent = plan.iter().find(|t| t.to_pod == "pod-rent").unwrap();
        assert_eq!(rent.amount_cents, 100_000); // full, by design — not the 50k pace
    }

    #[test]
    fn proportional_splits_a_short_pool_by_need() {
        // 30k pool, needs 60k+20k=80k -> 60/80 and 20/80 of 30k = 22_500 and 7_500
        let budget = auto_budget(vec![bill("car", 60_000, 10), bill("insurance", 20_000, 10)]);
        let mut balances = HashMap::new();
        balances.insert("pool".to_string(), 30_000);
        let plan = plan(
            &budget,
            &balances,
            d(2026, 6, 5),
            &semi(),
            FundingStrategy::Proportional,
        );
        let car = plan.iter().find(|t| t.to_pod == "pod-car").unwrap();
        let ins = plan.iter().find(|t| t.to_pod == "pod-insurance").unwrap();
        assert_eq!(car.amount_cents, 22_500);
        assert_eq!(ins.amount_cents, 7_500);
    }
}
