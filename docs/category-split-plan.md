# Category Split Plan: item / commodity / pet everywhere

> **Status: implemented.** All phases landed as one pass (2026-10-27).
> Two deviations from the original step breakdown, both compile-green
> granularity calls: the `Category` step landed enum + derivation + tests
> only (row fields joined the steps that produce their data), and
> `items_with_data` gained a `category` parameter returning plain `ItemRef`s
> (feed export calls it once per category) instead of returning
> `(ItemRef, Category)` pairs.

Split the market data model by auction-house category — `item`, `commodity`,
`pet` — as the new default everywhere: storage, ranking, overview, metrics,
watchlist, feed, alerts, API and UI. Replaces the previous merged per-item
aggregates. Breaking changes are accepted (no current API consumers);
the API and the UI change in lockstep.

Locked decisions (from the design discussion):

1. **Three categories**, derived — not a raw payload mirror:

   | Payload kind | `ItemRef` shape | Category |
   |---|---|---|
   | Commodities | any | `commodity` |
   | Auctions | `Item { .. }` | `item` |
   | Auctions | `Pet { .. }` | `pet` |

   Total (every listing maps to exactly one category), data-independent, and
   one enum variant away from a fourth category.
2. **Split everywhere by default.** No merged rows anywhere; merged history
   ends at the migration. Consumers who want item totals across categories
   union per-category rows themselves (exact, fold merge stays possible).
3. **Server-side processed.** Per-`(target, category)` prepared scans
   (existing `ScanSnapshot` machinery, keyed ×3): the scheduler refreshes all
   categories in the background; requests read prepared artifacts only.
4. **Sale rates split too.** `reliability` becomes a per-category trait.
5. **Wipe + rebuild migration.** `item_day`, `sales`, `metrics_cache` are
   wiped and the day folds are rebuilt exactly from retained `listing` rows.
   `sight` is untouched (already payload-kind namespaced). Sale-rate history
   re-accumulates over its rolling 14-day window.
6. **API synced with UI.** `category` parameter everywhere; omitted → one
   response keyed by all three categories. Default UI tab: `item`.
7. **Global `min_samples` / `min_churn` defaults**, applied per category —
   no per-category config surface in v1.

---

## 1. Domain model

**`crates/core/src/domain.rs`**

- New enum, mirroring `PayloadKind`'s `as_str`/`parse` pattern:

  ```rust
  pub enum Category { Item, Commodity, Pet }
  ```

  Wire spelling: `"item"` / `"commodity"` / `"pet"` (ListingKind spelling in
  user-facing positions, per the agreed API vocabulary).
- Derivation helper (single source of truth for the table above):

  ```rust
  impl Category {
      pub fn derive(kind: ListingKind, item: &ItemRef) -> Category;
  }
  ```
- `ItemDayRow` gains `category: Category`.
- `FeedEntry` gains `category: Category`.

**`crates/core/src/services/opportunity.rs`**

- `Opportunity` and `MoverRow` gain `category: Category`; rows are always
  per-category from now on.

Naming discipline: the new storage column is `category`, never a fourth
`kind`. The three existing `kind` columns keep their meanings
(`item_ref.kind` = item-vs-pet, `snapshots.kind`/`sight.kind` = PayloadKind,
`NormalizedListing.kind` = ListingKind).

## 2. Storage

**`crates/infra/src/store.rs`**

Schema (bump `SCHEMA_VERSION`):

- `item_day`: PK `(target_id, item_ref_id, day, category)`, column
  `category TEXT NOT NULL`.
- `sales`: PK `(target_id, item_ref_id, day, category)`, same column.
- `metrics_cache`: PK `(target_id, item_ref_id, category, span)`, same column.
- `sight`, `listing`, `snapshots`: unchanged.

Migration (runs once on open at the new version):

1. Drop all rows from `item_day`, `sales`, `metrics_cache`.
2. Rebuild folds: stream
   `listing JOIN snapshots USING snapshot_id ORDER BY target_id, item_ref_id, day, snapshot_id`,
   derive `Category` per row from `snapshots.kind` + the item's shape, and
   fold into `DayFold`s (ordered replay preserves last-observation-wins).
   Write per-category folds back through the same serialization as live
   writes. Streaming group-by keeps memory bounded (one fold group at a
   time); batched inserts mirror `prune_raw_before`'s batching style.
