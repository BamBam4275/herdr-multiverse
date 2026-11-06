//! Dispatch policy and orchestration.
//!
//! No clap, no ratatui — those live in `mv-cli` and `mv-board`.

/// Re-exported so a production [`runner::DispatchStore`]/[`runner::Herdr`]
/// adapter built outside this crate (`mv-cli`'s `commands::dispatch`, which
/// has no direct `mv-herdr` dependency of its own — see that crate's
/// `Cargo.toml`) can still name `HerdrCommand`, build `argv::*` commands,
/// and read `envelope::CommandResult` without this workspace growing a
/// second place that depends on `mv-herdr` directly.
pub use mv_herdr;

pub mod blocking_prompt;
pub mod breaker;
pub mod brief;
pub mod codex_trust;
pub mod codex_worktree_permissions;
pub mod council;
pub mod directives;
pub mod dispatch;
pub mod dockerreport;
pub mod fleet_start;
pub mod gateway;
pub mod grok_usage;
pub mod health;
pub mod hostdisk;
pub mod hostsleep;
pub mod lane_diff;
pub mod limits;
pub mod nexus_seed;
pub mod prompts;
pub mod risk_paths;
pub mod runner;
pub mod seatstate;
pub mod selector;
pub mod skills;
pub mod sweep_nudges;
pub mod trust;
pub mod usage;
