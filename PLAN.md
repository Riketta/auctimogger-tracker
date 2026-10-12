# auctimogger-tracker — Implementation Plan

**Status:** planning locked; implementation not started (R0 pending green light).
**Role:** the standalone market memory for the AuctiMogger ecosystem. Polls
Blizzard-format auction dumps (and any conforming endpoint), keeps full history
in SQLite, computes the analytics the in-game sniper trusts (bit-identical
formulas), and publishes them as `feed.lua`, JSON API, and a decision-support
web UI.

**Companion project:** `AuctiMogger` (separate repo) consumes feed v2 per its
`PLAN.md §2.1` — that contract is frozen; this service must satisfy it and may
only extend additively. A future companion client app will poll this service's
JSON API to update the game machine's feed automatically.

---

## 0. Locked decisions

| # | Decision |
|---|---|
| D1 | Hexagonal architecture, compile-enforced: Cargo workspace, `core` crate depends on **no** infrastructure; adapters implement core-defined port traits; `server` composes (composition root). |
| D2 | Blizzard's wire format is absorbed by an **anti-corruption layer** in the source adapter; the core only ever sees normalized types. WoW Forever / any other endpoint reuses the same mapping via URL-profile config (payloads mimic the Blizzard shape). |
| D3 | **Every successful distinct payload becomes a permanent snapshot row** (metadata + histograms forever). Raw listing rows are a 7-day working set. HTTP 304 = health event, not a snapshot. |
| D4 | Realms/regions are **config-first**: TOML `[[targets]]` profiles support Blizzard connected-realm shorthand and raw-URL profiles (region + realm pairs, custom endpoints, optional per-target auth). Multi-target is first-class: every table carries `target_id`. |
| D5 | Histograms count each `auction_id` **once per day, at its last observed price** (listing appearances, not poll repeats — keeps server data comparable with the addon's dedup'd observations). |
| D6 | Metric formulas are **bit-identical** to the addon's `summarize()` (nearest-rank percentiles, floor-even median/MAD, floored VWAP). Golden-vector fixtures are asserted in Rust AND in the addon repo's Lua specs. |
| D7 | Sell-through = snapshot diffing; disappearance earlier than the `time_left` band's lower bound credits a sale candidate. `saleRate` is a conservative floor (cancels are indistinguishable). |
| D8 | Watchlist is managed via API (`POST /api/watchlist`) and persisted; the slim feed builds from it. Full feed stays available. |
| D9 | Item names/classes fetched from the Blizzard item API and cached permanently; traffic is a non-issue (personal tool). Search/completions run over the name catalog. |
| D10 | Web UI is a personal tool: default bind `127.0.0.1:8080`, optional token for mutating endpoints; no multi-user machinery. |
| D11 | Pets are first-class (species/level/quality/breed stored); analytics grouped by species in v1. |
| D12 | Feed delivery may later be automated by the companion client through this service's API — the API is a stable seam; keep lua + JSON dual-export from one code path. |
| D13 | The core is **synchronous** (std only): SQLite (`rusqlite`) is blocking, the scheduler runs on a plain thread, and async exists only at the HTTP edge (axum) in the `server` crate. Async never crosses a port. |

---

## 1. Architecture — hexagonal, compile-enforced

Cargo workspace; dependency direction is enforced by Cargo itself (the `core`
crate cannot even name `reqwest`/`rusqlite`/`axum`/`tokio` — they are not among
its dependencies), plus a CI boundary grep as belt-and-braces.

```
crates/
  core/      domain types + port TRAITS + application services (use cases)
  infra/     outbound adapters implementing core ports:
               sqlite (repositories), blizzard (auction source + item info),
               feed (renderer/publisher), clock
  server/    driving adapters + composition root:
               http (axum, DTOs), scheduler (poll loop driver), cli
main.rs      wiring only (like the addon's bootstrap.lua)
```

Dependency rule: `infra → core ← server`; `main` → all. Nothing else.

### 1.1 Domain types (core) — no serde, no sqlx, no tokio

```
TargetId, RealmProfile           item identity: ItemRef::{Item(item_id), Pet{species,level,quality,breed}}
NormalizedListing                one clean listing row (post-ACL)
SnapshotMeta                     id, target, kind, fetched_at, source_last_modified, sha256, counts
Span (D1|D7|D14|D30), MetricBlock { n, min, p05, p25, median, p75, p95, max, mad, vwap,
                                    qty_sum, listing_count, sale_rate? }
ItemDayHistogram                 {"unit_price": {listings, qty}} + unique-auction count
SaleCredit, SellThroughStats
FeedEntry, FeedSnapshot          the v2 contract shape (rendered by the feed adapter)
Opportunity, OpportunityScore    snipe-candidate + transparent score components
WatchlistEntry, SearchHit
```

