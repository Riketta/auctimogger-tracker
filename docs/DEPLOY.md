# Deploying auctimogger-tracker (Linux server)

The tracker is a **single static-data binary**: `serve` runs the poll
scheduler and the web UI/API in one process, state lives in SQLite (WAL), and
the tracker feed for the in-game addon is written under `feeds/`. There is no
external database and no other service to install.

## Layout

| Path | Purpose |
|---|---|
| `/opt/auctimogger-tracker/auctimogger-tracker` | release binary (CI artifact) |
| `/etc/auctimogger-tracker/config.toml` | configuration (copy of `config.example.toml`) |
| `/etc/auctimogger-tracker/env` | secrets only — Blizzard token(s), `chmod 640 root:tracker` |
| `/var/lib/auctimogger-tracker/` | **WorkingDirectory**: SQLite DB, raw archive, `feeds/` |

`config.toml` should use absolute paths under `/var/lib/auctimogger-tracker`:

```toml
[server]
bind = "127.0.0.1:8080"     # front with a reverse proxy for remote access
token_env = "AMT_API_TOKEN" # gates mutating endpoints (POST /api/fetch, watchlist)

[storage]
database = "/var/lib/auctimogger-tracker/data/tracker.sqlite3"
archive_dir = "/var/lib/auctimogger-tracker/data/raw"
```

Secrets (the env file is sourced by systemd, `KEY=value` lines only):

```sh
# /etc/auctimogger-tracker/env
AMT_TOKEN=<blizzard bearer token>
AMT_API_TOKEN=<random string for the tracker's own mutation API>
```

## First-time install

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

Verify:

```sh
curl -s http://127.0.0.1:8080/api/health
journalctl -u auctimogger-tracker -f
```

## One-command updates

Once the service is installed, updating to a released version is a single
command on the server:

```sh
sudo AUCTIMOGGER_REPO=owner/repo ./scripts/install-release.sh          # latest
sudo AUCTIMOGGER_REPO=owner/repo ./scripts/install-release.sh v0.2.0   # pinned
```

The script downloads the `x86_64-unknown-linux-gnu` archive from GitHub
releases, swaps the binary under `/opt`, and restarts the unit.

**Binary compatibility:** CI builds the Linux artifact on `ubuntu-latest`
(currently glibc 2.39). Older glibc on the target host will refuse to run it;
in that case build on the host (`cargo build --release`) or ask for a musl
target — the source makes no assumptions beyond rusqlite (bundled) and ureq.

## Storage, WAL, and the filesystem

- SQLite runs in **WAL mode**. Expect three files: `tracker.sqlite3`,
  `tracker.sqlite3-wal`, `tracker.sqlite3-shm`. The `-wal`/`-shm` files are
  part of the database — never delete or move them separately.
- Keep the DB on **local disk** (or a block device that honors mmap/locks).
  NFS/CIFS network mounts can corrupt WAL databases and are not supported.
- Raw listing snapshots are pruned after `storage.raw_listing_days` (default
  7); day-folds, snapshot metadata, and the name catalog are permanent. A
  multi-month DB stays in the low single-digit GB.

## Backups

Do **not** `cp` a live WAL database by itself — a bare file copy can miss
committed transactions sitting in the `-wal` file. Use SQLite's backup API:

```sh
# Online, consistent, safe while the service runs:
sqlite3 /var/lib/auctimogger-tracker/data/tracker.sqlite3 \
    ".backup '/backups/tracker-$(date +%F).sqlite3'"
```

Or, with the service stopped, copy all three files together (or checkpoint
first: `PRAGMA wal_checkpoint(TRUNCATE);` then copy the single `.sqlite3`).

Cron example (nightly, keep 14):

```cron
0 4 * * * sqlite3 /var/lib/auctimogger-tracker/data/tracker.sqlite3 ".backup '/backups/tracker-$(date +\%F).sqlite3'" && find /backups -name 'tracker-*.sqlite3' -mtime +14 -delete
```

## Operations

- `systemctl status auctimogger-tracker` / `journalctl -u auctimogger-tracker`.
- The service restarts on failure (`Restart=on-failure`, 5 s backoff).
- Health probe for uptime monitoring: `GET /api/health`.
- Mutating endpoints (`POST /api/fetch`, watchlist changes) require the
  `AMT_API_TOKEN` value (config `server.token_env`); reads are open on the
  loopback bind. For LAN access, bind `0.0.0.0` only behind a reverse proxy
  with TLS + auth.
- The in-game addon consumes `http://host:8080/feed.lua` (full) or
  `/feed-slim.lua` (watchlist); JSON at `/api/feed.json` for the future
  companion client.
