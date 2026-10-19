//! Span metric computation — bit-identical to the in-game addon's
//! `domain/metrics.lua` (golden vectors are asserted in BOTH repos; changing
//! either side without the other must fail a spec).

use crate::domain::{
    ItemDayHistogram,
    MetricBlock,
};

/// Computes one analytics block from the merged item-day histograms of the
/// requested window. Returns `None` when there is no data at all.
///
/// Semantics (identical to `Metrics.summarize` in the addon):
/// - one sample per unique auction at its last observed price of the day
///   (histograms already encode that);
/// - nearest-rank percentiles: `idx = ceil(f · n)` clamped to `[1, n]`;
/// - even-count median/MAD floor the middle-pair average;
/// - `vwap = floor(Σ p·q / Σ q)` with median fallback when `Σ q = 0`.
// Histogram-derived indices are clamped or derived from `samples.len()`, so
// slicing cannot go out of bounds; a `.get()` fallback would silently corrupt
// metrics on a logic bug, while a panic fails loudly. Sample counts sit far
// below every cast boundary; `i64 → f64` only shapes the percentile *index*
// fraction (prices stay `i64`), mirroring the addon's Lua doubles.
#[allow(
    clippy::indexing_slicing,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
#[must_use]
pub fn block_from_histograms(
    histograms: &[ItemDayHistogram],
    sale_rate: Option<f64>,
) -> Option<MetricBlock> {
    let mut samples: Vec<i64> = Vec::new();
    // `qty_sum` saturates like the ranking math: the i128 `value_qty` care
    // below would be undone by an i64 overflow in the denominator.
    let mut qty_sum = 0i64;
    // `price * qty` overflows i64 on extreme-but-legal inputs (huge quantities
    // at huge prices); the accumulator stays i128 so the floored quotient is
    // exact and the parity rule (vwap = floor(Σ p·q / Σ q)) holds for any
    // input the store can carry.
    let mut value_qty = 0i128;
    for histogram in histograms {
        for (price, bucket) in &histogram.buckets {
            for _ in 0..bucket.listings {
                samples.push(*price);
            }
            qty_sum = qty_sum.saturating_add(bucket.qty);
            value_qty += i128::from(*price) * i128::from(bucket.qty);
        }
    }
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();

    let n = samples.len() as i64;
    let last = (n - 1) as usize;
    let percentile = |fraction: f64| -> i64 {
        let idx = (fraction * n as f64).ceil();
        let idx = idx.clamp(1.0, n as f64) as usize;
        samples[idx - 1]
    };
    let mid = ((n + 1) / 2) as usize; // ceil(n / 2)
                                      // `midpoint` (not `(a + b) / 2`): prices/deviations are non-negative, so
                                      // it floors identically while staying overflow-free (clippy::manual_midpoint).
    let median =
        if n % 2 == 1 { samples[mid - 1] } else { i64::midpoint(samples[mid - 1], samples[mid]) };
    let mut deviations: Vec<i64> = samples.iter().map(|p| (p - median).abs()).collect();
    deviations.sort_unstable();
    let mad = if n % 2 == 1 {
        deviations[mid - 1]
    } else {
        i64::midpoint(deviations[mid - 1], deviations[mid])
    };
    let vwap = if qty_sum > 0 {
        // Prices/quantities are non-negative, so the quotient is
        // non-negative; `i64::MAX` saturation is unreachable for any real
        // market (the average of in-range prices is in range) and only
        // guards corrupt rows.
        i64::try_from(value_qty / i128::from(qty_sum)).unwrap_or(i64::MAX)
    } else {
        median
    };

    Some(MetricBlock {
        n,
        min: samples[0],
        p05: percentile(0.05),
        p25: percentile(0.25),
        median,
        p75: percentile(0.75),
        p95: percentile(0.95),
        max: samples[last],
        mad,
        vwap,
        qty_sum,
        listing_count: n,
        sale_rate,
    })
}