### 1.2 Ports

Driven (implemented by `infra`):

| Port | Purpose | Adapter |
|---|---|---|
| `AuctionSourcePort` | fetch one payload for `(target, kind)` → `RawPayload { bytes, source_last_modified }` | `blizzard` (reqwest; conditional GET; backoff) |
| `ItemInfoPort` | item name / class / quality by id (cache-aside) | `blizzard` |
| `SnapshotRepository` | snapshot rows + raw payload archive (7-day listing working set) | `sqlite` |
| `PriceHistoryRepository` | write/read `item_day` histograms; per-target | `sqlite` |
| `SalesRepository` | sights + sale credits | `sqlite` |
| `MetricsCacheRepository` | memoized span payloads | `sqlite` |
| `ItemCatalogRepository` | id → name/class/quality catalog (search index) | `sqlite` |
| `WatchlistRepository` | CRUD | `sqlite` |
| `FeedPublisher` | publish rendered feed (disk + in-memory for HTTP) | `feed` |
| `FeedRenderer` | `FeedSnapshot` → lua bytes / json bytes | `feed` |
| `ClockPort` | time injection | `clock` |

Driving (implemented by `server`): HTTP handlers, the poll-loop scheduler, and
the CLI all call **application services** — never infrastructure.

### 1.3 Application services (core)

- `IngestCycleService` — one cycle per target: `AuctionSourcePort` → ACL parse
  (in source adapter) → `NormalizedListing` batch → snapshot row → day
  histograms (unique-auction fold) → metrics cache invalidation → sell-through
  diff → feed re-render/publish. Transactional per snapshot; a failed cycle
  writes nothing (D-decision: integrity first).
- `MetricsService` — span queries (cache-aside).
- `OpportunityService` — ranks snipe candidates (see §5).
- `MarketOverviewService` — movers/churn aggregates (see §5.3).
- `SearchService` — id + name search with completions (§5.1).
- `WatchlistService`, `FeedExportService`, `HealthService`.

### 1.4 ACL & DTOs

- **Source ACL** lives in the `blizzard` adapter: `item.id` / `unit_price` vs
  `buyout/quantity` semantics, modifier type 6 = pet species, `pet_*` fields,
  `time_left` strings, degenerate-row rules (cage rows without species, prices
  ≤ 0, bid-only handling) are all absorbed there. Core receives
  `NormalizedListing` only.
- **HTTP DTOs** live in `server` (`serde` structs, one per response shape),
  mapped from domain types; API schema changes never leak into core.

### 1.5 Boundary enforcement

- Cargo dependency graph (compile-time): the `core` crate has **no dependencies at all**.
- CI boundary test: core manifest must not contain a `[dependencies]` section; core sources must not `use` any infrastructure crate (`reqwest`, `rusqlite`, `axum`, `tokio`, `serde`, `toml`, `clap`).
- Ports are traits in `core`; `infra`/`server` `impl` them. `main.rs` is the only place that names concrete adapter types.

---

## 2. Configuration (TOML) — world-compatible targets

```toml
[server]
bind = "127.0.0.1:8080"
token_env = "AMT_API_TOKEN"          # mutating endpoints; unset = open (localhost)

[storage]
database = "./data/tracker.sqlite3"
raw_listing_days = 7                 # raw working set (snapshots metadata live forever)
archive_dir = "./data/raw"           # strict-parse archival (temp+rename)

[metrics]
spans = ["d1", "d7", "d14", "d30"]

[opportunities]                      # §5.2 weights, tunable
min_samples = 30
weights = { discount = 0.4, sale = 0.3, churn = 0.2, stability = 0.1 }

# --- targets: region+realm pairs or raw URLs; add as many as you like ---

[[targets]]
id = "us-4"
kind = "blizzard-connected-realm"
region = "us"
connected_realm_id = 4
auth = { type = "bearer", env = "AMT_TOKEN" }
poll = { commodities_secs = 3600, auctions_secs = 1800 }

[[targets]]
id = "wowforever-main"               # future: any conforming endpoint, world-wide
kind = "static-urls"
commodities_url = "https://example.org/api/commodities.json"
auctions_url = "https://example.org/api/auctions.json"
# auth = { type = "bearer", env = "AMT_WF_TOKEN" }   # optional
poll = { commodities_secs = 3600, auctions_secs = 3600 }
```

- `blizzard-connected-realm` builds standard URLs from region + realm id;
  `static-urls` accepts any conforming endpoint (WoW Forever's, mirrors, local
  files for tests). New endpoint flavors = a new mapping in the source ACL, not
  core changes.
