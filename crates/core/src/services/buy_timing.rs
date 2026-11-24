//! Buy-timing aggregation: pure folds answering "which weekday and which
//! hour of day is this identity usually cheapest" for the item page's
//! when-to-buy view. Inputs are store reads the caller already made — the
//! permanent intraday hour fold ([`ItemHourRow`]) and the per-day aggregate
//! rows (folds, weekday) — so everything here is deterministic and
//! table-testable.
//!
//! Buckets are UTC-shaped on purpose: the server SVG fallback renders UTC,
//! and the browser layer re-buckets from the raw `(day, hour)` pairs into
//! the viewer's local timezone (a pre-aggregated local bucket could not be
//! un-shifted again).

use crate::domain::{
    ItemDayRow,
    ItemHourRow,
};

/// One hour-of-day bucket over one UTC day: how cheap the identity was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HourBucket {
    /// UTC day number the bucket belongs to.
    pub day: i64,
    /// Hour of the UTC day, `0..=23`.
    pub hour: i64,
    /// Cheapest of the hour's per-snapshot minimum unit prices (copper) —
    /// the outright deal floor seen that hour.
    pub floor: i64,
    /// Median of the hour's per-snapshot minimum unit prices (copper) —
    /// how cheap it USUALLY was that hour (one undercut listing does not
    /// move it).
    pub typical: i64,
    /// Snapshots that observed the identity in this hour.
    pub snapshots: usize,
}

/// One weekday's aggregate over the requested range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WeekdayStat {
    /// `0 = Sunday ..= 6 = Saturday` (the JS `getDay` convention, so the
    /// browser layer can re-bucket by local weekday without remapping).
    pub weekday: usize,
    /// Median of the days' medians (copper) — the robust "typical weekday
    /// price" bar.
    pub median: i64,
    /// Median of the days' p05s (copper) — the robust floor marker.
    pub p05: i64,
    /// Days of evidence behind the numbers.
    pub days: usize,
}

/// The answer to "when is it cheapest": the best weekday and best UTC hour
/// against their reference medians, integer-only (the page renders the
/// percentages). `None` fields mean not enough evidence to say anything.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BuyVerdict {
    /// Cheapest weekday (index, its median) plus the all-weekday median it
    /// is compared against.
    pub weekday: Option<(usize, i64)>,
    /// Median across all weekday medians — the week reference.
    pub week_median: i64,
    /// Cheapest UTC hour (0..=23, its typical price) plus the all-hour
    /// median it is compared against.
    pub hour: Option<(i64, i64)>,
    /// Median of the eligible hours' typicals — the day reference, built
    /// exactly like `week_median`.
    pub hour_median: i64,
}

/// UTC-day → weekday, `0 = Sunday ..= 6 = Saturday`. Day 0 (1970-01-01)
/// was a Thursday (4); `rem_euclid` keeps pre-epoch day numbers honest.
// `rem_euclid(7)` lands in `0..=6`, so the `usize` cast never truncates.
#[allow(clippy::cast_possible_truncation)]
fn weekday_of_day(day: i64) -> usize {
    ((day + 4).rem_euclid(7)) as usize
}

/// Median with the metric engine's convention: nearest-rank for odd counts,
/// floor-even `i64::midpoint` for even counts. Input must be non-empty and
/// is sorted in place.
// Direct indexing is safe: the caller checks non-emptiness and `mid` lies in
// `1..=n`, so both indexes are valid — a `.get()` fallback would silently
// corrupt the statistics (the `price_stats_sorted` precedent).
#[allow(clippy::indexing_slicing)]
fn median_of(sorted: &mut [i64]) -> i64 {
    sorted.sort_unstable();
    let n = sorted.len();
    let mid = n.div_ceil(2);
    if n % 2 == 1 {
        sorted[mid - 1]
    } else {
        i64::midpoint(sorted[mid - 1], sorted[mid])
    }
}

/// Picks the fold's hour cells, sorted by `(day, hour)`, deriving each
/// cell's typical (median of its per-snapshot minimums) and evidence count.
/// Duplicate cells (the store's PK makes them impossible; defensive here)
/// merge by concatenating their minimums. Hours with no snapshots are simply
/// absent — the renderer gaps them instead of inventing zero evidence.
#[must_use]
pub fn hour_buckets(rows: &[ItemHourRow]) -> Vec<HourBucket> {
    let mut by_cell: std::collections::BTreeMap<(i64, i64), Vec<i64>> =
        std::collections::BTreeMap::new();
    for row in rows {
        by_cell.entry((row.day, row.hour)).or_default().extend_from_slice(&row.mins);
    }
    by_cell
        .into_iter()
        .map(|((day, hour), mut mins)| HourBucket {
            day,
            hour,
            floor: *mins.iter().min().unwrap_or(&0),
            typical: median_of(&mut mins),
            snapshots: mins.len(),
        })
        .collect()
}

