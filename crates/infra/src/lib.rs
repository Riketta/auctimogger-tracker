//! Driven adapters implementing the core's outbound ports.
//! R1 ships the Blizzard-format source (ACL + conditional GET) and the SQLite
//! repositories; feed rendering/publishing lands with R4.

#![forbid(unsafe_code)]

pub mod blizzard_item_info;
pub mod blizzard_source;
pub mod clock;
pub mod feed;
pub mod store;
