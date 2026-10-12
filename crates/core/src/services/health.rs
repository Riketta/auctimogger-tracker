//! Per-target health: newest snapshot per payload kind with its age.

use std::sync::Arc;

use crate::{
    config::TrackerConfig,
    domain::{
        PayloadKind,
        SnapshotMeta,
    },
    error::StoreError,
    ports::{
        ClockPort,
        SnapshotRepository,
    },
};

/// Reports per-target freshness over the snapshot repository.
pub struct HealthService {
    snapshots: Arc<dyn SnapshotRepository>,
    config: TrackerConfig,
    clock: Arc<dyn ClockPort>,
}

/// Freshness of one payload kind for one target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KindHealth {
    /// The payload kind being reported.
    pub kind: PayloadKind,
    /// Newest stored snapshot for the kind, if any exists.
    pub latest: Option<SnapshotMeta>,
    /// Seconds since the newest snapshot was fetched; `None` when the kind
    /// has no snapshot yet.
    pub age_seconds: Option<i64>,
}

/// Freshness of all payload kinds for one target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetHealth {
    /// The target being reported.
    pub target: crate::domain::TargetId,
    /// One row per payload kind, always both kinds in fixed order.
    pub kinds: Vec<KindHealth>,
}

impl HealthService {
    /// Creates the service over the snapshot repository, iterating the
    /// targets configured in `config`.
    pub fn new(
        snapshots: Arc<dyn SnapshotRepository>,
        config: TrackerConfig,
        clock: Arc<dyn ClockPort>,
    ) -> Self {
        Self { snapshots, config, clock }
    }

    /// Collects newest-snapshot freshness for every configured target and
    /// both payload kinds.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when snapshot metadata cannot be read.
    pub fn health(&self) -> Result<Vec<TargetHealth>, StoreError> {
        let now = self.clock.now();
        let mut out = Vec::new();
        for target in &self.config.targets {
            let mut kinds = Vec::new();
            for kind in [PayloadKind::Commodities, PayloadKind::Auctions] {
                let latest = self.snapshots.latest(&target.id, kind)?;
                let age_seconds = latest.as_ref().map(|m| now - m.fetched_at);
                kinds.push(KindHealth { kind, latest, age_seconds });
            }
            out.push(TargetHealth { target: target.id.clone(), kinds });
        }
        Ok(out)
    }
}
