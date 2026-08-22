//! crust-seed — fully-automatic cross-seeding with Torznab.
//!
//! A Rust rewrite of [cross-seed](https://github.com/cross-seed/cross-seed).
//! Module names map onto the original TypeScript files; each module's header
//! notes its counterpart and any behavioural divergence.

pub mod action;
pub mod arr;
pub mod build_info;
pub mod clients;
pub mod config;
pub mod constants;
pub mod data_files;
pub mod db;
pub mod decide;
pub mod errors;
pub mod health;
pub mod http;
pub mod indexers;
pub mod inject;
pub mod jobs;
pub mod log_watcher;
pub mod logger;
pub mod pipeline;
pub mod prefilter;
pub mod problems;
pub mod push_notifier;
pub mod searchee;
pub mod server;
pub mod torrent;
pub mod torznab;
pub mod user_auth;
pub mod utils;
