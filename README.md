# auctimogger-tracker

Standalone **market memory** for the AuctiMogger ecosystem: it polls
Blizzard-format auction dumps, stores the full history in SQLite, computes
per-item analytics over rolling windows, publishes the tracker feed consumed
by the in-game AuctiMogger addon (separate repo), and serves a
decision-support web UI — all in one process.

Where the addon answers *"buy this now?"*, the tracker answers *"what is
normal for this item, and how do I know this deal is real?"*.

## Contents

- [What it does](#what-it-does)
- [Quick start](#quick-start)
- [Configuration](#configuration)
- [CLI reference](#cli-reference)
- [Web UI & JSON API](#web-ui--json-api)
- [Feed contract (v3)](#feed-contract-v3)
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
  - [Feed the addon](#feed-the-addon)
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
  (`region` + `connected_realm_id`) or raw `static-urls` (any endpoint that
  serves the same JSON shape, e.g. mainline-tracking emulators). Conditional
  GET via `If-Modified-Since` (a 304 costs nothing); malformed or truncated
  rows are dropped **and counted** (per-reason breakdown persisted with the
  snapshot and shown on the snapshots page/API),
  never silently ingested; rate-limit answers back the poller off with their
  `Retry-After` hint.
- **Self-healing scheduler** — a panicking ingest cycle is caught, logged
  (panic hook feeds the file log and Sentry) and retried on the next
  interval; the web UI can never outlive ingestion silently. Failed store
  reads behind every page/API endpoint are logged server-side, not just
  returned as 500s.
- **SQLite storage (WAL)** — the raw listing working set is pruned after
  `storage.raw_listing_days` (default 7); day-folds, snapshot metadata,
  sell-through sightings and the item-name catalog are kept forever.
- **Span analytics** — per item and per span (`d1`/`d7`/`d14`/`d30`):
  `n, min, p05, p25, median, p75, p95, max, mad, vwap, qtySum, listingCount`
  plus a 14-day `saleRate` (`d1` is the current UTC day-to-date, so it is
  thin right after midnight). The engine is **bit-identical** to the in-game
  addon's (golden vectors asserted in both repos), and one auction is counted
  once per UTC day at its last observed price (hourly snapshots don't inflate
  samples).
- **Opportunity ranking** — transparent weighted score (discount, sale-rate
  reliability, churn, stability + confidence) over the stored history,
  ranked per market category (items, commodities, pets).
- **Tracker feed v2** — `feed.lua` (full), `feed-slim.lua` (watchlist only),
  `feed.json`; atomic writes; `generatedAt` taken from the source's
  `Last-Modified` so the addon can trust freshness.
- **Watchlist + alerts** — the watchlist page shows live market numbers per entry (batched reads); alert rules (`price-below`, `below-floor`, `sale-rate-above`) are evaluated after every ingest cycle with per-rule cooldowns, recorded as event history and delivered via log, Discord webhook or Telegram bot.
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
re-renders and publishes `feed.lua` + `feed.json` to
`./feeds/<target-id>/` (**relative to the working directory**; throttled by
`server.feed_publish_secs`, default 300), applies raw-listing retention once
per UTC day, and keeps the poll loop running. The DB path comes from the
config. Then open `http://127.0.0.1:8080`. The `export-feed` subcommand
remains for one-off/manual exports; the plain `fetch` subcommand runs
ingestion only — feeds, retention and ranking refreshes are `serve`'s job.

Prebuilt binaries (linux-gnu / windows-msvc archives with
`config.example.toml`) are attached to GitHub releases — see
[Releases](#github-ci--releases).

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
| `storage.raw_listing_days` | `7` | raw working-set retention; metadata/folds stay forever |
| `storage.backup_dir` | absent | directory for scheduled online backups; absent = off |
| `storage.backup_secs` | `0` | minimum seconds between scheduled backups (`0` = off) |
| `storage.backup_keep` | `7` | recent backups kept (oldest pruned after each backup) |
| `metrics.spans` | `["d1","d7","d14","d30"]` | computed windows |
| `opportunities.min_samples` | `30` | samples before an item is scored (per category) |
| `opportunities.weights` | discount .4 / sale .3 / churn .2 / stability .1 | score mix, tunable without code changes |
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
| `export-feed --config <p> [--target <id>] [--out feeds] [--slim]` | write feed.lua + feed.json without serving |

## Web UI & JSON API

Pages: `/` dashboard (health cards, recent ingest cycles, auto-reload) ·
`/snapshots` (recent snapshot history per target) · `/items?q=…` search +
alphabetical catalog browse · `/items/{target}/{item}` item detail ·
`/items/{target}/pet/{species}?level=&quality=&breed=` pet detail ·
`/opportunities` (filter/sort/CSV) · `/overview` (movers with real 30-day
sparklines) · `/watchlist` · `/help` (terminology).

Every page is complete server-rendered HTML; a small embedded script
(`app.js`) adds table sorting, search completions, the interactive price-band
chart, and — only when the mutating API is open — watchlist/"fetch now"
buttons. Hover or focus any dotted term (or read `/help`) for explanations of
median, p05/p95, MAD, VWAP, sale rate, score components, and so on. A target
switcher in the top bar works on every target-scoped page.

| Endpoint | Purpose |
|---|---|
| `GET /api` | machine-readable endpoint directory: every JSON API and feed route with its summary, plus links to the OpenAPI document, docs page and feeds — the entry point for agents/scripts |
| `GET /api/openapi.json` · `/api/docs` | OpenAPI 3.1 contract for the whole JSON API + feed surface, and a server-rendered HTML reference built from it (includes a Python quickstart) |
| `GET /api/health` | liveness probe (uptime monitoring) |
| `GET /api/search?q=…&limit=&offset=` | id/name search over items and pet species (kind-aware hits); page with `offset` until `hasMore` is false |
| `GET /api/opportunities` · `/api/overview?limit=` | ranked deals, market dynamics — per market `category` (`item\|commodity\|pet`); without `category`, one response keyed by all three (overview rows 1–50, default 15) |
| `GET /api/history?target=&days=&category=` | bulk analysis payload: every item/pet day aggregate in the window (listingCount, qtySum, min/p05/median/mad — integer copper, UTC day numbers, `category` per row); one call covers the whole market (rows ordered by day, then item), `days` clamps to 1–30 (default 7) |
| `GET /api/items/{target}/{item}/metrics?category=` | span blocks for one item in one market (`item` or `commodity`); without `category`, keyed by category |
| `GET /api/items/{target}/pet/{species}/metrics?level=&quality=&breed=` | the same, for one pet variant (all three identity parameters required) |
| `GET /api/snapshots?target=&kind=&limit=` | snapshot metadata rows, newest first (`kind=commodities\|auctions`) |
| `GET /api/feed.json` | machine-readable feed (future companion client) |
| `GET+POST /api/watchlist`, `DELETE /api/watchlist` | watchlist; POST and DELETE share one body: `{"target": "…", "type": "item", "itemID": 1}` (token-gated) |
| `GET /api/watchlist/metrics?target=&span=&sale_days=&trend_days=` | every watchlist entry joined to its metric blocks, sale rate and 30-day trend — one batched read (the live watchlist page's contract) |
| `GET /api/alerts?target=&events=` | a target's alert rules + newest fired events |
| `POST /api/alerts` · `DELETE /api/alerts` | add/remove rules (token-gated); condition is a tagged union: `{"type":"price-below","copper":10000}` \| `{"type":"below-floor","span":"d7","percentile":"p05"}` \| `{"type":"sale-rate-above","rate":0.3}` |
| `POST /api/alerts/evaluate?target=` | run one evaluation pass now (token-gated) |
| `POST /api/fetch` | trigger a cycle (token-gated; a JSON body is required — `{}` suffices — so cross-site forms cannot fire it in open mode); on success it also republishes the target's feed immediately. Unknown `?target=` values answer 400 |
| `GET /feed.lua` · `/feed-slim.lua` | feed downloads; `?target=<id>` optional (defaults to the first configured target) |

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
tables — no credentials needed, every id is fetched once.

Opportunities and overview are served from scans the scheduler prepares in
the background: once right after startup, after each successful ingest cycle
for the cycled target, and whenever a prepared scan outlives the scan TTL
(300 s). Requests therefore read no store in steady state; freshness is
bounded by the refresh cadence — typically well under one scan TTL plus a
scheduler tick — not by the TTL alone. Two inline fallbacks remain: a
completely cold cache (e.g. the very first request before the scheduler's
initial refresh lands) and, as a self-heal backstop, any scan that ages past
six scan TTLs (which would take a dead or persistently failing scheduler).

## Feed contract (v3)

```lua
AuctiMoggerTrackerFeed = {
  version = 3,
  generatedAt = 1760000000,                 -- newest source Last-Modified (unix secs)
  items = {
    { itemID = 194784, category = "commodity", metrics = { d1 = { n = 5, min = 100, p05 = 100, p25 = 200,
      median = 300, p75 = 400, p95 = 500, max = 500, mad = 100, vwap = 366,
      qtySum = 150, listingCount = 5, saleRate = 0.2500 } } },
    { kind = "pet", speciesID = 2546, level = 1, qualityID = 3, breedID = 11,
      category = "pet",
      metrics = { /* same block shape */ } },
  },
}
```

v3 added `category` to every entry (`item`, `commodity` or `pet`). The three
categories are separate evidence pools — items and commodities split the
auction houses, pets split off the item-AH rows by identity — so an item
trading in both auction houses exports once per category, each entry carrying
only its own market's metrics. Entries without metrics are omitted; the
in-game addon's ingestor parses this strictly. The executable source of truth:
`crates/infra/src/feed.rs`, `scripts/verify_feed.lua`, and the addon's
`spec/metrics_parity_spec.lua`.

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
  `api`, `render` and embedded `assets` modules) + DTOs, TOML config ACL,
  poll-loop scheduler thread (which also feeds the dashboard's recent-cycles
  ring).
- **`src/main.rs`** — composition root; the only place that names concrete
  adapter types.

Metric parity is sacred: `core::metrics::block_from_histograms` must stay
bit-identical to the addon's `Metrics.summarize` — golden vectors V1–V4 live
in both repos and fail either side on drift.

### Project structure

```
crates/core/     domain, ports, services (+ tests: boundary, services)
crates/infra/    sqlite, blizzard source/item/pet-info, feed, clock (+ tests)
crates/server/   axum web (pages/api/render/assets modules), scheduler, config ACL (+ tests)
crates/server/assets/  embedded UI stylesheet + enhancement script (include_str!)
src/main.rs      CLI + wiring
deploy/          systemd unit
scripts/         verify_feed.lua, install-release.sh
fixtures/        real trimmed dumps incl. the corrupted-prefix fixture
docs/            frontend-swap plan (future WASM UI decision doc)
```

### Quality gates (local CI stack)

Run all four before pushing — CI runs exactly these:

```sh
cargo +nightly fmt --all -- --check      # nightly-only rustfmt.toml options
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                   # 118 specs incl. golden vectors +
                                         # boundary checks; lua5.4 enables the
                                         # feed-verifier hooks (skipped if absent)
cargo deny check                         # licenses / advisories / bans / sources
```

The lint stack (`[workspace.lints]`, every crate opts in): `missing_docs`,
clippy `pedantic` + `cargo`; **denied**: `unwrap_used`, `expect_used`,
`panic`, `indexing_slicing`, `string_slice`. Test modules carry narrow
`#![allow(...)]` blocks with justification comments; production code
propagates errors and uses `.get()`/iterators, with scoped, justified allows
only where an index is proven in bounds.

### GitHub CI & releases

- **Linux lane**: fmt (nightly rustfmt) → clippy → tests (+ lua verifier) →
  cargo-deny. **Windows lane**: clippy → tests → build.
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

Every other key is documented in [Configuration](#configuration). Validate
before starting:

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

### Feed the addon

```sh
curl -sf -o feed.lua 'http://localhost:8080/feed.lua?target=wowforever-main'
# `?target` is optional and defaults to the first configured target.
# Watchlist-only variant:
curl -sf -o feed-slim.lua 'http://localhost:8080/feed-slim.lua?target=wowforever-main'

cp feed.lua <wow>/Interface/AddOns/AuctiMogger/AuctiMoggerTracker/feed.lua
```

`<wow>` is the client root (the folder containing `Interface/`).

In-game, `/reload` makes the AuctiMoggerTracker sub-addon ingest the file
at load; `/auctimogger ingest` re-reads it anytime without a reload. A 404
means nothing has been published for that target yet — wait for a green
cycle. The files also live in the `amtracker-feeds` volume:
`docker compose cp tracker:/work/feeds/wowforever-main/feed.lua .`

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
  the low single-digit GB).
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
- The addon consumes `feed.lua` / `feed-slim.lua`; `feed.json` is the seam
  for the future companion client.