#[cfg(test)]
mod tests {
    // Golden-vector construction is allowed to unwrap, index, and cast:
    // fixed synthetic inputs, not hostile data (the SecOutfall precedent).
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss
    )]

    use super::*;
    use crate::domain::PriceBucket;

    /// Builds one item-day histogram from (price, qty) samples — one sample =
    /// one unique auction.
    fn hist_from_samples(samples: &[(i64, i64)]) -> ItemDayHistogram {
        let mut hist = ItemDayHistogram::default();
        for (price, qty) in samples {
            let bucket = hist.buckets.entry(*price).or_insert(PriceBucket { listings: 0, qty: 0 });
            bucket.listings += 1;
            bucket.qty += qty;
        }
        hist.listing_count = samples.len() as u64;
        hist.qty_sum = samples.iter().map(|(_, qty)| qty).sum();
        hist
    }

    fn block(samples: &[(i64, i64)]) -> MetricBlock {
        block_from_histograms(&[hist_from_samples(samples)], None).expect("data present")
    }

    /// GOLDEN VECTOR V1 — odd sample count. Mirrored in the addon repo's
    /// `spec/metrics_parity_spec.lua`.
    #[test]
    fn v1_odd_sample_count() {
        let b = block(&[(100, 10), (200, 20), (300, 30), (400, 40), (500, 50)]);
        assert_eq!(b.n, 5);
        assert_eq!(b.min, 100);
        assert_eq!(b.p05, 100);
        assert_eq!(b.p25, 200);
        assert_eq!(b.median, 300);
        assert_eq!(b.p75, 400);
        assert_eq!(b.p95, 500);
        assert_eq!(b.max, 500);
        assert_eq!(b.mad, 100);
        assert_eq!(b.vwap, 366); // floor(55000 / 150)
        assert_eq!(b.qty_sum, 150);
        assert_eq!(b.listing_count, 5);
    }

    /// GOLDEN VECTOR V2 — even sample count (floor middle-pair median/MAD).
    #[test]
    fn v2_even_sample_count() {
        let b = block(&[(100, 1), (200, 1), (300, 1), (400, 1)]);
        assert_eq!(b.n, 4);
        assert_eq!(b.median, 250);
        assert_eq!(b.mad, 100);
        assert_eq!(b.p05, 100);
        assert_eq!(b.p25, 100);
        assert_eq!(b.p75, 300);
        assert_eq!(b.p95, 400);
        assert_eq!(b.vwap, 250);
    }

    /// GOLDEN VECTOR V3 — single sample.
    #[test]
    fn v3_single_sample() {
        let b = block(&[(700, 3)]);
        assert_eq!(b.n, 1);
        assert_eq!(b.median, 700);
        assert_eq!(b.p05, 700);
        assert_eq!(b.p95, 700);
        assert_eq!(b.mad, 0);
        assert_eq!(b.vwap, 700);
        assert_eq!(b.qty_sum, 3);
    }

    /// Input order must not matter (histograms arrive per-day, unsorted).
    #[test]
    fn v4_unsorted_input_matches_v1() {
        let unsorted = block(&[(500, 50), (100, 10), (400, 40), (200, 20), (300, 30)]);
        let sorted = block(&[(100, 10), (200, 20), (300, 30), (400, 40), (500, 50)]);
        assert_eq!(unsorted, sorted);
    }

    /// Same evidence merged into one bucket must yield the identical block —
    /// bucket-level vwap is exact, percentiles survive the expansion.
    #[test]
    fn bucket_merge_exactness() {
        let expanded = block(&[(100, 10), (100, 20), (100, 30), (250, 5)]);
        let mut merged = ItemDayHistogram::default();
        merged.buckets.insert(
            100,
            PriceBucket { listings: 3, qty: 60 }, // 10 + 20 + 30
        );
        merged.buckets.insert(250, PriceBucket { listings: 1, qty: 5 });
        merged.listing_count = 4;
        merged.qty_sum = 65;
        let from_bucket = block_from_histograms(&[merged], None).expect("data present");

        assert_eq!(from_bucket, expanded);
        assert_eq!(from_bucket.median, 100); // samples: 100,100,100,250 → middle pair 100,100
        assert_eq!(from_bucket.vwap, 111); // floor((100*60 + 250*5) / 65) = floor(111.53)
    }

    #[test]
    fn vwap_falls_back_to_median_when_quantities_are_zero() {
        let b = block(&[(100, 0), (300, 0)]);
        assert_eq!(b.qty_sum, 0);
        assert_eq!(b.vwap, 200, "median fallback");
    }

    #[test]
    fn empty_input_yields_none() {
        assert!(block_from_histograms(&[], None).is_none());
        assert!(block_from_histograms(&[ItemDayHistogram::default()], None).is_none());
    }

    /// Extreme values: `price * qty` products that overflow any i64
    /// accumulator must neither panic (debug builds) nor wrap (release) —
    /// the i128 accumulator keeps the floored quotient exact.
    #[test]
    fn vwap_survives_extreme_price_quantity_products() {
        let mut hist = ItemDayHistogram::default();
        hist.buckets.insert(i64::MAX - 1, PriceBucket { listings: 2, qty: i64::MAX });
        hist.listing_count = 2;
        hist.qty_sum = i64::MAX;
        let b = block_from_histograms(std::slice::from_ref(&hist), None).expect("data present");
        assert_eq!(b.qty_sum, i64::MAX);
        assert_eq!(b.vwap, i64::MAX - 1, "floor((MAX-1) · MAX / MAX)");
    }

    /// Several extreme days accumulate across histograms without overflow:
    /// `qty_sum` stays inside i64 while `Σ p·q` only fits i128.
    #[test]
    fn vwap_mixes_extreme_and_moderate_days() {
        let mut extreme = ItemDayHistogram::default();
        extreme.buckets.insert(i64::MAX - 1, PriceBucket { listings: 1, qty: i64::MAX - 1 });
        extreme.listing_count = 1;
        extreme.qty_sum = i64::MAX - 1;
        let mut moderate = ItemDayHistogram::default();
        moderate.buckets.insert(100, PriceBucket { listings: 1, qty: 1 });
        moderate.listing_count = 1;
        moderate.qty_sum = 1;
        let b = block_from_histograms(&[extreme, moderate], None).expect("data present");
        assert_eq!(b.qty_sum, i64::MAX, "in-range total quantity");
        assert_eq!(b.vwap, i64::MAX - 2, "floor(((MAX-1)² + 100) / MAX)");
    }
}
