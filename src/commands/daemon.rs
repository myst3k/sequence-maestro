//! `maestro daemon` — long-running engine. Runs a funding cycle when a new
//! deposit lands; serves health/status/manual-trigger over HTTP. In-process
//! scheduling, no cron.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use sequence_rs::model::account::AccountType;
use sequence_rs::model::transfer::TransferDirection;
use sequence_rs::prelude::*;
use sequence_rs::{ListAccountTransfersParams, ListAccountsParams};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};

use crate::config::Config;
use crate::engine::{render, run_cycle, Report};
use crate::money::dollars as fmt_money;

/// A serializable summary of one cycle, for `/status` and logs.
#[derive(Serialize, Clone, Default)]
struct CycleSummary {
    ran_at: String,
    executed: bool,
    pool_balance_cents: i64,
    total_need_cents: i64,
    total_funded_cents: i64,
    transfers: usize,
    warnings: Vec<String>,
}

fn summarize(report: &Report) -> CycleSummary {
    CycleSummary {
        ran_at: Utc::now().to_rfc3339(),
        executed: report.executed,
        pool_balance_cents: report.pool_balance,
        total_need_cents: report.lines.iter().map(|l| l.need).sum(),
        total_funded_cents: report
            .transfers
            .iter()
            .filter(|t| t.from_pod == report.pool_pod_id)
            .map(|t| t.amount_cents)
            .sum(),
        transfers: report.transfers.len(),
        warnings: report.warnings.clone(),
    }
}

struct AppState {
    cfg: Config,
    last: RwLock<Option<CycleSummary>>,
}

type Shared = Arc<AppState>;

pub async fn run(cfg: &Config) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let state: Shared = Arc::new(AppState {
        cfg: cfg.clone(),
        last: RwLock::new(None),
    });

    log_startup(&state.cfg);

    let app = Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/run", post(trigger))
        .with_state(state.clone());

    let listener = TcpListener::bind(&state.cfg.bind).await?;
    tracing::info!(addr = %state.cfg.bind, "HTTP up — GET /health, GET /status, POST /run");

    // boot snapshot is always report-only — money moves only on a detected deposit
    tracing::info!("initial snapshot — report only, moves nothing on boot");
    let mut snapshot_cfg = state.cfg.clone();
    snapshot_cfg.dry_run = true;
    run_and_store(&state, &snapshot_cfg).await;

    tokio::spawn(monitor(state.clone()));
    tracing::info!(
        interval_secs = state.cfg.interval_secs.max(60),
        "watching income for new deposits"
    );

    axum::serve(listener, app).await?;
    Ok(())
}

/// Log what the daemon loaded and what it'll do — so a fresh boot explains itself.
fn log_startup(cfg: &Config) {
    let (sched, onboarded) = cfg.pay_schedule();
    tracing::info!("── maestro daemon ──");
    tracing::info!(phase = ?cfg.phase, "rollout phase (shadow=move nothing, groups=top-level only, full=everything)");
    if cfg.dry_run {
        tracing::info!(
            "mode: DRY RUN — reports only, moves no money (MAESTRO_DRY_RUN=false to go live)"
        );
    } else {
        tracing::warn!("mode: LIVE — money moves per the rollout phase above");
    }
    if !onboarded {
        tracing::warn!("no maestro.json — run `maestro discover` first; using defaults");
    }
    tracing::info!(schedule = ?sched, strategy = ?cfg.funding_strategy(), "funding config");
    if let Some(f) = cfg.state_file() {
        let pools: Vec<&str> = f.pools.iter().map(|p| p.name.as_str()).collect();
        let income: Vec<&str> = f.income_sources.iter().map(|s| s.name.as_str()).collect();
        tracing::info!(?pools, ?income, "pools funded from / income watched");
    }
}

async fn health() -> &'static str {
    "ok"
}

async fn status(State(s): State<Shared>) -> Json<serde_json::Value> {
    match s.last.read().await.clone() {
        Some(sum) => Json(serde_json::json!({ "last_cycle": sum })),
        None => Json(serde_json::json!({ "last_cycle": null, "note": "no cycle run yet" })),
    }
}

/// `POST /run` — trigger a cycle on demand (respects MAESTRO_DRY_RUN).
async fn trigger(State(s): State<Shared>) -> Json<serde_json::Value> {
    match run_cycle(&s.cfg).await {
        Ok(report) => {
            let sum = summarize(&report);
            *s.last.write().await = Some(sum.clone());
            tracing::info!(transfers = sum.transfers, "manual cycle complete");
            Json(serde_json::json!({ "ok": true, "cycle": sum }))
        }
        Err(e) => {
            tracing::error!(error = %e, "manual cycle failed");
            Json(serde_json::json!({ "ok": false, "error": e.to_string() }))
        }
    }
}

/// Poll income sources; when a new deposit appears, run a funding cycle.
async fn monitor(state: Shared) {
    let interval = Duration::from_secs(state.cfg.interval_secs.max(60));
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut first = true;

    loop {
        match latest_deposits(&state.cfg).await {
            Ok(latest) => {
                let mut new_deposit = false;
                for (id, ts) in &latest {
                    if seen.get(id).map_or(true, |prev| ts > prev) {
                        if !first {
                            new_deposit = true;
                        }
                        seen.insert(id.clone(), ts.clone());
                    }
                }
                if first {
                    tracing::info!(sources = latest.len(), "monitor baseline established");
                    first = false;
                } else if new_deposit {
                    tracing::info!("new deposit detected — running funding cycle");
                    run_and_store(&state, &state.cfg).await;
                } else {
                    tracing::debug!(sources = latest.len(), "polled — no new deposit");
                }
            }
            Err(e) => tracing::error!(error = %e, "deposit poll failed"),
        }
        sleep(interval).await;
    }
}

async fn run_and_store(state: &Shared, cfg: &Config) {
    match run_cycle(cfg).await {
        Ok(report) => {
            print!("{}", render(&report));
            let sum = summarize(&report);
            tracing::info!(
                mode = if sum.executed { "executed" } else { "dry" },
                pool = fmt_money(sum.pool_balance_cents),
                need = fmt_money(sum.total_need_cents),
                funded = fmt_money(sum.total_funded_cents),
                transfers = sum.transfers,
                warnings = sum.warnings.len(),
                "funding cycle complete"
            );
            *state.last.write().await = Some(sum);
        }
        Err(e) => tracing::error!(error = %e, "funding cycle failed"),
    }
}

/// Newest `MoneyIn` timestamp per income source (ISO strings sort lexically).
async fn latest_deposits(
    cfg: &Config,
) -> Result<HashMap<String, String>, Box<dyn std::error::Error + Send + Sync>> {
    let client = cfg.client();
    let page = client
        .accounts(&ListAccountsParams {
            page_size: Some(100),
            ..Default::default()
        })
        .await?;
    let mut out = HashMap::new();
    for a in &page.items {
        if a.account_type != AccountType::IncomeSource {
            continue;
        }
        let transfers = client
            .account_transfers(
                &a.id,
                &ListAccountTransfersParams {
                    page_size: Some(5),
                    ..Default::default()
                },
            )
            .await?;
        if let Some(newest) = transfers
            .items
            .iter()
            .filter(|t| t.direction == TransferDirection::MoneyIn)
            .map(|t| t.created_at.clone())
            .max()
        {
            out.insert(a.id.clone(), newest);
        }
    }
    Ok(out)
}
