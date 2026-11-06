//! A sync client for the `herdr` CLI.
//!
//! No fleet lock, no task/brief model — those live in `mv-state` and
//! `mv-engine`.

pub mod argv;
pub mod envelope;
pub mod error;
pub mod run;
pub mod shapes;