3. `sales` / `metrics_cache` start empty and re-accumulate (sale rate is a
   rolling 14-day statistic; metric blocks recompute on demand).

The rebuild is idempotent and resumable: it reads only retained `listing`
rows, so a crash mid-replay just re-runs the pass on next startup.

Port implementations updated for the new signatures (§3): per-category
fold load/save, per-category `item_day_rows[_for_item]`, per-category
sale-rate reads, per-category cache keys. `observe`/`close_missing` keep
their signatures — they already receive `PayloadKind` and derive the
category per listing/item internally.

## 3. Ports

**`crates/core/src/ports.rs`**

- `PriceHistoryRepository`: `load_fold`, `load_folds`, `save_fold`,
  `items_with_data`, `item_day_rows`, `item_day_rows_for_item` all gain a
  `Category` parameter. `items_with_data` returns `(ItemRef, Category)`
  pairs (feed export needs the pairing; the catalog name warm-up dedupes
  by `ItemRef`).
- `SalesRepository`: `sale_rate`, `sale_rate_multi`, `sale_rate_each` gain
  `Category`. `observe` / `close_missing` unchanged.
- `MetricsCacheRepository`: `get`, `put`, `invalidate`, `invalidate_items`,
  `get_many`, `put_many` gain `Category`.
- Everything else (`SnapshotRepository`, catalog, watchlist, alert repos,
  feed ports): unchanged.

## 4. Core services

