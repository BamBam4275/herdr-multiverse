//! The on-disk `.multiverse/` tree.
//!
//! No `herdr agent start` here — that belongs to `mv-engine`, which
//! calls back into `mv-herdr`.

pub mod activity;
pub mod backlog;
pub mod breaker_store;
pub mod gauge;
pub mod halt;
pub mod hostsleep_store;
pub mod index;
pub mod log;
pub mod mail;
pub mod notify;
pub mod notify_episodes;
pub mod nudge_memory;
pub mod paths;
pub mod scratch;
pub mod seatstate_store;
pub mod tasks;
pub mod worktrees;
