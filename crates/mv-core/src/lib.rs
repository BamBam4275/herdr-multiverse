//! Primitives shared by every Multiverse crate.
//!
//! No Herdr client, no TOML schema, no CLI parsing — those live in
//! `mv-herdr`, `mv-config`, and `mv-cli` respectively.

pub mod atomic;
pub mod crc32;
pub mod error;
pub mod exit;
pub mod id;
pub mod lock;
pub mod redact;
pub mod secure_open;
pub mod time;
