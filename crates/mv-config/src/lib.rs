//! Config loading and the role vocabulary.
//!
//! No dispatch policy — that belongs to `mv-engine`.
//!
//! `mv-core` note: this crate declares `mv-core` as a dependency
//! (`Cargo.toml`) per `architecture.md`'s crate DAG, and `mv-core`'s own
//! modules (`crates/mv-core/src/*.rs`) have since landed for real. Every
//! type this crate would otherwise have pulled from `mv-core` (its own
//! config error, timestamp formatting/parsing) is still instead a small
//! private equivalent in [`error`] / [`load`], the same way
//! `mv-herdr::error::HerdrError` chose to stay self-contained rather than
//! block on `mv-core`. These remain candidates to switch over to the
//! `mv-core` equivalents, but nothing here currently depends on that
//! happening.

pub mod edit;
pub mod error;
pub mod load;
pub mod migrate;
pub mod placeholder;
pub mod roles;
pub mod roster;
