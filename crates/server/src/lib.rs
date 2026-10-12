//! Driving adapters: TOML config loading, the HTTP web/API server, and the
//! poll-loop scheduler.

#![forbid(unsafe_code)]

pub mod config_file;
pub mod scheduler;
pub mod web;
