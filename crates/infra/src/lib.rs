//! Driven adapters implementing the core's outbound ports.
//! R0 ships the clock; SQLite repositories and the Blizzard-format source
//! adapter land with R1, feed rendering with R4.

#![forbid(unsafe_code)]

pub mod clock;