- 304 Not Modified → health event only (no snapshot row; nothing to track).
- Token never in the TOML; env references only.

---

## 3. Data model (SQLite, WAL, per-target keys)

```sql
snapshots(id, target_id, kind, fetched_at, source_last_modified, sha256,
          entry_count, dropped_count, status)          -- ok | parse_failed | http_error
              -- permanent (D3); raw payload blob archived to disk, path on row

listing(snapshot_id, target_id, auction_id, item_ref_id, quantity,
        unit_price, bid, time_left, pet_species_id, pet_level,
        pet_quality_id, pet_breed_id, context, bonus_lists)
              -- 7-day working set (raw_listing_days); deleted in daily sweep

item_ref(id, kind, item_id, pet_species_id, pet_level, pet_quality_id, pet_breed_id)
              -- canonical identity per (target-agnostic) item/pet

item_day(target_id, item_ref_id, day, hist_json, listing_count, qty_sum)
              -- permanent; hist = per unit_price {listings, qty} of each auction's
              -- LAST price that day (unique auction_ids — D5)

sight(target_id, auction_id, item_ref_id, first_seen_snapshot, last_seen_snapshot, band)
sales(target_id, item_ref_id, day, credited, qty)

metrics_cache(target_id, item_ref_id, span, computed_at, payload_json)

item_catalog(item_id, name, item_class, quality, fetched_at)   -- name search (D9)
watchlist(target_id, item_ref_id, note, added_at)
```

Indexes: `listing(target_id, item_ref_id, snapshot_id)`, `listing(snapshot_id)`,
`item_day(target_id, item_ref_id, day)`, `item_catalog(name)`, `sight(auction_id)`.

---

## 4. Analytics

### 4.1 Metric parity (D6)

Span = merge of the last N `item_day` histograms → `MetricBlock` with exactly
the addon's formulas: nearest-rank percentiles (`ceil(f·n)` clamped to `[1,n]`),
even-count median/MAD = floor of the middle-pair average, `vwap = floor(Σp·q/Σq)`
with median fallback. **Golden vectors**: one shared fixture (raw listings →
expected block) asserted in Rust tests and added to the addon repo's Lua specs
in the same commit, so drift is impossible to merge accidentally.

Bid-only auction rows never enter buyout histograms (counted in snapshot
metadata for visibility). `unit_price ≤ 0` and species-less cage rows are
dropped + counted at the ACL.

### 4.2 Sell-through

Per snapshot diff: new `auction_id` → open sight; missing → close. Credit a
sale when `last_seen − first_seen < band_lower_bound` (SHORT: 0 → never
credits; MEDIUM ≥ 30 min; LONG ≥ 2 h; VERY_LONG ≥ 12 h). Aggregated into
`sales` per item-day → `saleRate = credited / listing_count` (a floor: cancels
indistinguishable). Feeds `OpportunityScore.reliability` and, via the feed
later, the addon's sanity-floor evidence.

---

## 5. Web UI (axum + askama + vendored chart JS; no CDN)

### 5.0 Dashboard `/`

Per-target snapshot freshness (age, entries, dropped, status), poll-loop health,
feed age/size per target, recent errors, watchlist size.

### 5.1 Search with completions `/items?q=…`

- `GET /api/search?q=<prefix|substring>` → id, name, class, quality; matches on
  the name catalog (D9) with prefix-first ranking; the nav search box debounces
  into it (completions dropdown), Enter → item page.
- Item page `/items/{target}/{id}`: span table (d1/d7/d14/d30 blocks),
  price chart (daily median/min/p05 from histograms), liquidity + churn,
  sale rate, volatility (MAD/median), pet attributes when applicable.

### 5.2 Opportunities tab `/opportunities` — "what should I actually snipe"

Ranked, filterable candidate list over `item_day` + `sales` + latest snapshot,
with **transparent** scoring (every component displayed, weights in TOML):

| Component | Meaning |
|---|---|
| `discount` | current floor vs d7 median (deeper = better) |
| `reliability` | 14-day sale rate — do things at these prices actually transact (the anti-bait signal) |
| `churn` | unique listings/day — how often fresh undercuts appear at all |
| `stability` | 1 − normalized MAD/median — how much the median itself can be trusted |
| `confidence` | sample count + days-of-history → a plain % (thin history = low %, hidden by default filters) |

`score = Σ weightᵢ · componentᵢ` (weights per `[opportunities]`), rendered as
component bars + a reliability %. Filters: min samples, min churn, price band,
pet/item, target. Rationale column explains WHY each row ranks (plain language
from the components). This is an `OpportunityService` use case in core — pure,
spec-tested ranking over port data.