**`crates/core/src/services/cycle.rs`** — per item group, derive the
category once (`PayloadKind` of the cycle + the group's `ItemRef` shape),
pass it to `load_fold`/`save_fold`, and invalidate only that category's
metric-cache entries (sale-state invalidation likewise).

**`crates/core/src/services/opportunity.rs`**

- Scan cache: `HashMap<(TargetId, Category), (i64, Arc<ScanSnapshot>)>`.
- `refresh(target, category)`, `snapshot_age_secs(target, category)`,
  `opportunities(target, category, min_churn, limit)`,
  `overview(target, category, limit)`.
- `ItemScan` keyed by `(ItemRef, Category)` implicitly (each scan queries
  only its category's `item_day` rows) and stamped onto
  `Opportunity`/`MoverRow`.
- TTL / debounce / staleness-backstop logic per category: a failed refresh
  in one category keeps that category's previous artifact serving and never
  touches the others.
- `CHURN_ANCHOR` stays global in v1 (per-category anchors noted as the v2
  knob in config docs).

**`crates/core/src/services/metrics.rs`** — `block`, `blocks`,
`blocks_merged` gain `Category`. The species-grain merge (`blocks_merged`)
merges pet-variant folds within the `pet` category only; cross-category
merging no longer exists.

**`crates/core/src/services/watchlist_report.rs`** — `report` gains
`Category`; blocks, sale rate and trend rows all become per-category.

**`crates/core/src/services/feed_export.rs`**

- Selection: `items_with_data` pairs group by **(feed grain × category)**;
  a plain item trading in both AHs exports as two entries, one per
  category. Species-grain merge is unchanged inside `pet`.
- `FeedEntry.category` set per entry; entries sort by `(item, category)`.
- Freshness logic unchanged.

**`crates/core/src/services/alert.rs`** — no rule-schema change in v1.
Evaluation iterates the three categories per rule, observing each
category's block / sale rate; the first breach fires, and the event message
names the category (e.g. `item 7909 [commodity]: d1 median …`). The
cooldown anchor stays per rule, so one fire covers the pass.

**Unaffected (verified):** `chain.rs`, `health.rs`, `search.rs`,
`catalog.rs`, watchlist/alert rule CRUD.

## 5. Server

**`crates/server/src/scheduler.rs`** — `refresh_opportunities` loops the
three categories per target and logs per-category coverage;
`after_cycle_now` otherwise unchanged.

**`crates/server/src/web/api.rs`**

- `GET /api/opportunities?target=&category=&min_churn=&limit=` and
  `GET /api/overview?target=&category=`: `category` optional. Omitted →
  one response keyed by all three categories
  (`{"item": …, "commodity": …, "pet": …}`) so the UI renders its tabs from
  a single payload. Present → the bare single-category shape. Unknown
  values → 400.
- Item metrics and watchlist endpoints gain the same `category` parameter;
  omitted → per-category map.
- Every row carries `"category"`.
- Breaking: the previous merged shapes are gone.

**`crates/server/src/web/pages.rs`** — category tab switcher alongside the
existing target switcher (same query-param mechanics); `/opportunities`,
`/overview`, item pages and the watchlist page default to `item` and expose
all three tabs. `most active` / ranked tables show a per-row category chip
(when rendering the all-categories view).

**`crates/server/src/web/openapi.rs`** — params and response schemas for
every touched path; `Category` documented as an enum.

**`crates/infra/src/feed.rs`** — renderer emits the category field.

**Contract bump:** the README's "Feed contract (v2)" section becomes v3 —
entries gain `category`; dual-AH items appear once per category. The frozen
v2 section is replaced (no consumers today, per the locked decisions).

## 6. Config & docs

- `config.example.toml`: `[opportunities]` comments updated — `min_samples`
  gates **per category**; note that per-category churn anchors are the
  likely future knob (commodity traffic saturates the current 50/day anchor
  long before item-AH traffic does).
- README: market views describe three categories; feed contract bumped.

## 7. Test plan

- **domain**: `Category` derive table exhaustively (3 payload/kind ×
  item/pet shapes); `as_str`/`parse` round-trip.
- **store**: per-category fold save/load isolation (same item, two
  categories, no bleed); `sales` bumps per category; cache keyed per
  category; **migration test** — build a v-N database with known merged
  folds + listings from both payload kinds, open at v-N+1, assert folds
  rebuilt exactly per category, `sight` rows untouched, sale/cache tables
  empty.
- **fold/rebuild**: ordered replay of listings from both payload kinds
  reproduces last-observation-wins semantics per category.
- **opportunity service**: per-category scans are independent (refresh one,
  others still serve); all-three refresh coverage count; an item present in
  both AHs produces two independent rows with category-thinned evidence;
  debounce/TTL/backstop behavior unchanged per category.
- **metrics**: per-category blocks; species merge stays inside `pet`;
  no cross-category reads.
- **feed export**: dual-AH item → two entries with correct categories;
  pet species grain preserved; `generated_at` unchanged.
- **alert**: fires on the breaching category, message names it, cooldown
  respected across categories.
- **server web tests**: `category` param parsing (valid / invalid → 400 /
  omitted → all-three keyed); page tabs render and default to `item`;
  openapi spec contains the new parameter.

## 8. Suggested commit sequence

1. `Add Category to the domain model` (enum, derivation, row fields, tests)
2. `Split day folds, sales and metrics cache by category` (schema v+1,
   migration + rebuild, ports + store impls, store tests)
3. `Fold ingest per category` (cycle, cache invalidation)
4. `Rank and overview per category` (opportunity service, scheduler loop)
5. `Serve per-category metrics, watchlist and alert views`
6. `Tag feed entries with category and bump the feed contract`
7. `Add category to the API and UI` (api, pages, openapi, web tests)
8. `Document per-category behavior` (config example, README)

Steps 1–3 must land before anything reads per-category data; 4–7 can be
reviewed independently but bisect cleanly in this order.

## 9. Rollout notes

- First startup after deploy runs the rebuild (bounded by raw-listing
  retention); expect a one-time longer startup, batched like prune.
- **Evidence thinning is expected and correct**: an item in both AHs now
  has its 7-day samples split per category, so `min_samples` will drop some
  currently-rankable items. The merged number was spanning two markets.
- Sale rates (`reliability`) read thin for up to 14 days post-migration;
  confidence scaling degrades ranking gracefully rather than breaking it.
- Commodity churn will saturate `churn_score` at 1.0 much earlier than
  item/pet churn under the shared anchor — watch the ranked lists and tune
  weights first, per-category anchors only if needed.
