//! Derive the budget model from Sequence pod names — the single source of truth.
//! Convention: `Name (amount) - DueDay` (e.g. "Phone Bill ($80) - 16th"); categories
//! as `Name (monthly/half)`. Never turns a malformed bill into nothing — callers surface non-Bill results.

use chrono::NaiveDate;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Bill {
        name: String,
        amount_cents: i64,
        due_day: u32,
    },
    /// Amount parsed but no due day — can't be scheduled; must be flagged.
    BillNoDueDay { name: String, amount_cents: i64 },
    /// A `(monthly/half)` category pod.
    Category { name: String },
    /// No recognizable bill/category pattern — ignored.
    Skip,
}

fn parse_money(s: &str) -> Option<i64> {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    match cleaned.split_once('.') {
        Some((d, c)) => {
            let dollars: i64 = if d.is_empty() { 0 } else { d.parse().ok()? };
            let frac = format!("{:0<2}", &c[..c.len().min(2)]);
            Some(dollars * 100 + frac.parse::<i64>().ok()?)
        }
        None => Some(cleaned.parse::<i64>().ok()? * 100),
    }
}

fn leading_u32(s: &str) -> Option<u32> {
    let digits: String = s
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// True for an explicit `G: Name` group pod — the one definition of the marker.
pub fn is_group_marker(name: &str) -> bool {
    let t = name.trim_start();
    t.len() >= 2 && t.as_bytes()[..2].eq_ignore_ascii_case(b"g:")
}

pub fn parse(name: &str) -> Parsed {
    // Explicit group marker: `G: Home` is the group pod for group "Home".
    let trimmed = name.trim();
    if is_group_marker(trimmed) {
        let mut g = trimmed[2..].trim();
        // Tolerate a leftover `(total/half)` suffix during migration.
        if g.ends_with(')') {
            if let Some(open) = g.rfind('(') {
                g = g[..open].trim();
            }
        }
        return Parsed::Category {
            name: g.to_string(),
        };
    }
    let (open, close) = match (name.rfind('('), name.rfind(')')) {
        (Some(o), Some(c)) if c > o => (o, c),
        _ => return Parsed::Skip,
    };
    let inside = &name[open + 1..close];
    let label = name[..open].trim().to_string();

    if inside.contains('/') {
        return Parsed::Category { name: label };
    }
    let amount_cents = match parse_money(inside) {
        Some(a) => a,
        None => return Parsed::Skip,
    };

    let tail = &name[close + 1..];
    match tail.split_once('-').and_then(|(_, d)| leading_u32(d)) {
        Some(due_day) if (1..=31).contains(&due_day) => Parsed::Bill {
            name: label,
            amount_cents,
            due_day,
        },
        _ => Parsed::BillNoDueDay {
            name: label,
            amount_cents,
        },
    }
}

// ---- New scheme: `Group / Name / Amount / DueDay [/ Frequency]` (grammar in the README) ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DueDay {
    Day(u32),
    Last,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frequency {
    Month,
    Quarter,
    Year,
    /// Top up to `amount` each paycheck — not a sinking fund. Ignores the running
    /// balance; looks at how much has landed *since the last payday* (ledger) and
    /// funds only the gap up to `amount`. Keyword `topup` / `paycheck-topup`.
    Paycheck,
    /// Save toward a dated lump: accumulate `amount` across the paychecks leading
    /// up to `target`, then roll the target forward by `period_months` and repeat.
    /// For irregular-but-known bills (e.g. an $800 vet bill every 6 months).
    Drawdown {
        target: NaiveDate,
        period_months: u32,
    },
    /// Keep a flat level on hand: the pod targets `amount` at all times, and when
    /// spending dips it below, the next cycle refills the gap. Never skimmed, and
    /// no scheduled debit is expected — for prepaid balances and spend-and-refill
    /// pods. Keyword `keep` (aliases `refill` / `float`).
    Keep,
}

/// Parse a calendar period like `6mo`, `3m`, `1y`, `1y6mo` into total months.
/// Only `y` and `mo`/`m` are accepted — days/weeks are rejected so the period
/// stays calendar-clean (chrono's `Months` does the actual stepping).
pub fn parse_period_months(s: &str) -> Option<u32> {
    let s = s.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    let mut total = 0u32;
    let mut num = 0u32;
    let mut seen_digit = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            num = num * 10 + (c as u32 - '0' as u32);
            seen_digit = true;
        } else if c == 'y' {
            total += num * 12;
            num = 0;
            seen_digit = false;
        } else if c == 'm' {
            if chars.peek() == Some(&'o') {
                chars.next();
            }
            total += num;
            num = 0;
            seen_digit = false;
        } else {
            return None; // unknown unit (days/weeks not allowed)
        }
    }
    if seen_digit || total == 0 {
        return None;
    }
    Some(total)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillName {
    /// `None` for a standalone (groupless) bill funded straight from the pool.
    pub group: Option<String>,
    pub name: String,
    pub amount_cents: i64,
    /// `None` when the name gives a frequency but no due day.
    pub due_day: Option<DueDay>,
    pub frequency: Frequency,
}

