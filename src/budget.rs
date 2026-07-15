//! Assemble the live budget and account for *every* pod: turn Sequence pods +
//! the onboarding file into the `Budget` the allocator plans against, classifying
//! the rest (unschedulable bills, old-naming pods, non-bills) so nothing is dropped.

use std::collections::BTreeMap;

use sequence_rs::model::account::{AccountSummary, AccountType};
use sequence_rs::model::rule::{Rule, RuleActionKind, RuleStatus};

use crate::derive::{derive_bill, is_group_marker, parse, parse_scheme, DueDay, Frequency, Parsed};
use crate::model::{Bill, Budget, Category};
use crate::money::dollars;
use crate::state::State;

/// Add the safety buffer to a bill's amount — except per-paycheck contributions
/// and kept levels, which are chosen amounts, not bills to over-cover. Rounds up
/// to the cent.
fn buffered(amount: i64, freq: &Frequency, pct: f64) -> i64 {
    if matches!(freq, Frequency::Paycheck | Frequency::Hold) {
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

/// One money-moving edge extracted from an ENABLED Sequence rule: source pod →
/// destination pod, what it tops up to (or moves), and its per-transfer cap.
pub struct Flow {
    pub src: String,
    pub dst: String,
    /// The top-up target or fixed amount, when the action declares one.
    pub target_cents: Option<i64>,
    /// Per-transfer cap, when set.
    pub cap_cents: Option<i64>,
}

/// Flatten enabled rules into the flows the drift checks reason about.
pub fn flows_from_rules(rules: &[Rule]) -> Vec<Flow> {
    let mut out = Vec::new();
    for r in rules {
        if r.status != RuleStatus::Enabled {
            continue;
        }
        for step in &r.steps {
            for a in &step.actions {
                let target = match &a.kind {
                    RuleActionKind::Fixed { amount_in_cents } => Some(*amount_in_cents),
                    RuleActionKind::TopUp {
                        amount_in_cents, ..
                    } => *amount_in_cents,
                    _ => None,
                };
                out.push(Flow {
                    src: a.base.source.id.clone(),
                    dst: a.base.destination.id.clone(),
                    target_cents: target,
                    cap_cents: a.base.limit.as_ref().map(|c| c.amount_in_cents),
                });
            }
        }
    }
    out
}

/// Config drift: where the pod names (the declared budget) and the enabled rules
/// (what actually moves money while maestro is in Shadow) disagree. Every drift
/// found here has caused a real bounce or an unfunded bill: a rule topping up
/// below the declared amount, a bill pod nothing funds, and a group whose income
/// pull is smaller than the sum its bills can draw.
pub fn drift_warnings(accounts: &[AccountSummary], flows: &[Flow]) -> Vec<String> {
    let mut out = Vec::new();
    for a in accounts {
        if a.account_type != AccountType::Pod {
            continue;
        }
        // Group pods: income must pull at least what the group's bills can draw.
        if is_group_marker(&a.name) {
            let demand: i64 = flows
                .iter()
                .filter(|f| f.src == a.id)
                .map(|f| f.cap_cents.or(f.target_cents).unwrap_or(0))
                .sum();
            let pull = flows
                .iter()
                .filter(|f| f.dst == a.id)
                .filter_map(|f| f.target_cents)
                .max();
            match pull {
                None if demand > 0 => out.push(format!(
                    "{}: no enabled rule pulls into this group — its bills won't fund",
                    a.name
                )),
                Some(p) if p < demand => out.push(format!(
                    "{}: pulls {} from income, but its bills can draw up to {} per cycle — starved by {}",
                    a.name,
                    dollars(p),
                    dollars(demand),
                    dollars(demand - p)
                )),
                _ => {}
            }
            continue;
        }
        // Bill pods: something must fund them, and to at least the declared amount.
        let Some(bill) = derive_bill(&a.name) else {
            continue; // not a bill (pool, savings, spending, …)
        };
        let (name, amount, freq) = (bill.name, bill.amount_cents, bill.frequency);
        let targeting: Vec<&Flow> = flows.iter().filter(|f| f.dst == a.id).collect();
        if targeting.is_empty() {
            out.push(format!(
                "{name}: no enabled rule funds this pod — it will sit empty"
            ));
            continue;
        }
        // Per-paycheck top-ups declare a cap (rule targets differ by design) and
        // drawdowns fund by fixed slice — fixed-amount bills and kept levels
        // compare against the rule target directly.
        if matches!(
            freq,
            Frequency::Month | Frequency::Quarter | Frequency::Year | Frequency::Hold
        ) {
            if let Some(max_t) = targeting.iter().filter_map(|f| f.target_cents).max() {
                if max_t < amount {
                    out.push(format!(
                        "{name}: rule tops up to {} but the pod declares {} — the rule will underfund it",
                        dollars(max_t),
                        dollars(amount)
                    ));
                }
            }
        }
    }
    out
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

    fn pod(id: &str, name: &str) -> AccountSummary {
        AccountSummary {
            id: id.into(),
            name: name.into(),
            account_type: AccountType::Pod,
            description: None,
            external_account_type: None,
            beneficiary_name: None,
            institution_name: None,
            can_be_source: true,
            created_at: String::new(),
            updated_at: String::new(),
            deleted_at: None,
        }
    }

    fn flow(src: &str, dst: &str, target: Option<i64>, cap: Option<i64>) -> Flow {
        Flow {
            src: src.into(),
            dst: dst.into(),
            target_cents: target,
            cap_cents: cap,
        }
    }

    // Amounts use dollar_cents grouping (`50_00` = $50.00) for readability.
    #[allow(clippy::inconsistent_digit_grouping)]
    #[test]
    fn drift_flags_underfunding_rule_orphan_bill_and_starved_group() {
        let accounts = vec![
            pod("g1", "G: Home"),
            pod("p1", "Home / Web / 40 / 24"), // rule below declared
            pod("p2", "Home / Water / 30 / 5"), // no rule at all
        ];
        let flows = vec![
            flow("pool", "g1", Some(50_00), None),      // group pulls $50…
            flow("g1", "p1", Some(39_00), Some(20_00)), // …bill can draw cap $20
            flow("g1", "px", Some(45_00), Some(45_00)), // …other bill draws $45 -> demand $65
        ];
        let w = drift_warnings(&accounts, &flows);
        assert_eq!(w.len(), 3, "{w:?}");
        assert!(w.iter().any(|s| s.contains("starved by $15.00")), "{w:?}");
        assert!(
            w.iter()
                .any(|s| s.contains("Web") && s.contains("underfund")),
            "{w:?}"
        );
        assert!(
            w.iter()
                .any(|s| s.contains("Water") && s.contains("no enabled rule funds")),
            "{w:?}"
        );
    }

    #[allow(clippy::inconsistent_digit_grouping)]
    #[test]
    fn drift_is_silent_when_names_and_rules_agree() {
        let accounts = vec![
            pod("g1", "G: Home"),
            pod("p1", "Home / Web / 40 / 24"),
            pod("p3", "Home / Cell / 25 / paycheck"), // cap semantics: presence is enough
        ];
        let flows = vec![
            flow("pool", "g1", Some(60_00), None),
            flow("g1", "p1", Some(40_00), Some(20_00)),
            flow("g1", "p3", Some(100_00), Some(40_00)),
        ];
        assert!(drift_warnings(&accounts, &flows).is_empty());
    }
}
