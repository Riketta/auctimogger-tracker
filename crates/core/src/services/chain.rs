//! Item-info source chaining: several [`ItemInfoPort`] adapters tried in
//! order, so a credential-free source can carry the daily load and an
//! authenticated one tops up the fields it cannot see (Wowhead carries the
//! icon and vendor price in one request, Blizzard the class and level).

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

/// Tries its legs in order per lookup, merging what they return. A leg
/// answers one of three ways — resolved (`Ok(Some)`), not-in-this-source
/// (`Ok(None)`), or source trouble (`Err`). A resolved record whose
/// enrichment fields are all present short-circuits the chain; a partial
/// record (a source that cannot see class/level, say) stays as the base and
/// the next leg's fields are merged in, so the cached result carries the
/// union of what the legs know. `Ok(None)` falls through to the next leg
/// (flavor coverage differs between sources), and an `Err` is never
/// mistaken for "does not exist": it falls through and surfaces only when
/// NO leg resolves anything — so transient outages are never
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
        let mut merged: Option<NameRecord> = None;
        let mut last_error = None;
        for leg in &self.legs {
            // A complete record is terminal: no later leg could contribute
            // anything, so asking one would only burn a request.
            if merged.as_ref().is_some_and(NameRecord::is_enrichment_complete) {
                break;
            }
            match leg.describe(flavor, item_id) {
                Ok(Some(record)) => {
                    merged = Some(match merged {
                        Some(base) => base.merged_with(record),
                        None => record,
                    });
                }
                Ok(None) => {}
                Err(error) => last_error = Some(error),
            }
        }
        match merged {
            Some(record) => Ok(Some(record)),
            None => last_error.map_or(Ok(None), Err),
        }
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
    /// a later leg succeed where the first missed the same lookup. Legs built
    /// with [`Self::fully_enriched`] answer with complete records (the
    /// Blizzard shape); plain legs answer sparse ones (the Wowhead shape), so
    /// tests can pin which answers terminate the chain and which merge on.
    struct ScriptedLeg {
        shift: i64,
        enrichment: bool,
        outcomes: Mutex<Vec<(Flavor, i64)>>,
    }

    impl ScriptedLeg {
        fn new(shift: i64) -> Self {
            Self { shift, enrichment: false, outcomes: Mutex::new(Vec::new()) }
        }

        fn fully_enriched(shift: i64) -> Self {
            Self { shift, enrichment: true, outcomes: Mutex::new(Vec::new()) }
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

    /// A complete record: every enrichment field present (the Blizzard shape
    /// the merge-stop check recognizes as terminal).
    fn complete_record(item_id: i64, name: &str) -> NameRecord {
        NameRecord {
            item_id,
            name: name.to_string(),
            item_class: Some("Scripted".into()),
            item_subclass: Some("Sub".into()),
            inventory_slot: Some("Slot".into()),
            quality: Some("Common".into()),
            icon: Some("icon".into()),
            sell_price: Some(1),
            item_level: Some(1),
            fetched_at: 0,
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
                Outcome::Named(name) if self.enrichment => Ok(Some(complete_record(item_id, name))),
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
            item_level: None,
            fetched_at: 0,
        }
    }

    fn retail() -> Flavor {
        Flavor("retail".into())
    }

    #[test]
    fn first_resolution_wins_and_short_circuits() {
        let first = Arc::new(ScriptedLeg::fully_enriched(0));
        let second = Arc::new(ScriptedLeg::fully_enriched(0));
        let chain = ChainedItemInfo::new(vec![first.clone(), second.clone()]);

        let record = chain.describe(&retail(), 1).unwrap().expect("resolved");
        assert_eq!(record.name, "First Leg Name");
        // A complete record is terminal: the second leg never saw the lookup.
        assert!(second.outcomes.lock().unwrap().is_empty());
    }

    /// A partial hit (the Wowhead shape: name but no class/level) hands off
    /// to the next leg, whose fields merge into the cached result.
    #[test]
    fn a_partial_hit_merges_the_next_legs_fields() {
        let first = Arc::new(ScriptedLeg::new(0));
        let second = Arc::new(ScriptedLeg::fully_enriched(0));
        let chain = ChainedItemInfo::new(vec![first.clone(), second.clone()]);

        let record = chain.describe(&retail(), 1).unwrap().expect("merged record");
        assert_eq!(record.name, "First Leg Name", "the first leg keeps the name");
        assert_eq!(record.item_class.as_deref(), Some("Scripted"), "class merged from leg 2");
        assert_eq!(record.item_level, Some(1), "level merged from leg 2");
        assert_eq!(second.outcomes.lock().unwrap().len(), 1, "the incomplete answer asked leg 2");
    }

    /// A partial hit survives a later leg's outage: source trouble is never
    /// allowed to discard data already in hand (and is not negative-cached).
    #[test]
    fn an_error_after_a_partial_hit_keeps_the_record() {
        let first = Arc::new(ScriptedLeg::new(0));
        // shift 5 makes id 1 fail on the second leg (effective 6).
        let second = Arc::new(ScriptedLeg::fully_enriched(5));
        let chain = ChainedItemInfo::new(vec![first, second]);

        let record = chain
            .describe(&retail(), 1)
            .expect("partial record is not an error")
            .expect("record kept");
        assert_eq!(record.name, "First Leg Name");
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
