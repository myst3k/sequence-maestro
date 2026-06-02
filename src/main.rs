//! `maestro` — the one binary. A personal money-automation engine over the
//! Sequence Platform. Onboard once with `discover`, then `run` funding cycles.

use clap::{Parser, Subcommand};

use maestro::commands;
use maestro::config::Config;

#[derive(Parser)]
#[command(
    name = "maestro",
    version,
    about = "Personal money-automation engine over Sequence"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Onboard: detect pay cadence, income pool(s), and typical amounts; write maestro.json
    Discover,
    /// Run one funding cycle (dry run unless --execute)
    Run {
        /// Actually move money. Without this, it only reports the plan.
        #[arg(long)]
        execute: bool,
    },
    /// Account + balance snapshot
    Accounts,
    /// Recent transfers for pods matching a name filter (e.g. `maestro tx Auto`)
    Tx {
        /// Case-insensitive substring of the pod name
        filter: String,
    },
    /// Inspect Sequence rules and the amounts they move (e.g. `maestro rules Auto`)
    Rules {
        /// Optional case-insensitive filter on rule name or source/destination
        filter: Option<String>,
        /// Dump the complete rule as raw JSON (nothing omitted)
        #[arg(long)]
        json: bool,
    },
    /// Level sinking-fund pods to pace; skim the ahead, fill the behind, park the net
    Rebalance {
        /// Pod to park the net surplus in (defaults to the discovered reclaim pod)
        #[arg(long)]
        to: Option<String>,
        /// Actually move the money (otherwise dry run)
        #[arg(long)]
        execute: bool,
    },
    /// Run as a long-running daemon: watch for deposits, fund on a schedule, serve HTTP
    Daemon {
        /// Go live — move money when a deposit is detected. Without this it only reports.
        #[arg(long)]
        execute: bool,
    },
    /// What-if: simulate a deposit on a date against live balances (moves nothing)
    Simulate {
        /// Paycheck amount in dollars, e.g. `--deposit 1000`
        #[arg(long)]
        deposit: f64,
        /// The date the paycheck lands, `YYYY-MM-DD`
        #[arg(long)]
        date: chrono::NaiveDate,
    },
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("{e}");
        // Debug form carries the API code/message — hint when the key lacks write perm
        let detail = format!("{e:?}");
        if detail.contains("ACCESS_DENIED") || detail.to_lowercase().contains("permission") {
            eprintln!("hint: this API key can read but can't move money — use a key with transfer/write permission.");
        }
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Colorize only when stdout is a terminal, unless CLICOLOR_FORCE
    let force = std::env::var("CLICOLOR_FORCE").map_or(false, |v| v != "0");
    if !force && !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        colored::control::set_override(false);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        // Plain (no ANSI) when piped/redirected
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    let cli = Cli::parse();
    let mut cfg = Config::load()?;
    match cli.command {
        Command::Discover => commands::discover::run(&cfg).await,
        Command::Run { execute } => {
            if execute {
                cfg.dry_run = false;
            }
            commands::cycle::run(&cfg).await
        }
        Command::Accounts => commands::finances::run(&cfg).await,
        Command::Tx { filter } => commands::tx::run(&cfg, &filter).await,
        Command::Rules { filter, json } => {
            commands::rules::run(&cfg, filter.as_deref(), json).await
        }
        Command::Rebalance { to, execute } => {
            commands::rebalance::run(&cfg, to.as_deref(), execute).await
        }
        Command::Daemon { execute } => {
            if execute {
                cfg.dry_run = false;
            }
            commands::daemon::run(&cfg).await
        }
        Command::Simulate { deposit, date } => commands::simulate::run(&cfg, deposit, date).await,
    }
}
