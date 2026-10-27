//! Item-info source chaining: several [`ItemInfoPort`] adapters tried in
//! order, so a credential-free source can carry the daily load and an
//! authenticated one only sees what the first could not resolve.

use std::sync::Arc;

use crate::{
    domain::{
        Flavor,
        NameRecord,
        PetNameRecord,
    },
    error::SourceError,
    ports::ItemInfoPort,
};

/// Tries its legs in order per lookup. A leg answers one of three ways —
/// resolved (`Ok(Some)`), not-in-this-source (`Ok(None)`), or source trouble
/// (`Err`). Only a resolved record short-circuits; `Ok(None)` falls through
/// to the next leg (flavor coverage differs between sources), and an `Err`
/// is never mistaken for "does not exist": it falls through to the next leg
/// and surfaces only when NO leg resolves — so transient outages are never
/// negative-cached as missing (which the caller would otherwise lock in).
pub struct ChainedItemInfo {
    legs: Vec<Arc<dyn ItemInfoPort>>,
}

impl ChainedItemInfo {
    /// Builds the chain; the first leg is tried first per lookup.
    #[must_use]
    pub fn new(legs: Vec<Arc<dyn ItemInfoPort>>) -> Self {
        Self { legs }
    }
}

impl ItemInfoPort for ChainedItemInfo {
    fn describe(&self, flavor: &Flavor, item_id: i64) -> Result<Option<NameRecord>, SourceError> {
        let mut last_error = None;
        for leg in &self.legs {
            match leg.describe(flavor, item_id) {
                Ok(Some(record)) => return Ok(Some(record)),
                Ok(None) => {}
                Err(error) => last_error = Some(error),
            }
        }
        last_error.map_or(Ok(None), Err)
    }

    fn describe_pet(
        &self,
        flavor: &Flavor,
        species_id: i64,
    ) -> Result<Option<PetNameRecord>, SourceError> {
        let mut last_error = None;
        for leg in &self.legs {
            match leg.describe_pet(flavor, species_id) {
                Ok(Some(record)) => return Ok(Some(record)),
                Ok(None) => {}
                Err(error) => last_error = Some(error),
            }
        }
        last_error.map_or(Ok(None), Err)
    }
}

#[cfg(test)]
mod tests {
    // Test assertions may unwrap/expect/panic: fixture-driven, deterministic
    // inputs (the SecOutfall precedent).
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::{
        Mutex,
        PoisonError,
    };

    use super::*;

    /// One scripted leg: `item_id + shift` picks the outcome —
    /// `1` resolves, `2` answers missing, anything else fails. The shift lets
    /// a later leg succeed where the first missed the same lookup.
    struct ScriptedLeg {
        shift: i64,
        outcomes: Mutex<Vec<(Flavor, i64)>>,
    }

    impl ScriptedLeg {
        fn new(shift: i64) -> Self {
            Self { shift, outcomes: Mutex::new(Vec::new()) }
        }

        fn answer(&self, flavor: &Flavor, item_id: i64) -> Outcome {
            let effective = item_id + self.shift;
            self.outcomes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((flavor.clone(), item_id));
            match effective {
                1 => Outcome::Named("First Leg Name"),
                2 => Outcome::Missing,
                _ => Outcome::Fails,
            }
        }
    }

    enum Outcome {
        Named(&'static str),
        Missing,
        Fails,
    }

    impl ItemInfoPort for ScriptedLeg {
        fn describe(
            &self,
            flavor: &Flavor,
            item_id: i64,
        ) -> Result<Option<NameRecord>, SourceError> {
            match self.answer(flavor, item_id) {
                Outcome::Named(name) => Ok(Some(record(item_id, name))),
                Outcome::Missing => Ok(None),
                Outcome::Fails => Err(SourceError::Http("scripted outage".into())),
            }
        }

        fn describe_pet(
            &self,
            _flavor: &Flavor,
            _species_id: i64,
        ) -> Result<Option<PetNameRecord>, SourceError> {
            Ok(None)
        }
    }

    fn record(item_id: i64, name: &str) -> NameRecord {
        NameRecord {
            item_id,
            name: name.to_string(),
            item_class: None,
            item_subclass: None,
            inventory_slot: None,
            quality: None,
            icon: None,
            sell_price: None,
            fetched_at: 0,
        }
    }

    fn retail() -> Flavor {
        Flavor("retail".into())
    }

    #[test]
    fn first_resolution_wins_and_short_circuits() {
        let first = Arc::new(ScriptedLeg::new(0));
        let second = Arc::new(ScriptedLeg::new(0));
        let chain = ChainedItemInfo::new(vec![first.clone(), second.clone()]);

        let record = chain.describe(&retail(), 1).unwrap().expect("resolved");
        assert_eq!(record.name, "First Leg Name");
        // The second leg never saw the lookup (short-circuit).
        assert!(second.outcomes.lock().unwrap().is_empty());
    }

    #[test]
    fn missing_falls_through_to_the_next_leg() {
        // Leg 1 misses id 2, leg 2 (shifted) resolves it.
        let first = Arc::new(ScriptedLeg::new(0));
        let second = Arc::new(ScriptedLeg::new(-1));
        let chain = ChainedItemInfo::new(vec![first.clone(), second.clone()]);

        let record = chain.describe(&retail(), 2).unwrap().expect("resolved by leg 2");
        assert_eq!(record.name, "First Leg Name");
        assert_eq!(first.outcomes.lock().unwrap().len(), 1, "leg 1 asked first");
        assert_eq!(second.outcomes.lock().unwrap().len(), 1, "leg 2 asked after");
    }

    /// All legs missing = `Ok(None)` (the caller negative-caches); any leg
    /// erroring = `Err` even when a later leg answered "missing" — a transient
    /// outage must not be remembered as junk.
    #[test]
    fn all_missing_is_none_but_any_error_is_propagated() {
        let chain = ChainedItemInfo::new(vec![
            Arc::new(ScriptedLeg::new(0)),
            Arc::new(ScriptedLeg::new(0)),
        ]);
        assert!(chain.describe(&retail(), 2).unwrap().is_none(), "leg 1 missing, leg 2 missing");

        let chain = ChainedItemInfo::new(vec![Arc::new(ScriptedLeg::new(0))]);
        assert!(chain.describe(&retail(), 3).is_err(), "leg 1 fails → error propagates");
    }

    #[test]
    fn flavor_flows_through_to_every_asked_leg() {
        let first = Arc::new(ScriptedLeg::new(0));
        let chain = ChainedItemInfo::new(vec![first.clone()]);
        let flavor = Flavor("forever".into());
        let _ = chain.describe(&flavor, 1).unwrap();
        let asked = first.outcomes.lock().unwrap();
        let first_ask = asked.first().expect("leg was asked once");
        assert_eq!(*first_ask, (flavor, 1), "the lookup carries the requested flavor");
    }
}
