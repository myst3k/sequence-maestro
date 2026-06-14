//! Subcommands behind the single `maestro` binary. Each is a thin `run(&Config)`
//! entry point; the logic lives in the library modules.

pub mod cycle;
pub mod daemon;
pub mod discover;
pub mod finances;
pub mod rebalance;
pub mod rules;
pub mod simulate;
pub mod spend;
pub mod tx;
