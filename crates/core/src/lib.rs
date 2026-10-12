//! auctimogger-tracker core: domain types, port traits, application services.
//!
//! This crate depends on nothing outside `std` — that is compile-enforced
//! (see `tests/boundary.rs` and the workspace dependency graph) and is the
//! hexagonal contract of the service. Infrastructure adapters live in
//! `amtracker-infra`, driving adapters (HTTP, scheduler, CLI) in
//! `amtracker-server`; `main.rs` is the composition root.

#![forbid(unsafe_code)]

pub mod config;
pub mod domain;
pub mod error;
pub mod fold;
pub mod metrics;
pub mod ports;
pub mod services;
