//! One funding cycle, end to end. Read live pods, assemble the budget, plan, then
//! report (dry run) or execute. `MAESTRO_DRY_RUN` is the only difference.

use std::collections::HashMap;

use chrono::{Duration, NaiveDate, Utc};
use sequence_rs::model::account::AccountType;
use sequence_rs::{ListAccountTransfersParams, Sequence};

use crate::allocator::{bill_target, drawdown_on_track, drawdown_slice, plan, PlannedTransfer};
use crate::budget::assemble;
use crate::config::{Config, Phase};
use crate::derive::Frequency;

/// Which leg of the waterfall a transfer is, classified by its source. Drives
/// what each rollout phase is allowed to execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leg {
    /// Source is the Income Fund → a group pod or a standalone bill.
    TopLevel,
    /// Source is a group pod → a bill inside that group.
    WithinGroup,
}

/// Whether `phase` may execute a transfer on `leg`. (`Shadow` executes nothing,
/// `Groups` only top-level, `Full` everything.)
fn phase_executes(phase: Phase, leg: Leg) -> bool {
    match phase {
        Phase::Shadow => false,
        Phase::Groups => leg == Leg::TopLevel,
        Phase::Full => true,
    }
}

/// How a bill is funded — drives both display and whether `rebalance` touches it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Sinking fund, paced toward a due date/period. The only kind `rebalance` touches.
    Sinking,
    /// Per-paycheck top-up. Income-funded, so `rebalance` leaves it alone.
    Topup,
    /// Dated lump (drawdown). Income-funded, so `rebalance` leaves it alone.
    Drawdown,
    /// Flat level held on hand (`hold`) — refilled by funding, never rebalanced.
    Hold,
}

/// One bill's demand-vs-funded picture for this cycle.
pub struct BillLine {
    pub group: String,
    pub name: String,
    pub pod_id: String,
    pub current: i64,
    pub target: i64,
    pub need: i64,
    pub give: i64,
    /// Money committed to a scheduled debit that hasn't cleared this cycle (a late
    /// ACH/weekend slip). Decisions use the effective balance `current - reserved`.
    pub reserved: i64,
    pub kind: LineKind,
}

impl BillLine {
    /// Income-funded (top-up/drawdown) or a kept level — never rebalanced.
    pub fn is_contribution(&self) -> bool {
        self.kind != LineKind::Sinking
    }
}

pub struct Report {
    pub today: NaiveDate,
    pub pool_pod_id: String,
    pub pool_balance: i64,
    pub transfers: Vec<PlannedTransfer>,
    pub lines: Vec<BillLine>,
    /// Recognized bills that can't be scheduled yet (name needs fixing).
    pub not_funded: Vec<String>,
    /// Funded, but still on the old naming.
    pub migrate: Vec<String>,
    /// Pods that aren't bills.
    pub ignored: Vec<String>,
    /// Informational notes (e.g. a drawdown pod that's behind pace).
    pub notes: Vec<String>,
    pub warnings: Vec<String>,
    pub executed: bool,
    /// pod id -> display name, for rendering the plan.
    pub names: HashMap<String, String>,
}

/// What a rebalance would move, from a cycle's lines — the single source of truth
/// for "ahead/behind/net" so `run` and `rebalance` never disagree. Skims pods ahead
/// of pace, fills those behind; skips income-funded contributions and cycle-boundary
/// pods (target $0, holding a just-due bill). $1.00 transfer floor.
pub struct RebalancePlan {
    /// (display name, pod id, amount) for each pod to skim into the reclaim pod.
    pub skims: Vec<(String, String, i64)>,
    /// (display name, pod id, amount) for each pod to top back up to pace.
    pub fills: Vec<(String, String, i64)>,
    /// Pods skipped because they're at a cycle boundary (just-due bill).
    pub boundary: u32,
}

