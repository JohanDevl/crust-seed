//! crust-seed — fully-automatic cross-seeding with Torznab.
//!
//! A Rust rewrite of [cross-seed](https://github.com/cross-seed/cross-seed).
//! Module names map onto the original TypeScript files; each module's header
//! notes its counterpart and any behavioural divergence.

pub mod arr;
pub mod config;
pub mod constants;
pub mod db;
pub mod decide;
pub mod errors;
pub mod http;
pub mod indexers;
pub mod logger;
pub mod prefilter;
pub mod problems;
pub mod searchee;
pub mod torrent;
pub mod torznab;
pub mod utils;
