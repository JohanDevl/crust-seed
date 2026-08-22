//! crust-seed — fully-automatic cross-seeding with Torznab.
//!
//! A Rust rewrite of [cross-seed](https://github.com/cross-seed/cross-seed).
//! Module names map onto the original TypeScript files; each module's header
//! notes its counterpart and any behavioural divergence.

pub mod config;
pub mod constants;
pub mod db;
pub mod errors;
pub mod logger;
pub mod utils;