impl RebalancePlan {
    pub fn skim_total(&self) -> i64 {
        self.skims.iter().map(|s| s.2).sum()
    }
    pub fn fill_total(&self) -> i64 {
        self.fills.iter().map(|f| f.2).sum()
    }
    /// Net surplus reclaimed (skims − fills). Negative means leveling to pace
    /// needs money *from* income.
    pub fn net(&self) -> i64 {
        self.skim_total() - self.fill_total()
    }
}

pub fn rebalance_plan(report: &Report) -> RebalancePlan {
    let mut skims = Vec::new();
    let mut fills = Vec::new();
    let mut boundary = 0u32;
    for line in &report.lines {
        if line.is_contribution() {
            continue; // income-funded — not ours to skim
        }
        let diff = (line.current - line.reserved) - line.target;
        if line.target == 0 && diff >= 100 {
            boundary += 1; // just-due: holding for the bill, don't drain
        } else if diff >= 100 {
            skims.push((line.name.clone(), line.pod_id.clone(), diff));
        } else if diff < 0 {
            // behind pace — fill, floored to the $1.00 min transfer
            fills.push((line.name.clone(), line.pod_id.clone(), (-diff).max(100)));
        }
    }
    RebalancePlan {
        skims,
        fills,
        boundary,
    }
}

/// Render a cycle's accounting — the have/target/need/fund table plus not-funded /
/// migrate / ignored. Shared by `run` and the daemon.
pub fn render(report: &Report) -> String {
    use colored::Colorize;
    use std::fmt::Write;
    let d = crate::money::dollars;
    let mut out = String::new();

    let mut group = String::new();
    for line in &report.lines {
        if line.group != group {
            group = line.group.clone();
            let _ = writeln!(out, "{}", format!("=== {group} ===").cyan().bold());
        }
        // have = real balance; colored vs target for sinking/drawdown, plain for top-up (spent down)
        let have_cell = format!("{:>9}", d(line.current));
        let have_col = match line.kind {
            LineKind::Topup => have_cell.normal().to_string(),
            _ if line.current < line.target => have_cell.red().to_string(),
            _ if line.current > line.target => have_cell.blue().to_string(),
            _ => have_cell.green().to_string(),
        };
        let need_cell = format!("{:>9}", d(line.need));
        let need_col = if line.need > 0 {
            need_cell.red().bold().to_string()
        } else {
            need_cell.dimmed().to_string()
        };
        let tail = if line.reserved > 0 {
            format!("  [debit due — {} not yet cleared]", d(line.reserved))
                .yellow()
                .to_string()
        } else {
            match line.kind {
                LineKind::Topup if line.need > 0 => "  [top-up — owes this pay]".cyan().to_string(),
                LineKind::Topup => "  [top-up — met this pay]".dimmed().to_string(),
                LineKind::Drawdown if line.need > 0 => "  [drawdown — behind pace, no catch-up]"
                    .yellow()
                    .to_string(),
                LineKind::Drawdown => "  [drawdown — on pace]".dimmed().to_string(),
                LineKind::Hold if line.need > 0 => {
                    "  [hold — below level, refilling]".cyan().to_string()
                }
                LineKind::Hold => "  [hold — at level]".dimmed().to_string(),
                LineKind::Sinking if line.give < line.need => {
                    "  <-- short".red().bold().to_string()
                }
                LineKind::Sinking if line.current > line.target => {
                    format!("  ahead {}", d(line.current - line.target))
                        .blue()
                        .to_string()
                }
                LineKind::Sinking => String::new(),
            }
        };
        let _ = writeln!(
            out,
            "  {:<22} have {}  target {:>9}  need {}  fund {:>9}{tail}",
            line.name.chars().take(22).collect::<String>(),
            have_col,
            d(line.target),
            need_col,
            d(line.give),
        );
    }

    // rebalance view — the same computation the `rebalance` command runs
    let rb = rebalance_plan(report);
    let net = rb.net();
    let _ = writeln!(
        out,
        "\n{}",
        format!(
            "ahead of pace {}  ·  behind {}",
            d(rb.skim_total()),
            d(rb.fill_total())
        )
        .bold()
    );
    if net > 0 {
        let _ = writeln!(
            out,
            "{}",
            format!(
                "net {} reclaimable → reclaim pod (run `maestro rebalance`)",
                d(net)
            )
            .green()
        );
    } else if net < 0 {
        let _ = writeln!(
            out,
            "{}",
            format!(
                "net {} short — needs {} from income to level up",
                d(net),
                d(-net)
            )
            .red()
        );
    }
    if rb.boundary > 0 {
        let _ = writeln!(
            out,
            "{}",
            format!(
                "({} pod(s) at a cycle boundary, holding for a just-due bill)",
                rb.boundary
            )
            .dimmed()
        );
    }
    if !report.not_funded.is_empty() {
        let _ = writeln!(out, "\n{}", "[!] NOT funded — fix the name:".red().bold());
        for n in &report.not_funded {
            let _ = writeln!(out, "    {}", n.red());
        }
    }
    if !report.migrate.is_empty() {
        let _ = writeln!(
            out,
            "\n{}",
            format!("still on old naming ({}):", report.migrate.len()).yellow()
        );
        for n in &report.migrate {
            let _ = writeln!(out, "    {n}");
        }
    }
    if !report.notes.is_empty() {
        let _ = writeln!(out, "\n{}", "notes:".cyan());
        for n in &report.notes {
            let _ = writeln!(out, "    {}", n.dimmed());
        }
    }
    if !report.warnings.is_empty() {
        let _ = writeln!(out, "\n{}", "[!] warnings:".yellow().bold());
        for w in &report.warnings {
            let _ = writeln!(out, "    {}", w.yellow());
        }
    }
    if !report.ignored.is_empty() {
        let _ = writeln!(
            out,
            "\n{}",
            format!(
                "ignored — not bills ({}): {}",
                report.ignored.len(),
                report.ignored.join(", ")
            )
            .dimmed()
        );
    }
    out
}

