//! Driving adapters: TOML config loading, the HTTP web/API server, and the
//! poll-loop scheduler.

// The hand-authored OpenAPI document in `web::openapi` is one large `json!`
// literal; its nesting depth exceeds the default macro recursion limit.
#![recursion_limit = "1024"]
#![forbid(unsafe_code)]

pub mod config_file;
pub mod scheduler;
pub mod startup;
pub mod web;