### 5.3 Market overview tab `/overview`

Dynamics over time: top movers (median Δ% 24 h / 7 d, both directions), most
active markets (churn), biggest floor-vs-median gaps (live discount map),
filterable by item class and target; sparklines per row; click-through to item
pages. Aggregates computed from `item_day` (no raw-listing scans).

### 5.4 API seam (stable, for the future companion client)

```
GET  /feed.lua?target=…          GET /feed-slim.lua?target=…
GET  /api/feed.json?target=…     — same payload, JSON
GET  /api/items/{id}/metrics?spans=d1,d7
GET  /api/search?q=…             GET /api/opportunities?target=…
GET  /api/overview?target=…      GET /api/health
GET  /api/watchlist              POST /api/watchlist      DELETE /api/watchlist/{id}
POST /api/fetch?target=…         (token) out-of-band cycle trigger
```

---

## 6. Repo layout

```
auctimogger-tracker/
  Cargo.toml                     workspace
  crates/core/                   domain, port traits, services (no infra deps)
  crates/infra/                  sqlite, blizzard source/item-info, feed, clock
  crates/server/                 axum HTTP + DTOs, scheduler driver, CLI
  src/main.rs                    composition root (wiring only)
  migrations/*.sql
  templates/ static/             askama templates, vendored chart js/css
  fixtures/                      real trimmed dumps incl. the corrupted prefix
  tests/                         integration + golden vectors + boundary grep
  .github/workflows/ci.yml
  config.example.toml
  PLAN.md                        this file
```

---

## 7. CI / CD

- `cargo fmt --check`, `clippy -D warnings`, `cargo test` (fixtures: corrupted
  prefix must be rejected; golden metric vectors; boundary grep).
- Release job: static Linux binary (`x86_64-unknown-linux-gnu`, rusqlite
  bundled) + Windows build for local runs, attached as artifacts.
- Deploy doc: systemd unit (`ExecStart=… serve`), WAL + `db.path` notes, backup
  = sqlite backup API / file copy with checkpoint.

---

## 8. Milestones

| | Deliverable | Done when |
|---|---|---|
| R0 ✅ | Workspace scaffold, ports/traits, config TOML, boundary test | `cargo test` green (17 specs: fold semantics, ingest cycle over fakes, watchlist, config parsing, boundary); example config parses; `config-check` CLI works |
| R1 ✅ | Blizzard source adapter + ingest cycle + snapshots + item_day | live fetch verified against the real API (commodities 266 258 entries → 10 124 folds; auctions 12 948 → 6 199); 304 → NotModified; corrupted/truncated fixtures rejected (tests); SQLite store with atomic snapshot+listing records, fold round-trip, retention prune |
| R2 | Metric engine + golden vectors | Rust == Lua addon numbers on shared fixtures |
| R3 | Sell-through | synthetic sight sequences credit/deny per band rules |
| R4 | Feed export (lua+json, full+slim) + watchlist API | generated `feed.lua` passes `luac -p`, shape asserts, and the addon's ingestor specs |
| R5 | Web UI: dashboard, search/completions, item page, opportunities, overview | all tabs render live data |
| R6 | CI release + systemd docs | one-command deploy |

---

## 9. Open points / defaults chosen

- Item names: lazy fetch on first sight of an item + on search misses; permanent
  cache (D9).
- Opportunity score weights above are starting points; tunable in TOML, no code
  change needed.
- Single process (`serve` = poll loop + web). If fetch stability ever demands
  it, the scheduler driver can be split into its own process against the same
  SQLite (WAL) — noted, not built.

## Change log

- 2026-10-12: plan created; hexagonal crate layout, config-first targets,
  name-search + opportunities + overview UI, watchlist API locked.
- 2026-10-12: R0 implemented — three-crate workspace (`core` dependency-free,
  `infra` clock, `server` config+CLI), full port trait set, domain types,
  `DayFold` unique-auction fold semantics (D5) with tests, ingest cycle service
  over fakes, boundary tests, CI workflow, `config-check` CLI. D13 (sync core)
  locked during implementation.
- 2026-10-12: R1 implemented — Blizzard-format source adapter (ACL + conditional
  GET via `If-Modified-Since`, strict parse, degenerate-row rules; `ureq` chosen
  over `reqwest` because the core is synchronous per D13), SQLite repositories
  (atomic snapshot+listing+item_ref records, fold JSON round-trip, retention
  pruning of the raw working set while metadata/folds stay permanent), real
  `fetch` CLI, live-verified end-to-end against the real API. 27 specs green.