/// Total money that actually landed in `pod_id` (settled transfers only) on or
/// after `since` — tells whether a per-paycheck contribution was already made this
/// period. A failed read is an error, not $0: "no contribution seen" would re-fund
/// the pod and double-pay it.
async fn contributed_since(
    client: &Sequence,
    pod_id: &str,
    since: NaiveDate,
) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
    let transfers = crate::fetch::transfers(
        client,
        pod_id,
        ListAccountTransfersParams {
            from: Some(format!("{since}T00:00:00Z")),
            ..Default::default()
        },
    )
    .await?;
    Ok(transfers
        .iter()
        .filter(|t| crate::fetch::settled(t))
        .filter(|t| t.destination.as_ref().and_then(|d| d.id.as_deref()) == Some(pod_id))
        .filter(|t| crate::fetch::parse_date(&t.created_at).is_some_and(|d| d >= since))
        .map(|t| t.amount_in_cents)
        .sum())
}

/// Simulation overrides for `assess`: pretend it's `today` and add `deposit_cents`
/// to the pool — see what a paycheck would do on a date, moving nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct SimInput {
    pub today: Option<NaiveDate>,
    pub deposit_cents: i64,
}

impl SimInput {
    /// True when any override is set. A simulation skips the pending-debit
    /// reservation (real outflow history against a pretend date produces phantom
    /// holds) and the config-drift checks (noise inside a what-if) — it answers
    /// one question: given balances now and this deposit, what does the allocator do?
    fn is_sim(&self) -> bool {
        self.today.is_some() || self.deposit_cents != 0
    }
}

/// Read-only assessment: balances, targets, plan, lines — nothing moved. `run` and
/// `rebalance` share it, so they see identical numbers.
pub async fn assess(cfg: &Config) -> Result<Report, Box<dyn std::error::Error + Send + Sync>> {
    assess_with(cfg, SimInput::default()).await
}