fn parse_dueday(s: &str) -> Option<DueDay> {
    match s.to_ascii_lowercase().as_str() {
        "last" => Some(DueDay::Last),
        d => match d.parse::<u32>() {
            Ok(n) if (1..=31).contains(&n) => Some(DueDay::Day(n)),
            _ => None,
        },
    }
}

fn parse_freq(s: &str) -> Option<Frequency> {
    match s.to_ascii_lowercase().as_str() {
        "month" | "monthly" => Some(Frequency::Month),
        "quarter" | "quarterly" => Some(Frequency::Quarter),
        "year" | "yearly" | "annual" | "annually" => Some(Frequency::Year),
        "topup" | "paycheck-topup" | "paycheck" | "payday" => Some(Frequency::Paycheck),
        "keep" | "refill" | "float" => Some(Frequency::Keep),
        _ => None,
    }
}

/// Parse the new positional scheme (None if not this scheme). Group optional (leading field):
/// `[Group /] Name / Amount / DueDay [/ Frequency]` (freq default month), `… / Amount / Frequency`
/// (no due day), or `… / Amount / drawdown / Date / Period` (dated sinking fund).
pub fn parse_scheme(pod_name: &str) -> Option<BillName> {
    let parts: Vec<&str> = pod_name.split(" / ").map(str::trim).collect();

    // Drawdown form: `[Group /] Name / Amount / drawdown / YYYY-MM-DD / Period`.
    if let Some(i) = parts
        .iter()
        .position(|p| p.eq_ignore_ascii_case("drawdown"))
    {
        if i < 2 {
            return None; // need at least Name / Amount before `drawdown`
        }
        let amount_cents = parse_money(parts[i - 1])?;
        let name = parts[i - 2].to_string();
        let group = if i >= 3 {
            Some(parts[i - 3].to_string())
        } else {
            None
        };
        let target = NaiveDate::parse_from_str(parts.get(i + 1)?, "%Y-%m-%d").ok()?;
        let period_months = parse_period_months(parts.get(i + 2)?)?;
        return Some(BillName {
            group,
            name,
            amount_cents,
            due_day: None,
            frequency: Frequency::Drawdown {
                target,
                period_months,
            },
        });
    }

    let (group, rest) = match parts.len() {
        3 => (None, &parts[..]),
        4 | 5 => (Some(parts[0].to_string()), &parts[1..]),
        _ => return None,
    };
    // rest = [Name, Amount, DueDayOrFreq, (Frequency)]
    let amount_cents = parse_money(rest[1])?;
    let (due_day, frequency) = if rest.len() == 4 {
        (Some(parse_dueday(rest[2])?), parse_freq(rest[3])?)
    } else if let Some(d) = parse_dueday(rest[2]) {
        (Some(d), Frequency::Month)
    } else {
        (None, parse_freq(rest[2])?)
    };
    Some(BillName {
        group,
        name: rest[0].to_string(),
        amount_cents,
        due_day,
        frequency,
    })
}