/// Aggregates day rows into per-weekday stats (one per weekday that has any
/// evidence), sorted by weekday index.
// Direct indexing is safe: `weekday_of_day` returns `0..=6` and both arrays
// hold exactly 7 entries.
#[allow(clippy::indexing_slicing)]
#[must_use]
pub fn weekday_stats(rows: &[ItemDayRow]) -> Vec<WeekdayStat> {
    let mut medians: Vec<Vec<i64>> = (0..7).map(|_| Vec::new()).collect();
    let mut floors: Vec<Vec<i64>> = (0..7).map(|_| Vec::new()).collect();
    for row in rows {
        let weekday = weekday_of_day(row.day);
        medians[weekday].push(row.median);
        floors[weekday].push(row.p05);
    }
    let mut out = Vec::new();
    for weekday in 0..7 {
        // Snapshot the lengths before the in-place sorts borrow them mutably.
        let days = medians[weekday].len();
        if days == 0 {
            continue;
        }
        out.push(WeekdayStat {
            weekday,
            median: median_of(&mut medians[weekday]),
            p05: median_of(&mut floors[weekday]),
            days,
        });
    }
    out
}

/// Distinct days an hour-of-day needs before it may be called the cheapest
/// hour — the hour twin of the weekday rule: one cheap 03:00 on one Tuesday
/// is not "03:00 is cheap".
const MIN_HOUR_DAYS: usize = 2;

/// Picks the cheapest weekday and hour-of-day against their reference
/// medians. Weekdays need ≥ 2 days of evidence (one Sunday is not "Sundays
/// are cheap"); an hour-of-day needs the same, however many snapshots those
/// days hold. Both references are built the same way — the median of the
/// per-weekday, resp. per-hour, medians — so often-observed hours do not
/// out-weigh rarer ones.
#[must_use]
pub fn cheapest_window(weekdays: &[WeekdayStat], buckets: &[HourBucket]) -> BuyVerdict {
    let mut verdict = BuyVerdict::default();
    let eligible: Vec<&WeekdayStat> = weekdays.iter().filter(|stat| stat.days >= 2).collect();
    if !eligible.is_empty() {
        let mut week = eligible.iter().map(|stat| stat.median).collect::<Vec<_>>();
        verdict.week_median = median_of(&mut week);
        if let Some(best) = eligible.iter().min_by_key(|stat| stat.median) {
            verdict.weekday = Some((best.weekday, best.median));
        }
    }
    // Median of each hour's typicals across the days it was observed.
    let mut per_hour: std::collections::BTreeMap<i64, (Vec<i64>, std::collections::BTreeSet<i64>)> =
        std::collections::BTreeMap::new();
    for bucket in buckets {
        let (typicals, days) = per_hour.entry(bucket.hour).or_default();
        typicals.push(bucket.typical);
        days.insert(bucket.day);
    }
    let mut hour_medians: Vec<(i64, i64)> = Vec::new();
    for (hour, (mut typicals, days)) in per_hour {
        if days.len() >= MIN_HOUR_DAYS {
            hour_medians.push((hour, median_of(&mut typicals)));
        }
    }
    if !hour_medians.is_empty() {
        let mut all: Vec<i64> = hour_medians.iter().map(|(_, typical)| *typical).collect();
        verdict.hour_median = median_of(&mut all);
        if let Some(&(hour, typical)) = hour_medians.iter().min_by_key(|(_, typical)| *typical) {
            verdict.hour = Some((hour, typical));
        }
    }
    verdict
}

#[cfg(test)]
mod tests {
    // Test assertions may unwrap/expect/panic and index: fixture-driven,
    // deterministic inputs (the SecOutfall precedent).
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    fn sample(fetched_at: i64, min_price: i64) -> ItemHourRow {
        ItemHourRow {
            day: crate::domain::utc_day(fetched_at),
            hour: fetched_at.rem_euclid(86_400) / 3_600,
            floor: min_price,
            mins: vec![min_price],
        }
    }

    fn day_row(day: i64, median: i64, p05: i64) -> ItemDayRow {
        ItemDayRow {
            item: crate::domain::ItemRef::Item { item_id: 1 },
            category: crate::domain::Category::Commodity,
            day,
            listing_count: 4,
            qty_sum: 40,
            min: p05,
            min_qty: 1,
            p05,
            median,
            qty_median: median,
            mad: 0,
        }
    }

    /// Day 0 (1970-01-01) was a Thursday; the mapping must agree for every
    /// index (JS `getDay` order: Sunday = 0).
    #[test]
    fn weekday_mapping_follows_the_epoch_calendar() {
        assert_eq!(weekday_of_day(0), 4, "1970-01-01 was a Thursday");
        assert_eq!(weekday_of_day(1), 5, "Friday");
        assert_eq!(weekday_of_day(3), 0, "1970-01-04 was a Sunday");
        assert_eq!(weekday_of_day(4), 1, "Monday");
    }