/// Pending-debit reservation: a due-day bill's scheduled debit doesn't land
/// exactly on the due date — it can slip a few days late (slow ACH/weekend) or
/// pull several days early (some autopays debit ~5 days ahead). If this cycle's
/// debit hasn't gone out yet, the pod still holds that money but it's committed —
/// reserve it so funding/rebalance don't mistake the pod for "full". Only bills
/// near their due date get the extra outflow fetch, so it stays cheap.
async fn pending_debit_reservations(
    client: &Sequence,
    assembled: &crate::budget::Assembled,
    balances: &HashMap<String, i64>,
    today: NaiveDate,
    sched: &crate::schedule::PaySchedule,
) -> HashMap<String, i64> {
    const DANGER_DAYS: i64 = 7;
    // Look back this far from the due date for the debit, to catch autopays that
    // pull before the due date. Kept well under a cycle so the prior month's debit
    // isn't counted as this one's.
    const DEBIT_LEAD_DAYS: i64 = 12;
    let at_risk: Vec<_> = assembled
        .budget
        .categories
        .iter()
        .flat_map(|c| &c.bills)
        .filter_map(|b| {
            let due = b.due_day?;
            let d_last = crate::schedule::most_recent_due(due, today);
            ((today - d_last).num_days() <= DANGER_DAYS).then_some((b, d_last))
        })
        .collect();
    // Fetch outflows one pod at a time: the at-risk set is small, and a concurrent
    // burst trips API rate limits — which, as fetch failures, would skip reservations
    // unpredictably (run-to-run flip-flop). Serial keeps the result reliable.
    let mut reserved: HashMap<String, i64> = HashMap::new();
    for (b, d_last) in &at_risk {
        let from = format!("{}T00:00:00Z", *d_last - Duration::days(DEBIT_LEAD_DAYS));
        // A failed fetch means we don't know — don't reserve, lest a transient
        // API error read as a missing debit. Cap at the real balance: a pod can only
        // have committed what it actually holds (else need double-counts the bill).
        let Ok(seen) = crate::fetch::outflow_since_cents(client, &b.pod_id, &from).await else {
            continue;
        };
        let current = balances.get(&b.pod_id).copied().unwrap_or(0);
        let target = bill_target(b, today, sched);
        let unc = crate::cards::reserved_cents(b.amount_cents, seen, current, target);
        if unc > 0 {
            reserved.insert(b.pod_id.clone(), unc);
        }
    }
    reserved
}

