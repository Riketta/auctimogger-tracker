//! Core error types. Hand-rolled (no error crates) to keep this crate
//! dependency-free; every port maps its transport failures into these.

use std::fmt;

/// Configuration file/structure problems.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid configuration: {}", self.0)
    }
}

impl std::error::Error for ConfigError {}

/// Fetch-side failures raised by `AuctionSourcePort` implementations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    /// The HTTP transport failed (non-success status, connection or timeout).
    Http(String),
    /// The payload could not be parsed into the normalized listing shape.
    Parse(String),
    /// The source cannot be reached with the given configuration (e.g. a
    /// required URL or credential is missing).
    Config(String),
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceError::Http(m) => write!(f, "source http error: {m}"),
            SourceError::Parse(m) => write!(f, "source payload parse error: {m}"),
            SourceError::Config(m) => write!(f, "source configuration error: {m}"),
        }
    }
}

impl std::error::Error for SourceError {}

/// Persistence failures raised by repository implementations. Transports stay
/// opaque: adapters stringify their backend specifics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The requested row/entity does not exist.
    NotFound,
    /// The write violates a uniqueness or state constraint (e.g. a duplicate
    /// watchlist entry).
    Conflict(String),
    /// The storage backend failed for a reason unrelated to the request.
    Backend(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::NotFound => write!(f, "not found"),
            StoreError::Conflict(m) => write!(f, "conflict: {m}"),
            StoreError::Backend(m) => write!(f, "storage backend error: {m}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// Feed rendering failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderError(pub String);

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "render error: {}", self.0)
    }
}

impl std::error::Error for RenderError {}

/// Ingestion cycle failures: anything any stage may raise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CycleError {
    /// The fetch stage failed; nothing was persisted.
    Source(SourceError),
    /// A persistence stage failed; the cycle stopped where it failed.
    Store(StoreError),
}

impl fmt::Display for CycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CycleError::Source(e) => write!(f, "{e}"),
            CycleError::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CycleError {}

impl From<SourceError> for CycleError {
    fn from(e: SourceError) -> Self {
        CycleError::Source(e)
    }
}

impl From<StoreError> for CycleError {
    fn from(e: StoreError) -> Self {
        CycleError::Store(e)
    }
}
