//! Personal money-automation engine. The brain is a pure, stateless allocator
//! (declared budget + pod balances + today's date → transfers to fund each
//! bill toward its due date); a long-running daemon schedules and executes it.

pub mod allocator;
pub mod budget;
pub mod cards;
pub mod commands;
pub mod config;
pub mod derive;
pub mod engine;
pub mod fetch;
pub mod model;
pub mod money;
pub mod schedule;
pub mod state;