/// `assess` with optional simulation overrides (date + injected deposit). With a
/// default `SimInput` it's identical to `assess`.
pub async fn assess_with(
    cfg: &Config,
    sim: SimInput,
) -> Result<Report, Box<dyn std::error::Error + Send + Sync>> {
    let client = cfg.client();
    let today = sim.today.unwrap_or_else(|| Utc::now().date_naive());
    let file = cfg.state_file();
    let (sched, _) = cfg.pay_schedule();
    let strategy = cfg.funding_strategy();

    let accounts = crate::fetch::accounts(&client).await?;

    let mut names = HashMap::new();
    for a in &accounts {
        names.insert(a.id.clone(), a.name.clone());
    }
    // pod balances: the list endpoint omits them, so each pod needs its own GET.
    let pod_ids: Vec<String> = accounts
        .iter()
        .filter(|a| a.account_type == AccountType::Pod)
        .map(|a| a.id.clone())
        .collect();
    let mut balances = crate::fetch::balances(&client, &pod_ids).await?;

    let assembled = assemble(&accounts, file.as_ref(), cfg.buffer_pct);

    // Config drift: the engine reads names, rules, and money every cycle — it
    // notices when they disagree instead of waiting for a human to cross-check.
    // (Every class of drift here has caused a real bounce.) A failed rules read
    // degrades to a warning; funding still runs on the declared model.
    let mut warnings = assembled.warnings.clone();
    if !sim.is_sim() {
        match crate::fetch::rules_detailed(&client).await {
            Ok(rules) => {
                let flows = crate::budget::flows_from_rules(&rules);
                warnings.extend(crate::budget::drift_warnings(&accounts, &flows));
            }
            Err(e) => warnings.push(format!("rules unreadable — config drift not checked ({e})")),
        }
    }

    // Drawdown notes: how far behind pace each drawdown pod is — never auto-catches-up
    let mut notes = Vec::new();
    for bill in assembled.budget.categories.iter().flat_map(|c| &c.bills) {
        if let Frequency::Drawdown {
            target,
            period_months,
        } = &bill.frequency
        {
            let real = balances.get(&bill.pod_id).copied().unwrap_or(0);
            let on_track =
                drawdown_on_track(bill.amount_cents, *target, *period_months, today, &sched);
            let slice = drawdown_slice(bill.amount_cents, *target, *period_months, today, &sched);
            if real + slice < on_track {
                notes.push(format!(
                    "{}: {} saved toward {} by {} — ~{} behind pace (top up to catch up, optional)",
                    bill.name,
                    crate::money::dollars(real),
                    crate::money::dollars(bill.amount_cents),
                    target,
                    crate::money::dollars(on_track - real),
                ));
            }
        }
    }

    // top-ups/drawdowns fund off the LEDGER (deposits since last payday), not running balance
    let period_start = sched
        .paydays_in(today - Duration::days(40), today)
        .last()
        .copied()
        .unwrap_or(today);
    let contrib_pods: Vec<String> = assembled
        .budget
        .categories
        .iter()
        .flat_map(|c| &c.bills)
        .filter(|b| {
            matches!(
                b.frequency,
                Frequency::Paycheck | Frequency::Drawdown { .. }
            )
        })
        .map(|b| b.pod_id.clone())
        .collect();
    let contribs = futures::future::join_all(
        contrib_pods
            .iter()
            .map(|pod_id| contributed_since(&client, pod_id, period_start)),
    )
    .await;
    let mut contributed: HashMap<String, i64> = HashMap::new();
    for (pod_id, res) in contrib_pods.iter().zip(contribs) {
        let c = res.map_err(|e| format!("could not read contributions for pod {pod_id}: {e}"))?;
        contributed.insert(pod_id.clone(), c);
    }

    // sim: inject the pretend paycheck into the pool (shown + planned against)
    if sim.deposit_cents != 0 {
        *balances
            .entry(assembled.budget.pool_pod_id.clone())
            .or_insert(0) += sim.deposit_cents;
    }

    // A simulation skips the reservation — see `SimInput::is_sim`.
    let reserved = if sim.is_sim() {
        HashMap::new()
    } else {
        pending_debit_reservations(&client, &assembled, &balances, today, &sched).await
    };

    // funding map: real balances, contribution pods swapped to their ledger value,
    // and any uncleared scheduled debit reserved out (so the plan funds to cover it).
    let mut funding = balances.clone();
    for (pod_id, c) in &contributed {
        funding.insert(pod_id.clone(), *c);
    }
    for (pod_id, r) in &reserved {
        let e = funding.entry(pod_id.clone()).or_insert(0);
        *e = (*e - r).max(0);
    }

    let transfers = plan(&assembled.budget, &funding, today, &sched, strategy);

    // have = real balance; target/need/give vary by kind (see LineKind)
    let mut lines = Vec::new();
    for cat in &assembled.budget.categories {
        for bill in &cat.bills {
            let current = balances.get(&bill.pod_id).copied().unwrap_or(0);
            let reserved_c = reserved.get(&bill.pod_id).copied().unwrap_or(0);
            let give: i64 = transfers
                .iter()
                .filter(|t| t.to_pod == bill.pod_id)
                .map(|t| t.amount_cents)
                .sum();
            let (kind, target, need) = match &bill.frequency {
                Frequency::Paycheck => {
                    let contributed = contributed.get(&bill.pod_id).copied().unwrap_or(0);
                    (
                        LineKind::Topup,
                        bill.amount_cents,
                        (bill.amount_cents - contributed).max(0),
                    )
                }
                Frequency::Hold => (
                    LineKind::Hold,
                    bill.amount_cents,
                    (bill.amount_cents - current).max(0),
                ),
                Frequency::Drawdown {
                    target: anchor,
                    period_months,
                } => {
                    let on_track = drawdown_on_track(
                        bill.amount_cents,
                        *anchor,
                        *period_months,
                        today,
                        &sched,
                    );
                    (LineKind::Drawdown, on_track, (on_track - current).max(0))
                }
                _ => {
                    let t = bill_target(bill, today, &sched);
                    (LineKind::Sinking, t, (t - (current - reserved_c)).max(0))
                }
            };
            lines.push(BillLine {
                group: cat.name.clone(),
                name: bill.name.clone(),
                pod_id: bill.pod_id.clone(),
                current,
                target,
                need,
                give,
                reserved: reserved_c,
                kind,
            });
        }
    }

    let pool_balance = balances
        .get(&assembled.budget.pool_pod_id)
        .copied()
        .unwrap_or(0);

    Ok(Report {
        today,
        pool_pod_id: assembled.budget.pool_pod_id,
        pool_balance,
        transfers,
        lines,
        not_funded: assembled.not_funded,
        migrate: assembled.migrate,
        ignored: assembled.ignored,
        notes,
        warnings,
        executed: false,
        names,
    })
}

