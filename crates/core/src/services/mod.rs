//! Application services (use cases). They depend only on port traits and
//! domain types; driving adapters call them, driven adapters serve them.

pub mod cycle;
pub mod health;
pub mod search;
pub mod watchlist;
