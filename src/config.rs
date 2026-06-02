//! Engine configuration, read from the environment (a local `.env` is loaded
//! first via dotenvy). Add settings here as the engine grows.

use envconfig::Envconfig;
use sequence_rs::{Credentials, Sequence};

use crate::allocator::FundingStrategy;
use crate::schedule::PaySchedule;
use crate::state::State;

/// How much of the funding plan the engine is allowed to execute — the rollout
/// dial for handing transfers over from Sequence's rules to maestro one leg at a
/// time. The plan is computed in full regardless; this only gates what moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    /// Move nothing — compute and log the whole plan, Sequence does the work.
    #[default]
    Shadow,
    /// Execute only top-level transfers (Income Fund → group / standalone bill);
    /// leave within-group distribution to Sequence's rules.
    Groups,
    /// Execute every leg — maestro owns the whole flow.
    Full,
}

impl std::str::FromStr for Phase {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "shadow" => Ok(Phase::Shadow),
            "groups" | "group" => Ok(Phase::Groups),
            "full" => Ok(Phase::Full),
            other => Err(format!(
                "unknown MAESTRO_PHASE '{other}' (use shadow|groups|full)"
            )),
        }
    }
}

#[derive(Envconfig, Debug, Clone)]
pub struct Config {
    #[envconfig(from = "SEQUENCE_API_KEY")]
    pub sequence_api_key: String,

    /// When true (default), compute and report only — never move money.
    #[envconfig(from = "MAESTRO_DRY_RUN", default = "true")]
    pub dry_run: bool,

    /// How often the engine runs a funding cycle, in seconds.
    #[envconfig(from = "MAESTRO_INTERVAL_SECS", default = "3600")]
    pub interval_secs: u64,

    /// Path to the generated state file (`discover` writes it).
    #[envconfig(from = "MAESTRO_STATE_PATH", default = "maestro.json")]
    pub state_path: String,

    /// Address the daemon's HTTP server binds to (health/status/trigger).
    #[envconfig(from = "MAESTRO_BIND", default = "0.0.0.0:8080")]
    pub bind: String,

    /// Safety cushion added to every bill's funding target, as a percent (e.g.
    /// `2.5`): the engine funds `bill × (1 + pct/100)`. Contributions are exempt.
    #[envconfig(from = "MAESTRO_BUFFER_PCT", default = "2.5")]
    pub buffer_pct: f64,

    /// Rollout phase: `shadow` (move nothing), `groups` (top-level transfers only),
    /// or `full` (everything). Defaults to the safe `shadow`.
    #[envconfig(from = "MAESTRO_PHASE", default = "shadow")]
    pub phase: Phase,
}

impl Config {
    /// Load `.env` (if present), then read config from the environment.
    pub fn load() -> Result<Self, envconfig::Error> {
        let _ = dotenvy::dotenv();
        Self::init_from_env()
    }

    /// A Sequence client built from the configured API key.
    pub fn client(&self) -> Sequence {
        Sequence::new(Credentials::new(self.sequence_api_key.clone()))
    }

    /// The discovered state file, if onboarding has been run.
    pub fn state_file(&self) -> Option<State> {
        State::load(&self.state_path).ok()
    }

    /// The funding rhythm to plan against. Falls back to the semi-monthly
    /// default (with a caller-visible flag) when no state file exists yet.
    pub fn pay_schedule(&self) -> (PaySchedule, bool) {
        match self.state_file() {
            Some(f) => (f.pay_schedule, true),
            None => (PaySchedule::default(), false),
        }
    }

    /// The rationing strategy from the state file, or the default.
    pub fn funding_strategy(&self) -> FundingStrategy {
        self.state_file()
            .map(|f| f.funding_strategy)
            .unwrap_or_default()
    }
}