/// Run one cycle: assess, then execute each transfer the phase permits. Every leg is
/// logged — `[EXEC]` when moved, `[SHADOW]` when only computed — so the log is
/// complete in all phases. A leg moves only when not dry-run AND the phase covers it.
pub async fn run_cycle(cfg: &Config) -> Result<Report, Box<dyn std::error::Error + Send + Sync>> {
    let mut report = assess(cfg).await?;
    let client = cfg.client();
    let pool = report.pool_pod_id.clone();
    let name = |id: &str| {
        report
            .names
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_string())
    };

    let mut any_executed = false;
    for t in &report.transfers {
        if t.amount_cents < 100 {
            continue; // API floor is $1.00; skip dust
        }
        let leg = if t.from_pod == pool {
            Leg::TopLevel
        } else {
            Leg::WithinGroup
        };
        let will_exec = !cfg.dry_run && phase_executes(cfg.phase, leg);
        let (from, to) = (name(&t.from_pod), name(&t.to_pod));
        if will_exec {
            crate::fetch::create_transfer(&client, &t.from_pod, &t.to_pod, t.amount_cents).await?;
            any_executed = true;
            tracing::info!(tag = "EXEC", leg = ?leg, amount_cents = t.amount_cents, %from, %to, "moved money");
        } else {
            tracing::info!(tag = "SHADOW", leg = ?leg, amount_cents = t.amount_cents, %from, %to, "would move (not executed)");
        }
    }
    report.executed = any_executed;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(name: &str, current: i64, target: i64, kind: LineKind) -> BillLine {
        BillLine {
            group: "g".into(),
            name: name.into(),
            pod_id: format!("pod-{name}"),
            current,
            target,
            need: (target - current).max(0),
            give: 0,
            reserved: 0,
            kind,
        }
    }

    fn report_with(lines: Vec<BillLine>) -> Report {
        Report {
            today: NaiveDate::from_ymd_opt(2026, 5, 30).unwrap(),
            pool_pod_id: "pool".into(),
            pool_balance: 0,
            transfers: vec![],
            lines,
            not_funded: vec![],
            migrate: vec![],
            ignored: vec![],
            notes: vec![],
            warnings: vec![],
            executed: false,
            names: HashMap::new(),
        }
    }

    #[test]
    fn rebalance_never_touches_hold_pods() {
        let plan = rebalance_plan(&report_with(vec![
            line("stash", 5_500, 5_000, LineKind::Hold), // above level — NOT skimmed
            line("spent", 1_000, 5_000, LineKind::Hold), // below level — funding refills it, not rebalance
        ]));
        assert!(plan.skims.is_empty() && plan.fills.is_empty());
    }

    #[test]
    fn rebalance_skips_a_pod_whose_debit_hasnt_cleared() {
        // New-cycle pace is low ($50) and the pod looks overfunded ($600), but that
        // $600 is last cycle's debit not yet cleared (reserved) → must NOT be skimmed.
        let mut l = line("carpay", 60_000, 5_000, LineKind::Sinking);
        l.reserved = 60_000;
        let plan = rebalance_plan(&report_with(vec![l]));
        assert!(plan.skims.is_empty(), "reserved pod must not be skimmed");
    }

    #[test]
    fn rebalance_skims_ahead_fills_behind_holds_boundary_skips_contributions() {
        let plan = rebalance_plan(&report_with(vec![
            line("ahead", 10_500, 10_000, LineKind::Sinking), // $5 over -> skim $5
            line("behind", 9_700, 10_000, LineKind::Sinking), // $3 under -> fill $3
            line("justdue", 5_000, 0, LineKind::Sinking),     // target $0, holding -> boundary
            line("contrib", 0, 20_000, LineKind::Topup),      // contribution -> never touched
        ]));
        assert_eq!(plan.skims, vec![("ahead".into(), "pod-ahead".into(), 500)]);
        assert_eq!(
            plan.fills,
            vec![("behind".into(), "pod-behind".into(), 300)]
        );
        assert_eq!(plan.boundary, 1); // just-due pod left alone, not drained
    }

    #[test]
    fn rebalance_respects_the_one_dollar_floor() {
        let plan = rebalance_plan(&report_with(vec![
            line("tiny_over", 10_050, 10_000, LineKind::Sinking), // $0.50 over -> ignored
            line("tiny_under", 9_950, 10_000, LineKind::Sinking), // $0.50 under -> filled to $1
        ]));
        assert!(plan.skims.is_empty()); // sub-dollar surplus is left alone
        assert_eq!(
            plan.fills,
            vec![("tiny_under".into(), "pod-tiny_under".into(), 100)] // floored up to $1.00
        );
    }

    #[test]
    fn rebalance_never_touches_a_contribution_even_when_ahead() {
        // have is the real balance now, so a contribution can show current > target — still never skimmed
        let plan = rebalance_plan(&report_with(vec![
            line("saved_ahead", 10_000, 4_000, LineKind::Drawdown), // $60 over pace
            line("topup_full", 75_000, 1_000, LineKind::Topup),     // big real balance
        ]));
        assert!(plan.skims.is_empty());
        assert!(plan.fills.is_empty());
    }

    #[test]
    fn phase_gates_which_legs_execute() {
        // shadow moves nothing
        assert!(!phase_executes(Phase::Shadow, Leg::TopLevel));
        assert!(!phase_executes(Phase::Shadow, Leg::WithinGroup));
        // groups: top-level only (maestro fills groups; Sequence distributes)
        assert!(phase_executes(Phase::Groups, Leg::TopLevel));
        assert!(!phase_executes(Phase::Groups, Leg::WithinGroup));
        // full: everything
        assert!(phase_executes(Phase::Full, Leg::TopLevel));
        assert!(phase_executes(Phase::Full, Leg::WithinGroup));
    }

    #[test]
    fn phase_parses_from_env_strings() {
        use std::str::FromStr;
        assert_eq!(Phase::from_str("shadow").unwrap(), Phase::Shadow);
        assert_eq!(Phase::from_str("GROUPS").unwrap(), Phase::Groups);
        assert_eq!(Phase::from_str("full").unwrap(), Phase::Full);
        assert!(Phase::from_str("nonsense").is_err());
        assert_eq!(Phase::default(), Phase::Shadow); // safe default
    }
}
