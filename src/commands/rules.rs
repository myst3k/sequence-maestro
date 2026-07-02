//! `maestro rules [filter]` — inspect Sequence rules and the amounts they move,
//! so you can verify rule edits without waiting for the next payday. Read-only.
//! With a filter, shows only rules whose name or any source/destination matches.

use std::collections::HashMap;

use crate::config::Config;
use sequence_rs::model::account::AccountNode;
use sequence_rs::model::rule::{
    PercentageTarget, RuleAction, RuleActionKind, TransferCapPeriod, Trigger,
};
use sequence_rs::prelude::*;
use sequence_rs::ListRulesParams;

use crate::money::dollars as money;

fn cap_period(p: &TransferCapPeriod) -> &'static str {
    match p {
        TransferCapPeriod::PerTransfer => "transfer",
        TransferCapPeriod::PerWeek => "week",
        TransferCapPeriod::PerMonth => "month",
        TransferCapPeriod::PerYear => "year",
    }
}

fn node<'a>(n: &'a AccountNode, names: &'a HashMap<String, String>) -> &'a str {
    n.name
        .as_deref()
        .or_else(|| names.get(&n.id).map(String::as_str))
        .unwrap_or(&n.id)
}

fn describe(a: &RuleAction, names: &HashMap<String, String>) -> String {
    let what = match &a.kind {
        RuleActionKind::Fixed { amount_in_cents } => money(*amount_in_cents),
        RuleActionKind::Percentage {
            percentage_value,
            percentage_target,
        } => {
            let of = match percentage_target {
                PercentageTarget::IncomingAmount => "of incoming",
                PercentageTarget::SourceAccount => "of balance",
            };
            // the API reports percentageValue on the 0–100 scale
            format!("{percentage_value}% {of}")
        }
        RuleActionKind::TopUp {
            amount_in_cents: Some(c),
            ..
        } => format!("top up to {}", money(*c)),
        RuleActionKind::TopUp { .. } => "top up (dynamic)".to_string(),
        RuleActionKind::RoundDown { amount_in_cents } => {
            format!("round down {}", money(*amount_in_cents))
        }
        _ => "dynamic".to_string(),
    };
    let cap = match &a.base.limit {
        Some(c) => format!(
            "   [≤ {} / {}]",
            money(c.amount_in_cents),
            cap_period(&c.period)
        ),
        None => String::new(),
    };
    format!(
        "{} → {}   {}{}",
        node(&a.base.source, names),
        node(&a.base.destination, names),
        what,
        cap
    )
}

pub async fn run(
    cfg: &Config,
    filter: Option<&str>,
    json: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = cfg.client();
    let accts = crate::fetch::accounts(&client).await?;
    let names: HashMap<String, String> = accts
        .iter()
        .map(|a| (a.id.clone(), a.name.clone()))
        .collect();

    let list = client
        .rules(&ListRulesParams {
            page_size: Some(100),
            ..Default::default()
        })
        .await?;
    let needle = filter.map(|s| s.to_lowercase());

    // Each supported rule's full detail, fetched serially: it's a handful of GETs,
    // and a listing with silently dropped rules reads as "that rule doesn't exist".
    let supported: Vec<_> = list.items.iter().filter(|s| s.is_supported).collect();
    let mut details = Vec::with_capacity(supported.len());
    for s in &supported {
        details.push(
            client
                .rule(&s.id)
                .await
                .map_err(|e| format!("could not read rule {}: {e}", s.id))?,
        );
    }

    for (summary, rule) in supported.iter().zip(details) {
        let name_hit = summary
            .name
            .as_deref()
            .map(|n| n.to_lowercase())
            .zip(needle.as_ref())
            .map(|(n, f)| n.contains(f))
            .unwrap_or(false);
        let mut lines = Vec::new();
        let mut hit = name_hit;
        for step in &rule.steps {
            for a in &step.actions {
                if let Some(f) = &needle {
                    if node(&a.base.source, &names).to_lowercase().contains(f)
                        || node(&a.base.destination, &names).to_lowercase().contains(f)
                    {
                        hit = true;
                    }
                }
                lines.push(describe(a, &names));
            }
        }
        if needle.is_some() && !hit {
            continue;
        }

        // Full fidelity: dump the complete rule, nothing summarized away.
        if json {
            println!("{}", serde_json::to_string_pretty(&rule)?);
            continue;
        }

        let trig = match &rule.trigger {
            Trigger::OnFundsTransferred { account_id } => format!(
                "on deposit → {}",
                names
                    .get(account_id)
                    .map(String::as_str)
                    .unwrap_or(account_id)
            ),
            Trigger::Scheduled { schedule_type, .. } => format!("scheduled {schedule_type:?}"),
            Trigger::Manual { .. } => "manual".to_string(),
        };
        println!(
            "\n● {}  [{:?}]  ({trig})",
            rule.name.as_deref().unwrap_or("(unnamed)"),
            rule.status,
        );
        for l in lines {
            println!("   {l}");
        }
        // The flat summary can't represent conditional/multi-step rules faithfully.
        let conditional = rule.steps.len() > 1 || rule.steps.iter().any(|s| s.conditions.is_some());
        if conditional {
            println!("   (conditional/multi-step — run with --json for the full rule)");
        }
    }
    Ok(())
}
