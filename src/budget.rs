//! Assemble the live budget and account for *every* pod: turn Sequence pods +
//! the onboarding file into the `Budget` the allocator plans against, classifying
//! the rest (unschedulable bills, old-naming pods, non-bills) so nothing is dropped.

use std::collections::BTreeMap;

use sequence_rs::model::account::{AccountSummary, AccountType};

use crate::derive::{parse, parse_scheme, DueDay, Frequency, Parsed};
use crate::model::{Bill, Budget, Category};
use crate::state::State;

/// True for an explicit `G: Name` group pod.
fn is_group_marker(name: &str) -> bool {
    let t = name.trim_start();
    t.len() >= 2 && t.as_bytes()[..2].eq_ignore_ascii_case(b"g:")
}

/// Add the safety buffer to a bill's amount — except contributions (`Paycheck`),
/// which are a chosen amount, not a bill to over-cover. Rounds up to the cent.
fn buffered(amount: i64, freq: &Frequency, pct: f64) -> i64 {
    if matches!(freq, Frequency::Paycheck) {
        amount
    } else {
        (amount as f64 * (1.0 + pct / 100.0)).ceil() as i64
    }
}

pub struct Assembled {
    pub budget: Budget,
    /// Recognized as a bill but not schedulable yet — name needs fixing.
    pub not_funded: Vec<String>,
    /// Funded, but still on the old naming (or a group pod to migrate).
    pub migrate: Vec<String>,
    /// Not a bill — savings, the pool, debt buffers, spending, etc.
    pub ignored: Vec<String>,
    /// Assembly issues (e.g. a group with no group pod).
    pub warnings: Vec<String>,
}

/// Build the budget and full pod accounting from the account list + onboarding
/// file. `buffer_pct` is added to each bill's funding target (contributions exempt).
pub fn assemble(accounts: &[AccountSummary], file: Option<&State>, buffer_pct: f64) -> Assembled {
    let mut warnings = Vec::new();
    let mut not_funded = Vec::new();
    let mut migrate = Vec::new();
    let mut ignored = Vec::new();

    let pool_pod_id = match file.and_then(|f| f.pools.first()) {
        Some(p) => p.pod_id.clone(),
        None => {
            warnings.push("no income pool detected — run `discover` first".into());
            String::new()
        }
    };

    // group name -> group pod id (old "Auto (1800/900)" or migrated "Auto")
    let mut group_pods: BTreeMap<String, String> = BTreeMap::new();
    for a in accounts {
        if a.account_type != AccountType::Pod {
            continue;
        }
        if let Parsed::Category { name } = parse(&a.name) {
            group_pods.entry(name).or_insert_with(|| a.id.clone());
        }
    }

    // Classify every pod.
    let mut by_group: BTreeMap<Option<String>, Vec<Bill>> = BTreeMap::new();
    for a in accounts {
        if a.account_type != AccountType::Pod || a.id == pool_pod_id {
            continue;
        }
        // New scheme first.
        if let Some(b) = parse_scheme(&a.name) {
            let due_day = b.due_day.map(|d| match d {
                DueDay::Day(n) => n,
                DueDay::Last => 31, // clamped to month length downstream
            });
            if due_day.is_some() && b.frequency != Frequency::Month {
                not_funded.push(format!(
                    "{} — due day + {:?} (drop the day for a sinking fund, or use a date)",
                    a.name, b.frequency
                ));
                continue;
            }
            let amount_cents = buffered(b.amount_cents, &b.frequency, buffer_pct);
            by_group.entry(b.group).or_default().push(Bill {
                name: b.name,
                pod_id: a.id.clone(),
                amount_cents,
                due_day,
                frequency: b.frequency,
            });
            continue;
        }
        // Old scheme.
        match parse(&a.name) {
            Parsed::Bill {
                name,
                amount_cents,
                due_day,
            } => {
                migrate.push(a.name.clone());
                by_group.entry(None).or_default().push(Bill {
                    name,
                    pod_id: a.id.clone(),
                    amount_cents: buffered(amount_cents, &Frequency::Month, buffer_pct),
                    due_day: Some(due_day),
                    frequency: Frequency::Month,
                });
            }
            Parsed::BillNoDueDay { .. } => {
                not_funded.push(format!(
                    "{} — no due day (add a day, or a frequency)",
                    a.name
                ));
            }
            Parsed::Category { .. } => {
                // `G:` is target naming; only old `(total/half)` needs migrating
                if !is_group_marker(&a.name) {
                    migrate.push(format!("{} (group pod — rename to `G: …`)", a.name));
                }
            }
            Parsed::Skip => ignored.push(a.name.clone()),
        }
    }

    // grouped bills route through their group pod; ungrouped (or no group pod) from the pool
    let mut categories = Vec::new();
    for (group, bills) in by_group {
        let (name, pod_id) = match group {
            None => ("(ungrouped)".to_string(), pool_pod_id.clone()),
            Some(g) => match group_pods.get(&g) {
                Some(pid) => (g, pid.clone()),
                None => {
                    warnings.push(format!(
                        "no group pod for '{g}' — funding its bills from the pool"
                    ));
                    (g, pool_pod_id.clone())
                }
            },
        };
        categories.push(Category {
            name,
            pod_id,
            bills,
        });
    }

    Assembled {
        budget: Budget {
            pool_pod_id,
            categories,
        },
        not_funded,
        migrate,
        ignored,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Amounts use dollar_cents grouping (`100_00` = $100.00) for readability.
    #[allow(clippy::inconsistent_digit_grouping)]
    #[test]
    fn buffer_adds_pct_and_always_rounds_up_to_the_cent() {
        // $100.00 at 2.5% -> exactly $102.50.
        assert_eq!(buffered(100_00, &Frequency::Month, 2.5), 102_50);
        // $33.33 * 1.025 = $34.16325 -> rounds UP to $34.17 (never under-funds).
        assert_eq!(buffered(33_33, &Frequency::Month, 2.5), 34_17);
    }

    #[test]
    fn contributions_get_no_buffer() {
        // A per-paycheck contribution is a chosen amount, not a bill to over-cover.
        assert_eq!(buffered(50_000, &Frequency::Paycheck, 2.5), 50_000);
    }
}
