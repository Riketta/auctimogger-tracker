//! Driven adapters implementing the core's outbound ports.
//! R1 ships the Blizzard-format source (ACL + conditional GET) and the SQLite
//! repositories; feed rendering/publishing lands with R4.

#![forbid(unsafe_code)]

pub mod addon_sync;
pub mod alert_discord;
pub mod alert_telegram;
pub mod blizzard_forever;
pub mod blizzard_item_info;
pub mod blizzard_oauth;
pub mod blizzard_source;
pub mod clock;
pub mod feed;
pub mod pack;
pub mod record;
pub mod store;
pub mod testdir;
pub mod wowhead_item_info;
