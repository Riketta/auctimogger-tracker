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
- [Feed contract (v2)](#feed-contract-v2)
- [For developers](#for-developers)
  - [Architecture](#architecture)
  - [Project structure](#project-structure)
  - [Quality gates (local CI stack)](#quality-gates-local-ci-stack)
  - [GitHub CI & releases](#github-ci--releases)
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
  rows are dropped **and counted**, never silently ingested.
- **SQLite storage (WAL)** — the raw listing working set is pruned after
  `storage.raw_listing_days` (default 7); day-folds, snapshot metadata,
  sell-through sightings and the item-name catalog are kept forever.
- **Span analytics** — per item and per span (`d1`/`d7`/`d14`/`d30`):
  `n, min, p05, p25, median, p75, p95, max, mad, vwap, qtySum, listingCount`
  plus a 14-day `saleRate`. The engine is **bit-identical** to the in-game
  addon's (golden vectors asserted in both repos), and one auction is counted
  once per UTC day at its last observed price (hourly snapshots don't inflate
  samples).
- **Opportunity ranking** — transparent weighted score (discount, sale-rate
  reliability, churn, stability + confidence) over the stored history.
- **Tracker feed v2** — `feed.lua` (full), `feed-slim.lua` (watchlist only),
  `feed.json`; atomic writes; `generatedAt` taken from the source's
  `Last-Modified` so the addon can trust freshness.
- **Web UI + JSON API** — dashboard, snapshot browser, name-aware search with
  completions, item detail with 30-day sparkline, opportunities, market
  overview. Mutating endpoints are token-gated.

## Quick start

```sh
cargo build --release
cp config.example.toml config.toml      # adjust targets + paths

export AMT_TOKEN=<blizzard bearer token>            # target auth (name = your choice)
export AMT_API_TOKEN=<random string>                # gates mutating API endpoints

./target/release/auctimogger-tracker config-check config.toml
./target/release/auctimogger-tracker fetch  --config config.toml   # one cycle, all targets
./target/release/auctimogger-tracker serve  --config config.toml   # poll loop + UI on 127.0.0.1:8080
```

`serve` writes feeds to `./feeds/<target-id>/` **relative to the working
directory**; the DB path comes from the config. Then open
`http://127.0.0.1:8080`.

Prebuilt binaries (linux-gnu / windows-msvc archives with
`config.example.toml`) are attached to GitHub releases — see
[Releases](#github-ci--releases).

## Configuration

Copy `config.example.toml` and adjust. Tokens are read from the environment
only — never put secrets in this file.

| Key | Default | Meaning |
|---|---|---|
| `debug` | `false` | louder stdout defaults for the tracker's own crates; `RUST_LOG` overrides entirely |
| `server.bind` | `127.0.0.1:8080` | web UI/API listen address |
| `server.token_env` | `AMT_API_TOKEN` | env var holding the mutation token; unset = open (localhost use) |
| `storage.database` | `./data/tracker.sqlite3` | SQLite database (WAL) |
| `storage.archive_dir` | `./data/raw` | raw payload archive |
| `storage.raw_listing_days` | `7` | raw working-set retention; metadata/folds stay forever |
| `metrics.spans` | `["d1","d7","d14","d30"]` | computed windows |
| `opportunities.min_samples` | `30` | samples before an item is scored |
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
auth = { type = "bearer", env = "AMT_TOKEN" }   # env var NAME, not the secret
poll = { commodities_secs = 3600, auctions_secs = 1800 }
```

`static-urls` targets set `commodities_url` / `auctions_url` instead and have
optional `auth` — that is the seam a non-Blizzard server will plug into.

## CLI reference

| Command | Purpose |
|---|---|
| `config-check <path>` | parse + validate a config, print a target summary |
| `fetch --config <p> [--target <id>]` | run one ingestion cycle now |
| `serve --config <p>` | poll scheduler + web UI/API (the service mode) |
| `metrics --config <p> --target <id> --item <id> [--span d1]` | print one analytics block (debug aid) |
| `export-feed --config <p> [--target <id>] [--out feeds] [--slim]` | write feed.lua + feed.json without serving |

## Web UI & JSON API

Pages: `/` dashboard · `/snapshots` · `/items?q=…` search ·
`/items/{target}/{item}` detail · `/opportunities` · `/overview`.

| Endpoint | Purpose |
|---|---|
| `GET /api/health` | liveness probe (uptime monitoring) |
| `GET /api/search?q=…` | id/name search with completions |
| `GET /api/opportunities` · `/api/overview` | ranked deals, market dynamics |
| `GET /api/items/{target}/{item}/metrics` | span blocks for one item |
| `GET /api/feed.json` | machine-readable feed (future companion client) |
| `GET+POST /api/watchlist`, `DELETE /api/watchlist/{id}` | watchlist (POST/DELETE token-gated) |
| `POST /api/fetch` | trigger a cycle (token-gated) |
| `GET /feed.lua` · `/feed-slim.lua` | feed downloads served from the feeds dir |

## Feed contract (v2)

```lua
AuctiMoggerTrackerFeed = {
  version = 2,
  generatedAt = 1760000000,                 -- newest source Last-Modified (unix secs)
  items = {
    { itemID = 194784, metrics = { d1 = { n = 5, min = 100, p05 = 100, p25 = 200,
      median = 300, p75 = 400, p95 = 500, max = 500, mad = 100, vwap = 366,
      qtySum = 150, listingCount = 5, saleRate = 0.2500 } } },
    { kind = "pet", speciesID = 2546, level = 1, qualityID = 3, breedID = 11,
      metrics = { /* same block shape */ } },
  },
}
```

Entries without metrics are omitted; the in-game addon's ingestor parses this
strictly. The executable source of truth: `crates/infra/src/feed.rs`,
`scripts/verify_feed.lua`, and the addon's `spec/metrics_parity_spec.lua`.

## For developers

### Architecture

Hexagonal (ports & adapters), compile-enforced:

- **`crates/core`** — domain types, port traits, application services.
  **Zero dependencies by contract**: a boundary test fails the build if a
  `[dependencies]` section or an infra `use` appears. The core is synchronous
  (D13); async exists only at the HTTP edge.
- **`crates/infra`** — driven adapters: `ureq` Blizzard source (conditional
  GET, strict parse), `rusqlite` (bundled) repositories, feed codec +
  atomic publisher, `BlizzardItemInfo`, `SystemClock`.
- **`crates/server`** — driving adapters: axum web + DTOs, TOML config ACL,
  poll-loop scheduler thread.
- **`src/main.rs`** — composition root; the only place that names concrete
  adapter types.

Metric parity is sacred: `core::metrics::block_from_histograms` must stay
bit-identical to the addon's `Metrics.summarize` — golden vectors V1–V4 live
in both repos and fail either side on drift.

### Project structure

```
crates/core/     domain, ports, services (+ tests: boundary, services)
crates/infra/    sqlite, blizzard source/item-info, feed, clock (+ tests)
crates/server/   axum web, scheduler, config ACL (+ tests)
src/main.rs      CLI + wiring
deploy/          systemd unit
scripts/         verify_feed.lua, install-release.sh
fixtures/        real trimmed dumps incl. the corrupted-prefix fixture
```

### Quality gates (local CI stack)

Run all four before pushing — CI runs exactly these:

```sh
cargo +nightly fmt --all -- --check      # nightly-only rustfmt.toml options
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                   # 46 specs incl. golden vectors +
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
  glibc hosts should build from source.

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
AMT_TOKEN=<blizzard bearer token>
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
- Raw listings prune after `raw_listing_days`; folds/metadata/catalog grow
  slowly (multi-month DBs stay in the low single-digit GB).
- **Never `cp` a live WAL database by itself** — use the backup API:

```sh
sqlite3 /var/lib/auctimogger-tracker/data/tracker.sqlite3 \
    ".backup '/backups/tracker-$(date +%F).sqlite3'"
```

With the service stopped, copy all three files together, or checkpoint first
(`PRAGMA wal_checkpoint(TRUNCATE);`) and copy the single `.sqlite3`. Cron
example (nightly, keep 14):

```cron
0 4 * * * sqlite3 /var/lib/auctimogger-tracker/data/tracker.sqlite3 ".backup '/backups/tracker-$(date +\%F).sqlite3'" && find /backups -name 'tracker-*.sqlite3' -mtime +14 -delete
```

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
- `Ctrl+C` / `SIGTERM` shut down gracefully: in-flight requests finish and
  queued log/Sentry events are flushed before exit.
- Reads are open on the loopback bind; mutations (`POST /api/fetch`,
  watchlist changes) require the `AMT_API_TOKEN` value. For LAN access, bind
  `0.0.0.0` only behind a reverse proxy with TLS + auth.
- The addon consumes `feed.lua` / `feed-slim.lua`; `feed.json` is the seam
  for the future companion client.
