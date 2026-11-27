# auctimogger-tracker

Standalone **market memory** for the AuctiMogger ecosystem: it polls
Blizzard-format auction dumps, stores the full history in SQLite, computes
per-item analytics over rolling windows, publishes the market database
consumed by the in-game AuctiMogger Database addon (in `addon/`), and serves a
decision-support web UI — all in one process.

Where the addon answers *"buy this now?"*, the tracker answers *"what is
normal for this item, and how do I know this deal is real?"*.

## Contents

- [What it does](#what-it-does)
- [Quick start](#quick-start)
- [Configuration](#configuration)
- [CLI reference](#cli-reference)
- [Web UI & JSON API](#web-ui--json-api)
- [Crafting (recipe file)](#crafting-recipe-file)
- [Feed contract (v4)](#feed-contract-v4)
- [For developers](#for-developers)
  - [Architecture](#architecture)
  - [Project structure](#project-structure)
  - [Quality gates (local CI stack)](#quality-gates-local-ci-stack)
  - [GitHub CI & releases](#github-ci--releases)
- [Docker (demo compose)](#docker-demo-compose)
  - [Set up](#set-up)
  - [Configure](#configure)
  - [Run](#run)
  - [Use it](#use-it)
  - [AuctiMogger Database](#auctimogger-database)
  - [Update, stop, reset](#update-stop-reset)
  - [Without building](#without-building)
- [Deployment (Linux server)](#deployment-linux-server)
  - [Server layout](#server-layout)
  - [First-time install](#first-time-install)
  - [One-command updates](#one-command-updates)
  - [Storage, WAL and backups](#storage-wal-and-backups)
  - [Operations](#operations)

## What it does

- **Multi-target polling** — any number of targets: Blizzard connected realms
  (`region` + `connected_realm_id`), the undocumented Blizzard "Forever" API
  (`blizzard-forever`: one faction auction house of one ruleset), or raw
  `static-urls` (any endpoint that serves the same JSON shape, e.g.
  mainline-tracking emulators). Conditional GET via `If-Modified-Since` (a
  304 costs nothing); malformed or truncated rows are dropped **and counted**
  (per-reason breakdown persisted with the snapshot and shown on the
  snapshots page/API), never silently ingested; rate-limit answers back the
  poller off with their `Retry-After` hint.
- **Self-healing scheduler** — a panicking ingest cycle is caught, logged
  (panic hook feeds the file log and Sentry) and retried on the next
  interval; the web UI can never outlive ingestion silently. Failed store
  reads behind every page/API endpoint are logged server-side, not just
  returned as 500s.
- **SQLite storage (WAL)** — the raw listing working set is pruned after
  `storage.raw_listing_days` (default 7) and closed sell-through sightings
  after `storage.sight_retention_days` (default 0 = kept forever); day-folds,
  the intraday hour fold, snapshot metadata and the item-name catalog are
  kept on their own horizons (the hour fold: `storage.item_hour_retention_days`,
  default 45, `0` = forever).
- **Span analytics** — per item and per span (`d1`/`d7`/`d14`/`d30`):
  `n, min, p05, p25, median, p75, p95, max, mad, vwap, qtySum, listingCount`
  plus a 14-day `saleRate`, its 180-day slow-window twin `saleRate180d`, and
  the ledger-recency pair `daysSinceLastSale` / `closuresSinceLastSale`
  (the "stopped selling" churn signal; TSM's `NumExpires` analog)
  (`d1` is the current UTC day-to-date, so it is
  thin right after midnight). The engine is **bit-identical** to the in-game
  addon's (golden vectors asserted in both repos), and one auction is counted
  once per UTC day at its last observed price (hourly snapshots don't inflate
  samples).
- **Live deals** — `/opportunities` ranks only what the newest snapshot still
  lists: each item's cheapest listed price against its quantity-weighted
  7-day value (the bulk price for stackables), scored by a weighted geometric
  mean of discount, sale-rate reliability, churn and stability — a liquid but
  undiscounted market cannot outrank a real deal. Every row also carries the
  evidence a buyer reads beside the score: the **sold / expired** ledger
  counts behind the sale rate, **days of supply** at the current sale pace,
  where the price sits against the **previous 30 days' daily lows**, and the
  **vendor floor** (a `≤ vendor` badge on a zero-risk buy) — informational,
  never score inputs. Listings seen today that were bought or cancelled land
  in a separate "seen today, already gone" table (and `GET
  /api/missed-deals`) instead of passing as buyable. Ranked per market
  category (items, commodities, pets).
- **Efficient crafts** — `/crafts` prices a hand-maintained recipe file
  (`recipes.example.toml`) on each target: the reagents at their cheapest
  sourcing (7-day market value, or their own craft cost when the book crafts
  them and that is cheaper — recursively) against the crafted item's value,
  less the auction-house cut, scaled by how reliably the item sells — ranked
  by expected profit per make, with every recipe that misses a price, lacks
  sale evidence, sells too slowly or loses money listed separately with the
  reason. The recipe file reloads when it changes on disk.
- **Market database feed (v4)** — per target, `feed.lua` (the packed carrier
  the AuctiMogger Database addon loads as a market file, about 250 bytes per
  item) and `feed.json` (the same records, decoded); atomic writes;
  `generatedAt` taken from the source's `Last-Modified` so the addon can trust
  freshness. Every record carries the span blocks, the live listing and when
the item was last listed, the item's value and trend, the sold/expired
  evidence, the 30-day position, the vendor price and — where they apply —
  the craft and the deal rank.
- **Watchlist + alerts** — the watchlist page shows live market numbers per entry (batched reads); alert rules (`price-below` and `below-floor` on the cached blocks; `floor-below`, `floor-below-value` and `new-listing-below` on the newest snapshot's live listings — the sniper operands, each fired listing counted once; `sale-rate-above` and `days-since-last-sale-above` on the ledger) are evaluated first after every ingest cycle, with per-rule cooldowns, recorded as event history and delivered via log, Discord webhook or Telegram bot. Each market category only observes while its own feed is younger than `[alerts].max_data_age_secs` (48 h by default, `0` disables) — commodity rules gate on the commodities feed, item/pet rules on the auctions feed — so a silently dead feed can never fire an alert. Alert evaluation runs with ingest cycles: while a source is unreachable, alerts pause with the market itself (data cannot change without successful ingests).
- **Web UI + JSON API** — dashboard, snapshot browser, name-aware search with
  completions, item detail with interactive 30-day price-band chart (p05–p95,
  median, VWAP), opportunities, market overview, live watchlist, cross-target
  alerts. Mutating endpoints are token-gated.

## Quick start

```sh
cargo build --release
cp config.example.toml config.toml      # adjust targets + paths

export BLIZZARD_CLIENT_ID=<from develop.battle.net>    # target auth (name = your choice)
export BLIZZARD_CLIENT_SECRET=<from develop.battle.net>
export AMT_API_TOKEN=<random string>                # gates mutating API endpoints

./target/release/auctimogger-tracker config-check config.toml
./target/release/auctimogger-tracker fetch  --config config.toml   # one cycle, all targets (analysis only — no feeds published)
./target/release/auctimogger-tracker serve  --config config.toml   # poll loop + UI on 127.0.0.1:8080
```

`serve` is self-contained: after each successful ingest cycle the scheduler
refreshes the prepared market scans, then re-renders and publishes
`feed.lua` + `feed.json` to
`./feeds/<target-id>/` (**relative to the working directory**; throttled by
`server.feed_publish_secs`, default 300), applies raw-listing retention once
per UTC day, and keeps the poll loop running. The DB path comes from the
config. Then open `http://127.0.0.1:8080`. The `export-feed` subcommand
remains for one-off/manual exports; the plain `fetch` subcommand runs
ingestion only — feeds, retention and ranking refreshes are `serve`'s job.

Prebuilt binaries (linux-gnu / windows-msvc archives with
`config.example.toml` and `recipes.example.toml`) are attached to GitHub
releases — see [Releases](#github-ci--releases).

## Configuration

Copy `config.example.toml` and adjust. Tokens are read from the environment
only — never put secrets in this file.

| Key | Default | Meaning |
|---|---|---|
| `debug` | `false` | louder stdout defaults for the tracker's own crates; `RUST_LOG` overrides entirely |
| `server.bind` | `127.0.0.1:8080` | web UI/API listen address (a socket address, or `localhost:PORT` which is pinned to `127.0.0.1`; other hostnames are refused at startup) |
| `server.token_env` | `AMT_API_TOKEN` | env var holding the mutation token; unset **or empty** = open on a loopback bind (localhost dev; one loud startup warning). On any other bind this **refuses to start `serve`** |
| `server.allow_open` | `false` | explicit opt-out: set `true` to let `serve` start with an open mutating API on a non-loopback bind (strongly discouraged) |
| `server.feed_publish_secs` | `300` | minimum seconds between feed re-publications per target |
| `storage.database` | `./data/tracker.sqlite3` | SQLite database (WAL) |
| `storage.archive_dir` | `./data/raw` | raw payload archive |
| `storage.feed_dir` | `./feeds` | where `serve` publishes `feed.lua` / `feed.json` (CWD-relative unless absolute) |
| `storage.raw_listing_days` | `7` | raw working-set retention; metadata/folds stay forever |
| `storage.item_hour_retention_days` | `45` | intraday hour-fold retention (the item page's when-to-buy hour view); `0` = forever. Keep ≥ the largest intraday preset you use (30-day preset needs ≥ 30) |
| `storage.sight_retention_days` | `0` | days closed sell-through sightings are kept (`0` = forever; aggregates live in the sales ledger regardless) |
| `storage.backup_dir` | absent | directory for scheduled online backups; absent = off |
| `storage.backup_secs` | `0` | minimum seconds between scheduled backups (`0` = off) |
| `storage.backup_keep` | `7` | recent backups kept (oldest pruned after each backup) |
| `storage.vacuum` | `incremental` | freed-space reclamation after prunes: `incremental` (returns pages once ~20% of the file is free; legacy stores convert with a one-time full VACUUM), `full` (complete VACUUM daily), `off` (never shrink) |
| `metrics.spans` | `["d1","d7","d14","d30"]` | computed windows |
| `opportunities.min_samples` | `30` | auction-days of 7-day evidence behind an item's value (per category): below it an item neither ranks nor lists as a gone-today deal, and `floor-below-value`/`new-listing-below` alerts do not fire |
| `opportunities.weights` | discount .4 / sale .3 / churn .2 / stability .1 | exponents of the weighted-geometric-mean score (only ratios matter; a component at zero scores zero) |
| `crafts.recipes` | absent | path of the manual recipe file (TOML, see `recipes.example.toml` and [Crafting](#crafting-recipe-file)); re-read when the file changes on disk. Without it `/crafts` has no recipes |
| `crafts.min_sale_rate` | `0.2` | crafted items below this 14-day sale rate are not ranked as efficient crafts |
| `crafts.ah_cut_percent` | `5.0` | auction-house cut taken off the output value |
| `[logging]` | absent | optional file logging: `dir` (created at boot, failure aborts), `rotation` = `daily`\|`never`, independent `level` (EnvFilter directives) |
| `[sentry]` | absent | optional Sentry/GlitchTip reporting: `dsn` (empty = off), `environment`, `traces_sample_rate` (0.0..=1.0; absent = error-only) |
| `[[targets]]` | — | see below |

Targets (one block each, any number):

```toml
[[targets]]
id = "us-4"
kind = "blizzard-connected-realm"     # or "static-urls"
region = "us"
connected_realm_id = 4
# OAuth client credentials: env var NAMES, not secrets. Bearer tokens are
# fetched from the Battle.net OAuth endpoint and refreshed automatically
# ahead of expiry (a rejected token triggers one refresh + retry).
# `serve` refuses to start while a configured target's credential env vars
# are unset or empty, so a forgotten secret cannot masquerade as a working
# deployment that never ingests.
auth = { type = "client-credentials", client_id_env = "BLIZZARD_CLIENT_ID", client_secret_env = "BLIZZARD_CLIENT_SECRET" }
poll = { commodities_secs = 3600, auctions_secs = 1800 }
```

Blizzard targets may omit `auth` entirely — the default is exactly the block
above. A static token is still available for endpoints that want one:
`auth = { type = "bearer", env = "AMT_TOKEN" }`.

`static-urls` targets set `commodities_url` / `auctions_url` instead and have
optional `auth` — that is the seam a non-Blizzard server will plug into.

### Forever targets (undocumented Blizzard API, realmless)

`blizzard-forever` tracks **one faction auction house of one ruleset** —
rulesets replace realms in the Forever API, and each ruleset exposes
faction-separate Alliance/Horde houses plus the neutral Blackwater ones.
Faction AHs are separate markets (an Alliance bidder cannot see Horde
auctions), so track one target per house and never merge across houses or
rulesets — the API currently returns identical data for every ruleset, and
counting it several times would corrupt the aggregates.

```toml
[[targets]]
id = "forever-eu-pvp-alliance"
kind = "blizzard-forever"
region = "eu"         # us | eu | kr | tw (CN has no Forever API)
ruleset_id = 1        # the realm replacement
house_id = 111        # one house within the ruleset (111 = the EU PvP Alliance house)
poll = { commodities_secs = 3600, auctions_secs = 1800 }
```

Auth defaults to the same Blizzard client credentials as connected-realm
targets. `flavor` defaults to `"forever"`: item names resolve through
`static-forever-{region}` with Wowhead first in the chain covering the ids
the Forever item API does not have (~40% of Classic-era items). Caveats of
an undocumented, still-moving API: some houses carry no `auctions` field at
all (a healthy empty market, not an error), bid-only rows are dropped and
counted like retail, and the endpoint shapes can change — a parse drift
fails the cycle loudly rather than ingesting nothing silently.

Discover rulesets and house ids empirically (needs the credentials in your
current config):

```sh
auctimogger-tracker --config config.toml forever-rulesets
```

Unknown keys anywhere in the file abort startup naming the offending path
(`unknown config keys: sentry.environment2`): a typo like `enviroment` must
fail loudly rather than silently disable the setting it was meant to set.

## CLI reference

| Command | Purpose |
|---|---|
| `config-check <path>` | parse + validate a config, print a target summary |
| `backup --config <p> --out <path>` | write one consistent online backup of the database, then exit |
| `fetch --config <p> [--target <id>]` | run one ingestion cycle now |
| `serve --config <p>` | poll scheduler + web UI/API (the service mode) |
| `metrics --config <p> --target <id> --item <id> [--span d1]` | print one analytics block (debug aid) |
| `export-feed --config <p> [--target <id>] [--out feeds]` | write feed.lua + feed.json (the v4 market database) without serving |
| `forever-rulesets --config <p>` | list Forever rulesets + auction houses via the config's credentials (discovery aid for `blizzard-forever` targets) |
| `backfill-item-hours --config <p> [--rebuild-since <utc day>] | rebuild the when-to-buy hour fold from the raw window (skips hours already folded; `serve` runs it at boot). `--rebuild-since` first clears the fold from that UTC day on — clamped to the raw window — and rebuilds exactly that range: the repair for an interrupted pass or hours known to be incomplete. Run it with `serve` stopped |
| `addon-sync --url <base> --addons-dir <Interface/AddOns> [--market <target>=<slot>]… [--no-addon] [--no-markets] [--dry-run] [--timeout-secs N]` | install or update the AuctiMogger Database addon from a **running** tracker (no config, no store): the addon's own files into `AuctiMoggerDatabase/`, then every market the tracker reports into slots 1–6 — `--market` pins a market to a slot, the rest fill the free slots in the tracker's order. A file is replaced only when it differs; a market replaces its slot only after its carrier passed verification (feed version, market, closing line — nothing lands that the addon would refuse at load); a slot that already holds a file is never overwritten by its placeholder. Prints a table (slot, target, bytes, age) and reminds to `/reload`; exit 1 on any failure |

## Web UI & JSON API

Pages: `/` dashboard (health cards, recent ingest cycles, auto-reload) ·
`/snapshots` (recent snapshot history per target) · `/items?q=…` search +
alphabetical catalog browse · `/items/{target}/{item}` item detail ·
`/items/{target}/pet/{species}?level=&quality=&breed=` pet detail ·
`/browse` snapshot viewer (one stored snapshot's listings, laid out like the
in-game auction house: name/id/class/quality filters, sortable price/qty/time
columns, page arrows; unpinned = newest snapshot, pinned = frozen history) ·
`/opportunities` (live deals with filter/sort/CSV, then the "seen today,
already gone" list) · `/crafts` (the manual recipe book priced on the
target: efficient crafts best-first, the rest with the reason) ·
`/overview` (movers with real 30-day
sparklines) · `/watchlist` · `/priorities` (the watchlist ranked best sell
first: 50% liquidity + 30% momentum + 20% recency, components in tooltips,
thin-data ranks marked `~`) · `/compare` (one watched identity across all
targets, best-paying market highlighted — answers "sell here or there?") ·
`/help` (terminology).

Every page is complete server-rendered HTML; a small embedded script
(`app.js`) adds table sorting, search completions, the interactive price-band
chart, and — only when the mutating API is open — watchlist/"fetch now"
buttons. Hover or focus any dotted term (or read `/help`) for explanations of
median, p05/p95, MAD, VWAP, sale rate, score components, and so on. A target
switcher in the top bar works on every target-scoped page.

| Endpoint | Purpose |
|---|---|
| `GET /api` | machine-readable endpoint directory: every JSON API and feed route with its summary, plus links to the OpenAPI document, docs page, feeds and the addon manifest — the entry point for agents/scripts |
| `GET /api/openapi.json` · `/api/docs` | OpenAPI 3.1 contract for the whole JSON API + feed surface, and a server-rendered HTML reference built from it (includes a Python quickstart) |
| `GET /api/health` | liveness probe (uptime monitoring) |
| `GET /api/search?q=…&limit=&offset=` | id/name search over items and pet species (kind-aware hits); page with `offset` until `hasMore` is false |
| `GET /api/opportunities` · `/api/overview?limit=` | live ranked deals, market dynamics — per market `category` (`item\|commodity\|pet`); without `category`, one response keyed by all three (overview rows 1–50, default 15). With `category`, deal rows carry the live price fields (`floorNow`, `floorQty`, `listedNow`, `value7`, `soldMedian14`, `valueSource`) and the evidence next to them (`soldCount`, `expiredCount`, `supplyDays`, `lowRank30d`, `vendorPrice` — see “Evidence next to the deal rows”), and the response names the `snapshot` they come from. `discount = 1 - floorNow / reference`, the reference being `soldMedian14` once ≥ 3 priced units sold in 14 days, else `value7` (see “Sold prices and the reference value”). `&sort=last_sale` orders stalest-since-last-sale first (default `score`) |
| `GET /api/missed-deals?target=&category=&min_discount=&limit=` | listings seen today that the newest snapshot no longer lists, at least `min_discount` percent (default 25) under the item's quantity-weighted 7-day value, deepest first, with the sold/expired counts behind each row's rate — the same two response shapes as opportunities |
| `GET /api/crafts?target=` | the manual recipe book priced on the target: every recipe with its priced reagents (each at the lower of its reference value — 14-day sold median once ≥ 3 priced units sold, else 7-day listing value — and its recursive `craftCost` when the book crafts it), the output's reference value and 14-day sale rate, and per-make cost, live cost, profit, expected profit and ROI in copper — efficient crafts first (`ranked: true`), the rest carrying the first `issue` that kept them out; `recipeCount` and `reloadError` describe the live book (the file reloads when it changes) |
| `GET /api/history?target=&days=&category=` | bulk analysis payload: every item/pet day aggregate in the window (listingCount, qtySum, min/p05/median/mad — integer copper, UTC day numbers, `category` per row); one call covers the whole market (rows ordered by day, then item), `days` clamps to 1–30 (default 7). The market is read once per category per 300 s; every `days` window is served from that read |
| `GET /api/items/{target}/{item}/metrics?category=` | span blocks for one item in one market (`item` or `commodity`); without `category`, keyed by category |
| `GET /api/items/{target}/pet/{species}/metrics?level=&quality=&breed=` | the same, for one pet variant (all three identity parameters required) |
| `GET /api/browse?target=&kind=&snapshot=&q=&item=&class=&quality=&sort=&dir=&page=` | one page (50 rows) of a stored snapshot's listings joined to their catalog identity; without `snapshot` the newest for the pair serves; `sort=price\|qty\|time` (default price asc, `dir=desc` flips) |
| `GET /api/snapshots?target=&kind=&limit=` | snapshot metadata rows, newest first (`kind=commodities\|auctions`) |
| `GET /api/feed.json` | the published `feed.json` — the v4 market database decoded (contract: [`addon/SPEC.md`](addon/SPEC.md), section 5): the same file the scheduler writes after each successful cycle (throttled by `server.feed_publish_secs`), never rebuilt per request; 404 until the first publish |
| `GET+POST /api/watchlist`, `DELETE /api/watchlist` | watchlist; POST and DELETE share one body: `{"target": "…", "type": "item", "itemID": 1}` (token-gated) |
| `GET /api/watchlist/metrics?target=&span=&sale_days=&trend_days=` | every watchlist entry joined to its metric blocks, sale rate and 30-day trend — one batched read (the live watchlist page's contract) |
| `GET /api/priorities?target=&category=` | the watchlist ranked best sell first: score (50% liquidity + 30% momentum + 20% recency) with components, current-day median, week block and trend rows (the `/priorities` page's contract; default category `item`) |
| `GET /api/watchlist/compare?category=` | the union of all configured targets' watchlists as a matrix: one row per identity, one cell per target (`watched`/`now`/`weekMedian`), `best` = index of the market with the highest current-day median per row (null when nothing traded it anywhere today; rows in a fixed identity order — the `/compare` page's contract; default category `item`) |
| `GET /api/alerts?target=&events=` | a target's alert rules + newest fired events |
| `POST /api/alerts` · `DELETE /api/alerts` | add/remove rules (token-gated); condition is a tagged union: `{"type":"price-below","copper":10000}` \| `{"type":"below-floor","span":"d7","percentile":"p05"}` \| `{"type":"floor-below","copper":10000}` \| `{"type":"floor-below-value","fraction":0.5}` \| `{"type":"new-listing-below","fraction":0.5}` \| `{"type":"sale-rate-above","rate":0.3}` \| `{"type":"days-since-last-sale-above","days":14}` |
| `POST /api/alerts/evaluate?target=` | run one evaluation pass now (token-gated) |
| `POST /api/fetch` | trigger a cycle (token-gated; a JSON body is required — `{}` suffices — so cross-site forms cannot fire it in open mode); on success it also republishes the target's feed immediately. Unknown `?target=` values answer 400 |
| `GET /feed.lua` | the published `feed.lua` — the v4 carrier the AuctiMogger Database addon loads (the same data as `/api/feed.json`, packed); `?target=<id>` optional (defaults to the first configured target); 404 until the first publish |
| `GET /api/addon/manifest` | the AuctiMogger Database addon embedded in this binary, file by file — `{ "addon", "version", "feedVersion", "files": [{ "path", "sha256", "size" }] }`. This is what `addon-sync` installs from, so the addon a tracker hands out always reads the feed format that tracker serves |
| `GET /api/addon/file/<path>` | one file of that bundle, verbatim: a manifest path, slashes included (`Core/pack.lua`); anything else is 404 — the lookup reads the embedded table, never the filesystem |

The API is designed to be driven by agents and scripts: `GET /api` is a
discoverable index, `/api/openapi.json` serves the OpenAPI 3.1 contract
(hand-authored next to the handlers and pinned to the router by a drift test
that asserts paths and methods match in both directions), and all reads are
unauthenticated with no rate limiting. Every target-scoped read requires
`?target=<id>` (unknown ids answer 400; only the feed downloads default to
the first configured target). Market views are per category — `item`,
`commodity` and `pet` are separate evidence pools, and every row names its
`category`. A typical analysis pass is two calls —
`GET /api/opportunities?target=<id>&category=commodity&limit=1000` for the
ranking, then `GET /api/history?target=<id>&days=7` for the whole market's
day-level price history — followed by per-item `/metrics` for the
interesting ids; the Python quickstart on `/api/docs` shows exactly that.

Mutation buttons are rendered only in open mode (no usable API token): with a
token configured, the browser never holds the secret, and watchlist/fetch
actions stay API-only. Item and pet names resolve through Wowhead's
credential-free endpoints first (`[catalog]`, optional Blizzard fallback)
and are cached permanently in the per-flavor `item_catalog`/`pet_catalog`
tables — no credentials needed, every id is fetched once. A background
warmer thread resolves every id ever folded (paced by
`[catalog].warm_batch` / `warm_pace_ms`), so unnamed ids drain to zero
without waiting on ingest cycles. It lists all-time once at boot and then
only what folded since its previous pass (re-checking known ids about once a
day), so its steady state costs no whole-market scans.

Opportunities and overview are served from scans the scheduler prepares in
the background: once right after startup, after each successful ingest cycle
for the cycled target, and whenever a prepared scan outlives the scan TTL
(300 s). Requests therefore read no store in steady state; freshness is
bounded by the refresh cadence — typically well under one scan TTL plus a
scheduler tick — not by the TTL alone. Two inline fallbacks remain: a
completely cold cache (e.g. the very first request before the scheduler's
initial refresh lands) and, as a self-heal backstop, any scan that ages past
six scan TTLs (which would take a dead or persistently failing scheduler).

### Sold prices and the reference value

A deal's discount is measured against the item's **reference value**: the
**14-day sold median** — the quantity-weighted median of what buyers actually
paid — once at least **3 priced units** sold in the window, otherwise the
quantity-weighted 7-day listing median (`value7`). One bargain is not a
market, so thinner sold evidence never overrides the asks. The ranking, the
gone-today list, the crafts view and the value-relative alert conditions all
read the same reference; rows and tables mark sold-based values (`valueSource
= "sold"`, a `*` in the UI).

Sold prices come from the sale ledger (schema v11): every credited sale stores
the unit price its listing was last seen asking, and a commodity partial fill
the listing's posted price. A reopened listing reverses its phantom sale,
priced units included. Nothing recorded before the upgrade can be
back-priced — those sales still count toward quantities and sale rates but
carry no price, so the reference only switches to "sold" as fresh evidence
accumulates after the deploy.

### Evidence next to the deal rows

Four informational fields ride on every live deal row (API and the
`/opportunities` table); none of them moves the score:

- **`soldCount` / `expiredCount`** — the raw ledger behind the 14-day sale
  rate. A credited close is *sold*; a credit-eligible close that outlived its
  band's lower bound is *expired unsold* (relisted). Their ratio is exactly
  `reliability`, so a 50 % rate over 2 sales and one over 200 read
  differently. No migration: both halves were always in the ledger
  (`expired = eligible − credited`). The item page shows the same counts
  under its metric blocks, and `sale-rate-above` alerts carry them.
- **`supplyDays`** — days the listed stock takes to clear at the fortnight's
  sale pace (quantity listed ÷ units sold per day). High = oversupplied,
  prices sag, a flip sits; `∞` = nothing sold in 14 days.
- **`lowRank30d`** — the share of the previous 30 days whose cheapest listing
  sat *above* the live price (100 % = a 30-day low). Read from the permanent
  hour fold (`MIN(floor)` per item-day), refreshed hourly per market — a
  narrow aggregate, not 30 days of fold JSON — and silent under five days of
  history.
- **`vendorPrice`** — the vendor sell price from the item API, when known.
  Vendors pay that much, so the market cannot durably sit below it; a floor at
  or under it is flagged `≤ vendor` (`at_vendor_floor`): it cannot fall
  further, and a vendor buys it back for at least that much.

## Crafting (recipe file)

The tracker knows prices, not recipes: which items craft into which is the
one thing you supply by hand. A recipe file is TOML, one `[[recipe]]` per
craft, with reagents as `item id = quantity`:

```toml
[[recipe]]
item = 12345          # the crafted item
makes = 1             # units per make (default 1)
[recipe.reagents]
11111 = 1
22222 = 2
```

Point `[crafts].recipes` at the file (relative paths resolve against the
working directory — `/work` in the container, so mount it beside
`config.toml`); `config-check` validates it like the rest of the config.
The file is live: the tracker re-reads it whenever it changes on disk and the
next `/crafts` view applies the new book — a broken save keeps the previous
book and the page (and `reloadError` in the API) says why. Unknown keys,
non-numeric reagent keys, non-positive ids or quantities, a repeated reagent
and a chain that crafts back into its own output are refused naming the
recipe. Several recipes may craft one item (alternative reagent sets) — each
is its own row. `recipes.example.toml` in the repository is a commented
template.

`/crafts?target=` then prices every recipe on that target:

- **value 7d** — the output's quantity-weighted 7-day median unit price,
  the same value the deal ranking discounts against (an item trading in
  both houses is priced where it has more evidence; never the output's own
  craft cost);
- **cost 7d / cost now** — the reagents at their cheapest sourcing, and at
  the newest snapshot's cheapest listings when every reagent is listed.
  Sourcing is the lower of the reagent's 7-day value and its **craft cost**
  when the book itself crafts it: one unit of a crafted reagent is costed
  from its own recipe's reagents (sourced the same way, recursively; whole
  copper, rounded up), so a chain of crafts prices end to end. The reagent
tooltip and the API (`craftCost`) name both routes.
- **profit** — value × makes − house cut (`crafts.ah_cut_percent`) − cost;
- **expected profit** — profit × the output's 14-day sale rate, the ranking
  key; **ROI** — profit ÷ cost.

A recipe is an *efficient craft* when every reagent and the output have a
7-day price, the output's sale rate is at least `crafts.min_sale_rate`, and
the craft is profitable. Everything else is listed below with the first
reason it missed: a reagent or the output without a price, no sale evidence
yet, a slow seller, or reagents that cost as much as the output nets.

## Feed contract (v4)

Per target the tracker publishes two files under `storage.feed_dir`:
`<target>/feed.lua`, the packed carrier the **AuctiMogger Database** addon
loads as a market file, and `<target>/feed.json`, the same records decoded.
[`addon/SPEC.md`](addon/SPEC.md) is the single source of truth for the
contract — its section 5 defines the files, the record layout and the
absence rules — and the tracker implements it in
`crates/core/src/services/database_export.rs` (what a record carries),
`crates/infra/src/record.rs` and `pack.rs` (how it is packed) and
`crates/infra/src/feed.rs` (the carrier and its JSON twin).

The same files reach the game through the **AuctiMogger Database** addon
([`addon/`](addon/SPEC.md)), and the tracker serves the addon itself: its
files are embedded in the binary and listed by `GET /api/addon/manifest`, and
`auctimogger-tracker addon-sync` installs them, then every market's carrier
into the addon's slots, under the game's `Interface/AddOns` (see the CLI
reference). A sync verifies each carrier against its market before it
replaces a slot file, so a truncated download never displaces a working one.

```lua
-- AuctiMogger Database market file - GENERATED by auctimogger-tracker 0.1.0. Do not hand-edit.
local _, A = ...
local m = A.BeginMarket({
    version = 4,
    trackerVersion = "0.1.0",
    target = "forever-eu-pvp-alliance",
    flavor = "forever",
    region = "eu",
    rulesetId = 1,
    houseId = 111,
    generatedAt = 1795607640,      -- newest source Last-Modified (unix secs)
    auctionsAt = 1795607700,
    commoditiesAt = 1795607700,
})
m.items[25] = "…"                  -- one packed record per identity
m.commodities[191460] = "…"
m.pets[2546] = "…"                 -- pets are keyed by species
A.EndMarket()
```

A record holds the four span blocks (`d1`/`d7`/`d14`/`d30`: `n, min, p05,
p25, median, p75, p95, max, mad, vwap, qtySum, listingCount`, `saleRate`,
`saleRate180d`, `daysSinceLastSale`, `closuresSinceLastSale`, `qtySold`) and
the groups behind them: the `live` listing (floor, quantity at the floor,
listings, total quantity, today's sighting that is already gone, and the
days since the item was last seen listed), the `value` (7-day listing value, 14-day sold median, today's trend in basis
points), the sold/expired `evidence` with days of supply, the 30-day
`position` (lowest ask, share of days above the live floor, a one-byte-per-day
sparkline), the vendor price, the best craft and the deal rank. Every
optional field is **absent when the tracker has no evidence — never zero** —
in the carrier and in the JSON twin alike.

The three categories are separate evidence pools — items and commodities
split the auction houses, pets split off the item-AH rows by identity — so an
item trading in both auction houses exports one record per category. Pets
pool every variant onto their species; a species with several variants
exports spans only. Records without a span are omitted.

`feed.json` mirrors what a decoder reads (rates as `bp / 10000`, saturated
values as saturated) and is the oracle of the cross-language test that pins
the addon's Lua decoder to the Rust reference decoder; the golden fixtures
under `fixtures/database_v4.*` are rendered by
`crates/infra/tests/database_golden.rs`.

## For developers

### Architecture

Hexagonal (ports & adapters), compile-enforced:

- **`crates/core`** — domain types, port traits, application services.
  **Zero dependencies by contract**: a boundary test fails the build if a
  `[dependencies]` section or an infra `use` appears. The core is synchronous
  (D13); async exists only at the HTTP edge.
- **`crates/infra`** — driven adapters: `ureq` Blizzard source (conditional
  GET, strict parse), `rusqlite` (bundled) repositories, feed codec +
  atomic publisher, `WowheadItemInfo` + `BlizzardItemInfo` behind the
  `ChainedItemInfo` name-resolution chain, `SystemClock`.
- **`crates/server`** — driving adapters: axum web (split into `pages`,
  `api`, `render` and embedded `assets` modules, plus the embedded addon
  `addon_bundle`) + DTOs, TOML config ACL, poll-loop scheduler thread (which
  also feeds the dashboard's recent-cycles ring).
- **`src/main.rs`** — composition root; the only place that names concrete
  adapter types.

Metric parity is sacred: `core::metrics::block_from_histograms` must stay
bit-identical to the addon's `Metrics.summarize` — golden vectors V1–V4 live
in both repos and fail either side on drift.

### Project structure

```
crates/core/     domain, ports, services (+ tests: boundary, services)
crates/infra/    sqlite, blizzard source/item/pet-info, feed codec + record packing, clock (+ tests)
crates/server/   axum web (pages/api/render/assets modules, embedded addon bundle), scheduler, config ACL (+ tests)
crates/server/assets/  embedded UI stylesheet + enhancement script (include_str!)
src/main.rs      CLI + wiring
addon/           AuctiMogger Database addon — its SPEC.md is the feed contract's source of truth; the deployable folder is embedded in the server binary
deploy/          systemd unit
scripts/         install-release.sh
fixtures/        real trimmed dumps incl. the corrupted-prefix fixture; the v4 golden trio (`database_v4.*`)
docs/            frontend-swap plan (future WASM UI decision doc)
```

### Quality gates (local CI stack)

Run all four before pushing — CI runs exactly these:

```sh
cargo +nightly fmt --all -- --check      # nightly-only rustfmt.toml options
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                   # 448 specs incl. golden vectors + boundary checks
cargo deny check                         # licenses / advisories / bans / sources
```

The lint stack (`[workspace.lints]`, every crate opts in): `missing_docs`,
clippy `pedantic` + `cargo`; **denied**: `unwrap_used`, `expect_used`,
`panic`, `indexing_slicing`, `string_slice`. Test modules carry narrow
`#![allow(...)]` blocks with justification comments; production code
propagates errors and uses `.get()`/iterators, with scoped, justified allows
only where an index is proven in bounds.

### GitHub CI & releases

- **Linux lane**: fmt (nightly rustfmt) → clippy → tests → cargo-deny.
  **Windows lane**: clippy → tests → build.
- **Release** (tag `v*`, or manual dispatch): `--release --locked` builds for
  `x86_64-unknown-linux-gnu` and `x86_64-pc-windows-msvc`, packaged with
  `config.example.toml`; tags additionally publish a GitHub release. The
  linux-gnu binary is built on `ubuntu-latest` (currently glibc 2.39) — older
  glibc hosts should use the Docker image or build from source.
- **Docker**: multi-stage image build, published to GHCR from main (`:main`,
  `:sha-<8>`) and from `v*` tags (`:x.y.z`, `:x.y`, `:latest`); the job does
  not run on pull requests. The image bundles its own glibc, so it
  sidesteps the host-glibc caveat of the release tarball.

## Docker (demo compose)

The fastest path to a running tracker. You need Docker Engine with the
compose plugin; Blizzard API credentials — a client id + secret from
<https://develop.battle.net> — are required only for live
`blizzard-connected-realm` targets — `static-urls` targets (e.g. a
mainline-tracking emulator) need no secret at all.

### Set up

```sh
git clone https://github.com/Riketta/auctimogger-tracker.git
cd auctimogger-tracker

cat > .env <<'EOF'
# Blizzard OAuth client credentials, used by blizzard-connected-realm
# targets only. Create an application at https://develop.battle.net.
BLIZZARD_CLIENT_ID=
BLIZZARD_CLIENT_SECRET=
# Protects mutating endpoints (watchlist writes, /api/fetch). Any random
# string. The compose default is the well-known dev token
# 'dev-insecure-token-change-me' (one loud startup warning tells you it is
# in effect — replace me). An EMPTY value makes `serve` refuse to start on
# the container's non-loopback bind (set [server].allow_open = true in
# config.docker.toml to override deliberately).
AMT_API_TOKEN=
EOF
```

Fill in the values; `.env` is gitignored and never baked into the image.

### Configure

`docker-compose.yml` mounts `./config.docker.toml` read-only as the
container's `/work/config.toml` — edit that file, not a copy of it. Its
defaults are container-shaped: bind `0.0.0.0:8080`, storage under `/data`,
stdout logging only. Replace the demo target block with the market you
follow:

```toml
# Mainline-tracking emulator (static JSON dumps, no auth):
[[targets]]
id = "wowforever-main"
kind = "static-urls"
commodities_url = "https://example.org/api/commodities.json"
auctions_url = "https://example.org/api/auctions.json"
poll = { commodities_secs = 3600, auctions_secs = 3600 }

# Live Blizzard connected realm (reads BLIZZARD_CLIENT_ID and
# BLIZZARD_CLIENT_SECRET from the environment; tokens are fetched and
# refreshed automatically):
# [[targets]]
# id = "us-4"
# kind = "blizzard-connected-realm"
# region = "us"
# connected_realm_id = 4
# poll = { commodities_secs = 3600, auctions_secs = 1800 }
```

To use the crafting view, mount a recipe file into `/work` as well and set
`[crafts].recipes = "/work/recipes.toml"` (see
[Crafting](#crafting-recipe-file)). Every other key is documented in
[Configuration](#configuration). Validate before starting:

```sh
docker compose run --rm tracker config-check /work/config.toml
```

### Run

```sh
docker compose up -d --build
docker compose logs -f tracker                # cycles, publications, warnings
curl -s http://localhost:8080/api/health
```

The first ingest cycle runs immediately at boot; pages stay empty until it
succeeds. After every successful cycle the scheduler re-publishes
`feed.lua`/`feed.json` (throttled by `server.feed_publish_secs`, default
300) and applies raw-listing retention once per UTC day.

### Use it

Open `http://localhost:8080`: `/` dashboard · `/items?q=…` name search ·
`/items/{target}/{itemID}` per-item detail · `/opportunities` ranked deals ·
`/overview` market dynamics · `/snapshots` raw history. The same data over
JSON:

```sh
curl -s 'http://localhost:8080/api/search?q=194784'
curl -s 'http://localhost:8080/api/items/wowforever-main/194784/metrics'
curl -s 'http://localhost:8080/api/opportunities'

# Mutating endpoints require the AMT_API_TOKEN bearer:
curl -s -X POST http://localhost:8080/api/watchlist \
  -H "Authorization: Bearer $AMT_API_TOKEN" -H 'Content-Type: application/json' \
  -d '{"target":"wowforever-main","type":"item","itemID":194784,"note":"flip"}'
curl -s -X DELETE http://localhost:8080/api/watchlist \
  -H "Authorization: Bearer $AMT_API_TOKEN" -H 'Content-Type: application/json' \
  -d '{"target":"wowforever-main","type":"item","itemID":194784}'
curl -s -X POST 'http://localhost:8080/api/fetch?target=wowforever-main' \
  -H "Authorization: Bearer $AMT_API_TOKEN" -H "Content-Type: application/json" -d '{}'
```

Pets use `"type":"pet"` with `speciesID`/`level`/`qualityID`/`breedID`.

### AuctiMogger Database

The in-game addon that shows this market data on item tooltips. Its guide —
installing, the `/amdb` commands, the options panel, what each tooltip line
means and the API other addons can read — is
[`addon/README.md`](addon/README.md); [`addon/SPEC.md`](addon/SPEC.md) is the
technical contract.

```sh
# On the machine with the game client, with any tracker binary of this version:
auctimogger-tracker addon-sync --url http://localhost:8080 \
    --addons-dir "<wow>/Interface/AddOns"
# writes the addon's files into AddOns/AuctiMoggerDatabase/, then one market
# per slot (every target the tracker reports; --market <target>=<slot> pins one).

# Or by hand:
curl -sf -o feed.lua 'http://localhost:8080/feed.lua?target=wowforever-main'
# `?target` is optional and defaults to the first configured target.
# `feed.json` (same route family: /api/feed.json) is the same market decoded.

cp feed.lua <wow>/Interface/AddOns/AuctiMoggerDatabase/Market1.lua
```

`<wow>` is the client root (the folder containing `Interface/`). The addon
lives in [`addon/`](addon/SPEC.md) of this repository and is served by the
tracker itself (`GET /api/addon/manifest` lists it, file by file), so the
addon always matches the feed format; a release also attaches
`AuctiMoggerDatabase.zip` — the addon folder with empty market slots — for a
manual install. It has six market slots (`Market1.lua` … `Market6.lua`), and
a carrier is installed by copying it over a slot.

In-game, `/reload` loads the market file; `GET /feed.lua` and
`/api/feed.json` answer 404 until the first successful cycle has published —
wait for a green cycle. After an update they also answer 404 while the file in
the volume is still the previous tracker's, until its first cycle republishes
it (`POST /api/fetch?target=` runs one now). The files also live in the
`amtracker-feeds` volume:
`docker compose cp tracker:/work/feeds/wowforever-main/feed.lua .`

In game: enable the addon, pick this character's primary market with
`/amdb market <target>`, then hover any item — a panel beside the game's
tooltip shows its market data, and **Shift** switches to the detail block.
`/amdb help` lists the rest.

### Update, stop, reset

```sh
git pull && docker compose up -d --build   # new revision, same volumes
docker compose down                        # stop; data + feeds volumes survive
docker compose down -v                     # ...or wipe the database and feeds
```

### Without building

CI publishes the image to GHCR on pushes to main (`:main`, `:sha-<8>`) and
on `v*` tags (`:x.y.z`, `:x.y`, `:latest`). To skip the local build, replace
the `tracker` service's `build: .` / `image:` lines with:

```yaml
    image: ghcr.io/riketta/auctimogger-tracker:main
```

The image bundles its own glibc, so it also sidesteps the host-glibc caveat
of the release tarball. CLI one-offs against the same volumes keep working:

```sh
docker compose run --rm tracker config-check /work/config.toml
docker compose run --rm tracker export-feed --config /work/config.toml --out /work/feeds
```

The demo stack is production-shaped — non-root user, named volumes, stdout
logging for `docker logs` — so keeping it long-term is fine; the systemd
layout below is the bare-metal alternative.

## Deployment (Linux server)

### Server layout

| Path | Purpose |
|---|---|
| `/opt/auctimogger-tracker/auctimogger-tracker` | release binary |
| `/etc/auctimogger-tracker/config.toml` | configuration (from `config.example.toml`, absolute paths) |
| `/etc/auctimogger-tracker/env` | secrets only — `chmod 640 root:tracker` |
| `/var/lib/auctimogger-tracker/` | **WorkingDirectory**: SQLite DB, raw archive, `feeds/` |

`config.toml` should point at absolute paths:

```toml
[storage]
database = "/var/lib/auctimogger-tracker/data/tracker.sqlite3"
archive_dir = "/var/lib/auctimogger-tracker/data/raw"
```

`/etc/auctimogger-tracker/env` (sourced by systemd, `KEY=value` lines):

```sh
BLIZZARD_CLIENT_ID=<from develop.battle.net>
BLIZZARD_CLIENT_SECRET=<from develop.battle.net>
AMT_API_TOKEN=<random string for the tracker's own mutation API>
```

### First-time install

```sh
sudo useradd --system --home /var/lib/auctimogger-tracker --shell /usr/sbin/nologin tracker
sudo mkdir -p /opt/auctimogger-tracker /etc/auctimogger-tracker \
              /var/lib/auctimogger-tracker/data/raw
sudo chown -R tracker:tracker /var/lib/auctimogger-tracker /opt/auctimogger-tracker
sudo install -m 0640 -o root -g tracker config.toml /etc/auctimogger-tracker/
sudo install -m 0640 -o root -g tracker env /etc/auctimogger-tracker/
sudo install -m 0755 auctimogger-tracker /opt/auctimogger-tracker/
sudo install -m 0644 deploy/auctimogger-tracker.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now auctimogger-tracker
```

### One-command updates

```sh
sudo AUCTIMOGGER_REPO=owner/repo ./scripts/install-release.sh          # latest
sudo AUCTIMOGGER_REPO=owner/repo ./scripts/install-release.sh v0.2.0   # pinned
```

Downloads the `x86_64-unknown-linux-gnu` archive from GitHub releases, swaps
the binary under `/opt`, and restarts the unit.

### Storage, WAL and backups

- SQLite runs in **WAL mode**: expect `tracker.sqlite3`, `-wal`, `-shm`. The
  sidecar files are part of the database — never delete or move them alone.
- Keep the DB on **local disk**; NFS/CIFS mounts can corrupt WAL databases.
- Raw listings prune after `raw_listing_days` (the scheduler applies it once
  per UTC day); folds/metadata/catalog grow slowly (multi-month DBs stay in
  the low single-digit GB). The intraday hour fold (when-to-buy hour view)
  holds ~one row per item per observed hour and prunes after
  `item_hour_retention_days` (default 45; `0` = forever) — a busy realm's
  fold is roughly 15–20× smaller than the raw rows it summarizes, but on
  very item-dense realms consider capping it near 45–60 to bound growth.
  Closed sell-through sightings are the fastest-growing table on busy
  realms — set `sight_retention_days` (e.g. 30–60) to cap them; their
  sale-rate aggregates live in the sales ledger regardless.
- Feeds are published to `storage.feed_dir` (default `./feeds`) — a bare-metal
  launcher's working directory decides where that lands unless the path is
  made absolute.
- **Upgrading from a pre-v2 database**: tracker schema v2 scopes sell-through
  sightings per payload kind (commodity and item auction ids are separate
  namespaces). Old files carry no kind on their sighting rows and cannot be
  migrated faithfully — the tracker refuses to start with a clear message;
  delete the database file and let it rebuild (folds regenerate within a
  day).
- **Never `cp` a live WAL database by itself** — use the backup API, e.g.
  the built-in subcommand (`VACUUM INTO` — a consistent copy, safe while
  `serve` runs; parent directories are created, an existing file is
  refused):

```sh
auctimogger-tracker backup --config /etc/auctimogger-tracker/config.toml \
    --out "/backups/tracker-$(date +%F).sqlite3"
```

`sqlite3 … ".backup …"` works identically. With the service stopped, copy
all three files together, or checkpoint first
(`PRAGMA wal_checkpoint(TRUNCATE);`) and copy the single `.sqlite3`. Cron
example (nightly, keep 14):

```cron
0 4 * * * sqlite3 /var/lib/auctimogger-tracker/data/tracker.sqlite3 ".backup '/backups/tracker-$(date +\%F).sqlite3'" && find /backups -name 'tracker-*.sqlite3' -mtime +14 -delete
```

**Scheduled backups** replace that cron: with `[storage].backup_dir` set
and `backup_secs > 0`, `serve` writes a consistent online backup every
`backup_secs` — `amtracker-backup-<UTC timestamp>.sqlite3` files in that
directory, pruned after each backup down to the newest `backup_keep`. A
failed or panicking backup is logged (warn) and never disturbs ingestion;
apply the same disk rules as the database itself (local disk, never
NFS/CIFS). Backups run on the scheduler thread, so very large databases
may briefly delay a poll cycle — size `backup_secs` accordingly.

### Operations

- `systemctl status auctimogger-tracker` · `journalctl -u auctimogger-tracker`.
- All runtime events flow through `tracing`: every ingest cycle logs its
  outcome (info), source failures warn, CLI commands emit structured
  startup/summary lines. `RUST_LOG` retunes stdout without touching the
  config; the optional `[logging]` section adds an independent file layer.
- With `[sentry]` configured, error-grade events become Issues and info/warn
  ship as breadcrumbs (Sentry or self-hosted GlitchTip — any Sentry-protocol
  DSN). No DSN = stdout/journald only.
- Restarts on failure (`Restart=on-failure`, 5 s backoff); probe
  `GET /api/health`.
- `Ctrl+C` / `SIGTERM` shut down gracefully: in-flight requests finish, the
  file-log worker is shut down cleanly, and queued Sentry events are flushed
  before exit.
- Reads are open on the loopback bind; mutations (`POST /api/fetch`,
  watchlist changes) require the `AMT_API_TOKEN` value. `serve` refuses to
  start with an unset/empty token on a non-loopback bind unless
  `[server].allow_open = true`. For LAN access, bind `0.0.0.0` only behind a
  reverse proxy with TLS + auth.
- The AuctiMogger Database addon consumes `feed.lua`; `feed.json` is the same
  market decoded, for reading and for scripts.
