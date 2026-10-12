//! Per-target health: newest snapshot per payload kind with its age.

use std::sync::Arc;

use crate::config::TrackerConfig;
use crate::domain::{PayloadKind, SnapshotMeta};
use crate::error::StoreError;
use crate::ports::{ClockPort, SnapshotRepository};

pub struct HealthService {
    snapshots: Arc<dyn SnapshotRepository>,
    config: TrackerConfig,
    clock: Arc<dyn ClockPort>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KindHealth {
    pub kind: PayloadKind,
    pub latest: Option<SnapshotMeta>,
    pub age_seconds: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetHealth {
    pub target: crate::domain::TargetId,
    pub kinds: Vec<KindHealth>,
}

impl HealthService {
    pub fn new(
        snapshots: Arc<dyn SnapshotRepository>,
        config: TrackerConfig,
        clock: Arc<dyn ClockPort>,
    ) -> Self {
        Self {
            snapshots,
            config,
            clock,
        }
    }

    pub fn health(&self) -> Result<Vec<TargetHealth>, StoreError> {
        let now = self.clock.now();
        let mut out = Vec::new();
        for target in &self.config.targets {
            let mut kinds = Vec::new();
            for kind in [PayloadKind::Commodities, PayloadKind::Auctions] {
                let latest = self.snapshots.latest(&target.id, kind)?;
                let age_seconds = latest.as_ref().map(|m| now - m.fetched_at);
                kinds.push(KindHealth {
                    kind,
                    latest,
                    age_seconds,
                });
            }
            out.push(TargetHealth {
                target: target.id.clone(),
                kinds,
            });
        }
        Ok(out)
    }
}