    /// Samples land in their (day, hour) cell with min-of-mins floor and
    /// median-of-mins typical; empty hours stay absent; output is ordered.
    #[test]
    fn samples_bucket_into_hours_with_floor_and_typical() {
        // Day 5: 02:00 has two snapshots (100, 300 → typical 200, floor 100),
        // 05:00 one snapshot, 23:59 one snapshot landing in hour 23.
        let samples = [
            sample(5 * 86_400 + 2 * 3_600, 100),
            sample(5 * 86_400 + 2 * 3_600 + 300, 300),
            sample(5 * 86_400 + 5 * 3_600, 250),
            sample(5 * 86_400 + 86_399, 400),
        ];
        let buckets = hour_buckets(&samples);
        assert_eq!(
            buckets,
            vec![
                HourBucket { day: 5, hour: 2, floor: 100, typical: 200, snapshots: 2 },
                HourBucket { day: 5, hour: 5, floor: 250, typical: 250, snapshots: 1 },
                HourBucket { day: 5, hour: 23, floor: 400, typical: 400, snapshots: 1 },
            ]
        );
    }

    /// Even-count typicals use the floor-even midpoint, matching the metric
    /// engine's median convention.
    #[test]
    fn even_sample_counts_take_the_midpoint_median() {
        let samples =
            [sample(3_600, 100), sample(3_601, 200), sample(3_602, 300), sample(3_603, 400)];
        let buckets = hour_buckets(&samples);
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].typical, 250, "midpoint of 200 and 300");
        assert_eq!(buckets[0].floor, 100);
    }

    /// Weekday stats take robust medians per weekday and skip empty ones;
    /// day rows must land on their calendar weekday.
    #[test]
    fn weekday_stats_aggregate_by_calendar_weekday() {
        // Days 3 (Sunday) ×2 and 4 (Monday) ×1.
        let rows = [day_row(3, 100, 80), day_row(10, 200, 180), day_row(4, 400, 350)];
        let stats = weekday_stats(&rows);
        assert_eq!(stats.len(), 2, "only weekdays with evidence");
        assert_eq!(
            stats[0],
            WeekdayStat { weekday: 0, median: 150, p05: 130, days: 2 },
            "Sunday: midpoint medians of (100,200) and (80,180)"
        );
        assert_eq!(stats[1], WeekdayStat { weekday: 1, median: 400, p05: 350, days: 1 }, "Monday");
    }

    /// The verdict names the cheapest weekday (only with ≥2 days of
    /// evidence) and the cheapest hour against their reference medians.
    #[test]
    fn verdict_picks_cheapest_weekday_and_hour() {
        let weekdays = [
            WeekdayStat { weekday: 0, median: 100, p05: 80, days: 2 }, // Sunday, cheap
            WeekdayStat { weekday: 1, median: 150, p05: 120, days: 2 }, // Monday
            WeekdayStat { weekday: 2, median: 90, p05: 70, days: 1 }, // one-off Tuesday: ineligible
        ];
        let buckets = [
            HourBucket { day: 5, hour: 2, floor: 90, typical: 100, snapshots: 2 },
            HourBucket { day: 6, hour: 2, floor: 110, typical: 120, snapshots: 2 },
            // Hour 14 was seen on ONE day, however cheap: not a pattern.
            HourBucket { day: 5, hour: 14, floor: 10, typical: 20, snapshots: 2 },
        ];
        let verdict = cheapest_window(&weekdays, &buckets);
        assert_eq!(verdict.weekday, Some((0, 100)), "Sunday wins; single-day Tuesday ignored");
        assert_eq!(verdict.week_median, 125, "midpoint of 100 and 150");
        assert_eq!(verdict.hour, Some((2, 110)), "hour 2: median of (100,120)");
        assert_eq!(verdict.hour_median, 110, "the median of the eligible hours' medians");
    }

    /// Every hour seen on a single day: nothing to say about the hour of day.
    #[test]
    fn single_day_hours_answer_nothing() {
        let buckets = [
            HourBucket { day: 5, hour: 2, floor: 90, typical: 100, snapshots: 2 },
            HourBucket { day: 6, hour: 14, floor: 10, typical: 20, snapshots: 9 },
        ];
        let verdict = cheapest_window(&[], &buckets);
        assert_eq!(
            (verdict.hour, verdict.hour_median),
            (None, 0),
            "nine snapshots on one day is still one day"
        );
    }

    /// No evidence → an all-`None` verdict; a single weekday alone says
    /// nothing either.
    #[test]
    fn empty_evidence_answers_nothing() {
        let empty = cheapest_window(&[], &[]);
        assert_eq!(empty, BuyVerdict::default());
        let single =
            cheapest_window(&[WeekdayStat { weekday: 3, median: 100, p05: 80, days: 1 }], &[]);
        assert_eq!(single.weekday, None, "one day of evidence is not a pattern");
    }
}