/// A bill ready for the allocator, from either naming scheme. New-scheme pods
/// carry a group; old-scheme bills are ungrouped (`group: None`) during migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedBill {
    pub group: Option<String>,
    pub name: String,
    pub amount_cents: i64,
    /// `None` = no due date; fund by even accumulation over the frequency period.
    pub due_day: Option<u32>,
    pub frequency: Frequency,
}

/// Unify both schemes into one bill, or None if the pod isn't a bill. The one
/// fallback chain every "what does this pod declare?" consumer shares. (An
/// old-scheme bill missing its due day derives with `due_day: None`.)
pub fn derive_bill(pod_name: &str) -> Option<DerivedBill> {
    if let Some(b) = parse_scheme(pod_name) {
        let due_day = b.due_day.map(|d| match d {
            DueDay::Day(n) => n,
            DueDay::Last => 31, // clamped to the real month length downstream
        });
        return Some(DerivedBill {
            group: b.group,
            name: b.name,
            amount_cents: b.amount_cents,
            due_day,
            frequency: b.frequency,
        });
    }
    match parse(pod_name) {
        Parsed::Bill {
            name,
            amount_cents,
            due_day,
        } => Some(DerivedBill {
            group: None,
            name,
            amount_cents,
            due_day: Some(due_day),
            frequency: Frequency::Month,
        }),
        Parsed::BillNoDueDay { name, amount_cents } => Some(DerivedBill {
            group: None,
            name,
            amount_cents,
            due_day: None,
            frequency: Frequency::Month,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_bill_handles_both_schemes() {
        assert_eq!(
            derive_bill("Auto / Car Payment / 450 / 28"),
            Some(DerivedBill {
                group: Some("Auto".into()),
                name: "Car Payment".into(),
                amount_cents: 45000,
                due_day: Some(28),
                frequency: Frequency::Month,
            })
        );
        assert_eq!(
            derive_bill("Electric (120) - 23rd ish"),
            Some(DerivedBill {
                group: None,
                name: "Electric".into(),
                amount_cents: 12000,
                due_day: Some(23),
                frequency: Frequency::Month,
            })
        );
        assert_eq!(derive_bill("Extra"), None);
    }

    #[test]
    fn frequency_only_no_due_day() {
        // 4 fields where the last is a frequency word -> no due day.
        let b = parse_scheme("Home / Pest Control / 50 / annual").unwrap();
        assert_eq!(b.due_day, None);
        assert_eq!(b.frequency, Frequency::Year);
        assert_eq!(
            derive_bill("Home / Pest Control / 50 / annual")
                .unwrap()
                .due_day,
            None
        );
    }

    #[test]
    fn keep_parses_with_aliases() {
        for kw in ["keep", "refill", "float"] {
            let b = parse_scheme(&format!("Misc / Game Credit / 25 / {kw}")).unwrap();
            assert_eq!(b.frequency, Frequency::Keep);
            assert_eq!(b.due_day, None);
            assert_eq!(b.amount_cents, 2_500);
        }
    }

    #[test]
    fn parses_new_scheme() {
        assert_eq!(
            parse_scheme("Auto / Car Payment / 450 / 28"),
            Some(BillName {
                group: Some("Auto".into()),
                name: "Car Payment".into(),
                amount_cents: 45000,
                due_day: Some(DueDay::Day(28)),
                frequency: Frequency::Month,
            })
        );
        // Groupless (3 fields): standalone monthly lump, funded from the pool.
        assert_eq!(
            parse_scheme("Allowance / 1000 / month"),
            Some(BillName {
                group: None,
                name: "Allowance".into(),
                amount_cents: 100000,
                due_day: None,
                frequency: Frequency::Month,
            })
        );
        // Both forms work: "yearly" == "year"; default (omitted) -> Month.
        assert_eq!(
            parse_scheme("Memberships / Warehouse Club / 120 / 10 / yearly").map(|b| b.frequency),
            Some(Frequency::Year)
        );
        assert_eq!(
            parse_scheme("Memberships / Warehouse Club / 120 / 10 / year").map(|b| b.frequency),
            Some(Frequency::Year)
        );
        assert_eq!(
            parse_scheme("Utilities / Electric / 90 / last").and_then(|b| b.due_day),
            Some(DueDay::Last)
        );
        // Old-scheme / non-bill names don't match.
        assert_eq!(parse_scheme("Car Loan (450) - 28th"), None);
        assert_eq!(parse_scheme("Extra"), None);
    }

    #[test]
    fn parses_standard_bill() {
        assert_eq!(
            parse("Phone Bill ($80) - 16th"),
            Parsed::Bill {
                name: "Phone Bill".into(),
                amount_cents: 8000,
                due_day: 16
            }
        );
    }

    #[test]
    fn tolerates_ish_and_no_dollar_sign_and_slash_in_name() {
        assert_eq!(
            parse("Electric (120) - 23rd ish"),
            Parsed::Bill {
                name: "Electric".into(),
                amount_cents: 12000,
                due_day: 23
            }
        );
        assert_eq!(
            parse("Water/Sewer (90) - 1st"),
            Parsed::Bill {
                name: "Water/Sewer".into(),
                amount_cents: 9000,
                due_day: 1
            }
        );
    }

    #[test]
    fn flags_bill_without_due_day() {
        assert_eq!(
            parse("Alarm (50)"),
            Parsed::BillNoDueDay {
                name: "Alarm".into(),
                amount_cents: 5000
            }
        );
    }

    #[test]
    fn category_and_skip() {
        assert_eq!(
            parse("Auto (600/300)"),
            Parsed::Category {
                name: "Auto".into()
            }
        );
        assert_eq!(parse("Spending Account"), Parsed::Skip);
        assert_eq!(parse("Extra"), Parsed::Skip);
    }

    #[test]
    fn explicit_group_marker() {
        // `G: Name` is the group pod for group "Name" (case-insensitive prefix).
        assert_eq!(
            parse("G: Home"),
            Parsed::Category {
                name: "Home".into()
            }
        );
        assert_eq!(
            parse("g: Auto"),
            Parsed::Category {
                name: "Auto".into()
            }
        );
        // Tolerate a half-migrated name with the old (total/half) suffix.
        assert_eq!(
            parse("G: Household (1000/500)"),
            Parsed::Category {
                name: "Household".into()
            }
        );
    }

    #[test]
    fn period_parsing() {
        assert_eq!(parse_period_months("6mo"), Some(6));
        assert_eq!(parse_period_months("3m"), Some(3));
        assert_eq!(parse_period_months("1y"), Some(12));
        assert_eq!(parse_period_months("1y6mo"), Some(18));
        assert_eq!(parse_period_months("2y"), Some(24));
        assert_eq!(parse_period_months("5d"), None); // days rejected
        assert_eq!(parse_period_months("mo"), None); // no number
    }

    #[test]
    fn parses_drawdown() {
        let b = parse_scheme("Health / Dentist / 600 / drawdown / 2026-08-26 / 6mo").unwrap();
        assert_eq!(b.group, Some("Health".into()));
        assert_eq!(b.name, "Dentist");
        assert_eq!(b.amount_cents, 60000);
        assert_eq!(
            b.frequency,
            Frequency::Drawdown {
                target: NaiveDate::from_ymd_opt(2026, 8, 26).unwrap(),
                period_months: 6,
            }
        );
        // groupless form
        assert_eq!(
            parse_scheme("Dentist / 600 / drawdown / 2026-08-26 / 6mo")
                .unwrap()
                .group,
            None
        );
        // missing period is rejected
        assert!(parse_scheme("Health / Dentist / 600 / drawdown / 2026-08-26").is_none());
    }
}
