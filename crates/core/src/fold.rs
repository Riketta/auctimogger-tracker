//! Daily histogram folding — the semantic heart of long-term price memory.
//!
//! Semantics (plan D5): within one UTC day, each `auction_id` is counted
//! **once**, at its **last observed** price/quantity. Hourly snapshots must
//! not re-count a long-lived listing 24x; this keeps server-side percentiles
//! comparable with the in-game addon's dedup'd observations.
//!
//! Pure logic: storage serializes `DayFold` faithfully and hands it back; the
//! derived `ItemDayHistogram` view feeds the metric engine.

use std::collections::{BTreeMap, HashMap};

use crate::domain::{ItemDayHistogram, ItemRef, NormalizedListing, PriceBucket};

/// Mutable per-(target, item, day) fold state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DayFold {
    item: ItemRef,
    entries: HashMap<i64, DayEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DayEntry {
    pub price: i64,
    pub qty: i64,
}

impl DayFold {
    pub fn new(item: ItemRef) -> Self {
        Self {
            item,
            entries: HashMap::new(),
        }
    }

    pub fn item(&self) -> &ItemRef {
        &self.item
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates the fold's auction entries (id, price, qty) — the shape
    /// storage adapters serialize.
    pub fn entries(&self) -> impl Iterator<Item = (i64, DayEntry)> + '_ {
        self.entries.iter().map(|(id, entry)| (*id, *entry))
    }

    /// Rebuilds a fold from serialized parts.
    pub fn from_parts(item: ItemRef, entries: HashMap<i64, DayEntry>) -> Self {
        Self { item, entries }
    }

    /// Applies one snapshot batch for this item. Listings for other items are
    /// ignored (callers may pass whole batches). Last observation of an
    /// auction wins; prices are stable per auction on the wire, quantities may
    /// shrink with partial commodity purchases.
    pub fn apply(&mut self, listings: &[NormalizedListing]) {
        for listing in listings {
            if listing.item_ref != self.item {
                continue;
            }
            self.entries.insert(
                listing.auction_id,
                DayEntry {
                    price: listing.unit_price,
                    qty: listing.quantity,
                },
            );
        }
    }

    /// Derived view: one bucket per distinct unit price. `listing_count` is
    /// the number of unique auctions (not snapshot sightings).
    pub fn histogram(&self) -> ItemDayHistogram {
        let mut buckets: BTreeMap<i64, PriceBucket> = BTreeMap::new();
        let mut qty_sum = 0i64;
        for entry in self.entries.values() {
            let bucket = buckets.entry(entry.price).or_insert(PriceBucket {
                listings: 0,
                qty: 0,
            });
            bucket.listings += 1;
            bucket.qty += entry.qty;
            qty_sum += entry.qty;
        }
        ItemDayHistogram {
            buckets,
            listing_count: self.entries.len() as u64,
            qty_sum,
        }
    }
}

/// Groups a snapshot batch by item, preserving first-appearance order for
/// deterministic processing.
pub fn group_by_item(listings: &[NormalizedListing]) -> Vec<(ItemRef, Vec<NormalizedListing>)> {
    let mut index: HashMap<&ItemRef, usize> = HashMap::new();
    let mut grouped: Vec<(ItemRef, Vec<NormalizedListing>)> = Vec::new();
    for listing in listings {
        let pos = match index.get(&listing.item_ref) {
            Some(pos) => *pos,
            None => {
                grouped.push((listing.item_ref.clone(), Vec::new()));
                let pos = grouped.len() - 1;
                index.insert(&listing.item_ref, pos);
                pos
            }
        };
        grouped[pos].1.push(listing.clone());
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ListingKind, TimeLeftBand};

    fn listing(auction_id: i64, item: &ItemRef, price: i64, qty: i64) -> NormalizedListing {
        NormalizedListing {
            auction_id,
            item_ref: item.clone(),
            kind: ListingKind::Commodity,
            quantity: qty,
            unit_price: price,
            bid: None,
            time_left: TimeLeftBand::VeryLong,
            context: None,
            bonus_lists: Vec::new(),
        }
    }

    fn item(id: i64) -> ItemRef {
        ItemRef::Item { item_id: id }
    }

    #[test]
    fn counts_each_auction_once_with_last_observation_winning() {
        let item = item(1);
        let mut fold = DayFold::new(item.clone());
        fold.apply(&[listing(10, &item, 100, 20), listing(11, &item, 200, 1)]);
        assert_eq!(fold.len(), 2);

        // Same auction re-sighted in a later snapshot: quantity shrank (partial
        // commodity purchase) — the new observation replaces the old one.
        fold.apply(&[listing(10, &item, 100, 5)]);
        assert_eq!(fold.len(), 2, "re-sighting must not add a listing");

        let hist = fold.histogram();
        assert_eq!(hist.listing_count, 2);
        let bucket = hist.buckets.get(&100).expect("bucket for price 100");
        assert_eq!(bucket.listings, 1);
        assert_eq!(bucket.qty, 5, "last observed quantity wins");
        assert_eq!(hist.qty_sum, 6); // auction 10 qty 5 (last) + auction 11 qty 1
    }

    #[test]
    fn merges_same_price_auctions_into_one_bucket() {
        let item = item(2);
        let mut fold = DayFold::new(item.clone());
        fold.apply(&[
            listing(1, &item, 500, 10),
            listing(2, &item, 500, 30),
            listing(3, &item, 600, 1),
        ]);
        let hist = fold.histogram();
        assert_eq!(
            hist.buckets.get(&500),
            Some(&PriceBucket {
                listings: 2,
                qty: 40
            })
        );
        assert_eq!(
            hist.buckets.get(&600),
            Some(&PriceBucket {
                listings: 1,
                qty: 1
            })
        );
        assert_eq!(hist.listing_count, 3);
    }

    #[test]
    fn ignores_listings_of_other_items() {
        let other = item(9);
        let item = item(1);
        let mut fold = DayFold::new(item.clone());
        fold.apply(&[listing(1, &item, 100, 1), listing(2, &other, 999, 5)]);
        assert_eq!(fold.len(), 1);
    }

    #[test]
    fn groups_a_batch_by_item_in_first_seen_order() {
        let a = item(1);
        let b = item(2);
        let batch = vec![
            listing(1, &a, 100, 1),
            listing(2, &b, 200, 1),
            listing(3, &a, 100, 1),
        ];
        let grouped = group_by_item(&batch);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].0, a);
        assert_eq!(grouped[0].1.len(), 2);
        assert_eq!(grouped[1].0, b);
        assert_eq!(grouped[1].1.len(), 1);
    }
}
