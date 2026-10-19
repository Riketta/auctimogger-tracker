# Frontend Swap Plan: Server-Rendered UI → Rust WASM (Dioxus / Leptos)

*Status: planning document, no commitment. Written 2026-10 against the repo as it stands
today (workspace `crates/core` / `crates/infra` / `crates/server` + root binary, axum 0.8,
server-rendered HTML in `crates/server/src/web/` — `pages`/`api`/`render`/`assets` modules
with the stylesheet + script embedded from `crates/server/assets/`).*

---

## 1. Summary and recommendation

**Recommendation: do not swap now. Finish the server-rendered UI improvement pass as
planned, and use that pass to complete the JSON API — the API is the swap enabler, and
most of its missing pieces are worth having regardless of any frontend decision. Treat
the WASM swap as a trigger-based option, not a roadmap item. When a swap does become
justified (see the checklist in §8), the default choice is Leptos with Trunk, WASM-only,
no SSR, embedded into the existing binary via `rust-embed`.**

Reasoning in brief:

- **Stay server-rendered (today).** The planned improvements — a design system, charts,
  more pages — are exactly the kind of work server rendering handles well. Hand-rolled
  SVG + a few hundred lines of vanilla JS gets charts and modest interactivity without a
  new toolchain. The repo's defining property is a single static binary with no Node
  anywhere (`Dockerfile`, `.github/workflows/ci.yml`, release tarballs all assume this);
  a WASM framework does not remove that property if done as described in §3, but it
  adds a second build pipeline whose maintenance cost is only repaid by interactivity
  the tool does not currently need.
- **Complete the JSON API (now, regardless).** The `/api/*` surface already covers most
  read paths, but has real gaps for a client-rendered UI (§3.3). Filling them makes the
  server pages easier to build too, and de-risks any future swap by turning the API
  into a contract that has a non-UI consumer.
- **Swap later, strangler-style (§6), if triggers fire.** The plausible triggers are
  live-updating dashboards, heavy cross-page client state, or offline use. Note that
  the first of these is satisfiable with an SSE endpoint plus vanilla JS — a swap is
  only clearly right when *several* triggers hold at once.
- **Leptos over Dioxus (if/when swapping).** For a WASM-only, no-SSR, axum-served ops
  tool, Leptos' fine-grained reactivity is a better fit for frequently refreshing
  market tables than Dioxus' VDOM diffing, Trunk is a lighter build pipeline than the
  `dx` CLI, and Leptos reaching 1.0 in 2025 reduces churn risk relative to Dioxus'
  fast-moving 0.x line. Dioxus remains the right answer if desktop/mobile targets or
  its hot-reload-first workflow ever matter — they do not today. Neither is
  recommended *now*; both carry real dependency-weight and debugging costs (§7).

### Dioxus vs Leptos vs status quo

| | Server-rendered (status quo) | Dioxus | Leptos |
|---|---|---|---|
| First released | — | 2021 (0.x line; 0.7 series current as of 2026) | 2023 (0.x line; **1.0 shipped in 2025**) |
| Rendering model | HTML strings in Rust; full page loads | Virtual DOM, React-like; re-renders subtrees | Fine-grained signals (Solid-like); no VDOM |
| SSR story | It *is* SSR (trivially) | `dioxus-fullstack`/`dioxus-ssr`; good but more machinery | First-class: axum/actix integrations, hydration, islands — unnecessary here |
| Tooling | None beyond `cargo` | `dx` CLI expected for hot reload/assets; opinionated project layout | Trunk for WASM-only builds (plain `cargo` + wasm target); `cargo-leptos` only if SSR |
| Ecosystem size (2026, approx.) | n/a | Larger by stars/downloads (desktop/mobile audience) | Smaller but concentrated on exactly this use case; Yew (2018) is the legacy incumbent, not recommended for new work |
| MSRV | repo floats `stable` (`rust-toolchain.toml`), `rust-version = 1.89` | 0.7 line MSRV well below current stable | 1.0 line MSRV well below current stable |
| Fit for this repo | **Best today** | Good, heavier toolchain | **Best of the two** when swapping |

Ecosystem figures are order-of-magnitude statements; verify exact versions and MSRVs at
decision time — both projects moved quickly through 2025 and will keep doing so.

---

## 2. Why the current UI fits — and when that flips

The server-rendered UI in `crates/server/src/web/` (split into `pages`, `api`,
`render` and embedded `assets`) is unusually well matched to this
product:

- **Single binary, zero toolchain.** Pages are Rust functions returning HTML strings
  (`layout`, `dashboard`, `item_page`, …) with one shared `STYLES` const and small
  helpers (`sparkline` already draws inline SVG bar-charts, `component_bar` draws
  stacked bars). There is no Node, no bundler, no asset step — `cargo build` is the
  whole frontend pipeline, in CI, Docker, and release tarballs alike.
- **Progressive enhancement by construction.** Every page works from a plain `GET`
  with query parameters (`?target=`, `?q=`, `?span=`). Links degrade gracefully;
  the UI is usable from curl, which matters for an ops tool.
- **Tiny attack and failure surface.** No JS execution environment to sandbox beyond
  the browser defaults, no client-side state that can drift from server state, no
  hydration mismatch class of bugs. Authorization is server-side only (reads are open,
  mutations are bearer-gated — §5), so there is no client-side trust boundary to get
  wrong.
- **Dependency hygiene is a stated value.** The root `Cargo.toml` carries a
  paragraph-level `cargo tree` analysis just to keep two ureq versions from growing a
  second TLS stack. `deny.toml` enforces licenses/advisories/bans. A WASM framework
  adds `wasm-bindgen`, `js-sys`, `web-sys`, and framework runtime to that surface —
  acceptable if it earns its keep, pure cost if not.

**When the calculus flips.** Server rendering gets expensive when the product becomes
an *application* rather than pages:

- **Live dashboards**: sub-second or push-based updates (market movers refreshing
  during a cycle) — today every update is a full page load. Note the partial fix is
  cheap: axum can serve SSE and ~30 lines of JS can patch a table; that fix weakens
  the "must swap" argument and should be tried first.
- **Interactive charts**: cross-filtering, brush/zoom across spans, hover-linked
  comparisons of multiple items — beyond what one-off inline SVG wants to be.
- **Client state spanning pages**: e.g. a watchlist dashboard where the user composes
  selections, sorts, and span choices that persist across navigation without server
  round-trips.
- **Offline or latency-sensitive use**: the tool is localhost/LAN today, so this is
  the least likely trigger; if a hosted multi-user deployment ever appears, revisit.

The design-system pass planned now does not cross any of these thresholds. Build it
server-rendered; structure it so page chrome and components are shared Rust helpers
(they already are: `layout`, `STYLES`), which keeps whatever comes later cheaper.

---

## 3. Architecture of a swap

### 3.1 Crate and serving layout

New crate `ui/` (top-level, not under `crates/`) compiling to `wasm32-unknown-unknown`
as a `cdylib`, **excluded from the main workspace** (`workspace.exclude` in the root
`Cargo.toml`):

- `cargo clippy --workspace` / `cargo test --workspace` in CI must keep building
  native-only; a wasm crate in the workspace would either break those lanes or force
  every native job to carry the wasm target.
- Two workspaces mean two `Cargo.lock`s and two cargo-deny passes (§4) — which is the
  honest accounting of the dependency surface anyway.
- The workspace's pedantic/deny-by-default lints (`unwrap_used = "deny"`,
  `panic = "deny"`, …) would otherwise apply verbatim to UI code; excluding the crate
  sidesteps a policy question the owner should answer deliberately, not accidentally.

Serving stays in the existing binary: `crates/server` gains a `rust-embed` (or
`include_bytes!`) embedding of `ui/dist/`, and `build_router_arc` gains fallback
routes (`/{*path}` after the concrete routes) that serve those assets with
content-type + cache headers. Fingerprinted asset names from Trunk's release build
give long cache lifetimes; the WASM payload itself should be served with
`Cache-Control: no-cache` on its loader so updates land without version-skew bugs.

The release/Docker story keeps its "one artifact" property: the asset directory is
embedded at compile time, so `Dockerfile`, the release tarballs
(`auctimogger-tracker-<tag>-<target>.tar.gz`), and the GHCR image all ship the UI
without any extra files.

### 3.2 The JSON API as the contract (as it exists today)

Verified against `build_router_arc` in `crates/server/src/web/mod.rs`:

| Endpoint | Shape today |
|---|---|
| `GET /api/health` | liveness payload (uptime, cycle state) |
| `GET /api/search?q=…&limit=…` | `{hits: [{itemID, name, itemClass, quality}]}`; `limit` defaults to 20 |
| `GET /api/opportunities?target=…&min_churn=&limit=` | ranked rows: `item`, `median7`, `floorNow`, `discount`, `reliability`, `churnPerDay`, `stability`, `samples7`, `confidence`, `score`; `target` required |
| `GET /api/opportunities` / `/api/overview` | served from scheduler-prepared scans (300 s TTL; README "Web UI & JSON API"), so reads never touch the store in steady state |
| `GET /api/overview?target=…` | `{moversUp, moversDown, mostActive}`, hardcoded 15 rows each |
| `GET /api/items/{target_id}/{item_id}/metrics?span=d1,…` | map of span name → metric block (`n`, `min`, `p05` … `max`, `mad`, `vwap`, `qtySum`, `listingCount`, `saleRate`); spans default to `d1` |
| `GET /api/feed.json?target=` | machine feed, the contract for any future companion client |
| `GET/POST/DELETE /api/watchlist` | GET `?target=`; POST body `{target, note?, …item}`; DELETE same minus note; item is a tagged union: `{type:"item", itemID}` or `{type:"pet", speciesID, level, qualityID, breedID}` |
| `POST /api/fetch?target=` | triggers an ingest cycle + immediate feed republish; unknown targets → 400 |

Plus the non-JSON routes a swap must not disturb: `/`, `/snapshots`, `/items`,
`/items/{target_id}/{item_id}`, `/opportunities`, `/overview`, `/feed.lua`,
`/feed-slim.lua`. The `.lua` feed endpoints serve the in-game addon and are
permanently out of scope for the frontend.

### 3.3 API gaps a WASM UI would expose

> **Note (2026-10-19):** the API gaps listed below have since been
> implemented. The live `/api` index (served by the tracker alongside its
> OpenAPI document) reflects the current surface, and the OpenAPI drift test
> in `crates/server/tests/web.rs` pins the document to the router's route
> table, so this list is kept for the historical record only.

These are the concrete server-side tasks a swap implies (all verified absent today):

1. **Pet-addressable metrics.** `api_item_metrics` always constructs
   `ItemRef::Item { item_id }` from the path — pets (whose identity is the
   `speciesID`/`level`/`qualityID`/`breedID` tuple, already modeled in `ApiItemRef`
   and supported by watchlist POST/DELETE) cannot be fetched in one call.
   Needs either a query-parameter variant or a canonical composite ID scheme,
   matched by the HTML `item_link` helper (which now emits dedicated
   `/items/{t}/pet/{species}?level=&quality=&breed=` links — the HTML side is
   solved; only the JSON API still lacks a pet-addressable metrics route).
2. **Bulk metrics.** A watchlist dashboard needs one call, not N ×
   `/api/items/.../metrics`. Add `GET /api/watchlist/metrics?target=&span=` (or accept
   an item list) joining entries to their latest blocks in one SQLite pass.
3. **Snapshot history.** The `/snapshots` page is HTML-only; there is no
   `/api/snapshots`. A client UI (and honestly, any scripted consumer) wants the raw
   archive listing/counters as JSON.
4. **Paging on search.** `SearchParams` is `{q, limit}` with default 20 — no
   offset/cursor. Fine for a completions box; insufficient for a browse page.
5. **Overview configurability.** The 15-rows-per-section cap is hardcoded in
   `api_overview`; a dashboard would want it as a parameter.

None of these are large. Doing them *before* any UI swap — with JSON-level tests in
`crates/server` — is the cheap insurance that the contract is right before two
clients depend on it.

---

## 4. Build & CI impact

**Toolchain.** `rust-toolchain.toml` currently pins `stable` + minimal profile +
clippy/rustfmt. Add `targets = ["wasm32-unknown-unknown"]`. Because the channel floats
`stable`, framework MSRV bumps are a non-issue in practice — the repo always rides
current stable, and the root package already declares `rust-version = 1.89`.

**GitHub Actions** (`.github/workflows/ci.yml` today: `linux` = fmt/clippy/test/deny
(+ lua5.4 for the feed verifier), `windows` = clippy/test/build, `docker` = buildx →
GHCR gated to main/tags, `release` = matrix `x86_64-unknown-linux-gnu` +
`x86_64-pc-windows-msvc` with `--locked` builds packaged with `config.example.toml`,
`publish` = attaches archives to the tag release):

- New `ui` job (linux): install the wasm target + Trunk (pinned version — Trunk must
  match the `wasm-bindgen` version resolved in `ui/Cargo.lock`; pinning both avoids
  the classic mismatch failure), then `trunk build --release` (which runs
  wasm-bindgen and optionally `wasm-opt`), then `cargo clippy -p amtracker-ui
  --target wasm32-unknown-unknown` and `cargo test -p amtracker-ui --target
  wasm32-unknown-unknown` for logic that survives headless testing. Cache with
  `Swatinem/rust-cache` keyed on the wasm target.
- `linux`/`windows` native jobs are unchanged *if* the UI crate is workspace-excluded
  (§3.1); otherwise every job needs the wasm target, which is the worse trade.
- Docker job: `Dockerfile` build stage (`rust:1-bookworm`, currently three cache
  mounts around a single `cargo build`) grows a UI stage: install the wasm target +
  Trunk, `trunk build --release` into `ui/dist`, then the existing `cargo build`
  picks it up through `rust-embed`. The runtime stage is untouched — assets live
  inside the binary. Expect several added minutes to image builds (wasm codegen +
  bindgen + wasm-opt), mitigated by a separate cache mount for `ui/target`.
- Release job: **no changes** if assets are embedded — the tarball remains binary +
  `config.example.toml`. This is the strongest argument for embedding over shipping a
  `web/` directory alongside the binary.
- **cargo-deny** (`deny.toml`: license allow-list, advisories v2, `multiple-versions
  = "warn"`, crates.io-only sources): run a second `cargo deny check` in `ui/`
  against its own lockfile. The wasm tree pulls `wasm-bindgen`/`js-sys`/`web-sys` and
  needs `getrandom`'s wasm feature — expect the multiple-versions warning count to
  rise; the license allow-list likely needs no change (same ecosystem, same
  licenses). Advisories in that tree are rare but not unheard of; the gate earns its
  keep precisely there.
- Binary size impact is modest and honest: `profile.release` already sets `lto` +
  `strip`; a realistic Leptos/Dioxus dashboard app lands in the low hundreds of KB
  gzipped, i.e. single-digit percent of the current binary. Parse/compile time of the
  wasm module per page load is negligible on localhost.

---

## 5. State and statefulness

- **URL as state.** The server pages already treat query parameters as the entire
  state model (`target_param` / `required_target` / `known_target` in
  `crates/server/src/web/mod.rs`). A
  WASM UI should keep exactly that: targets, search queries, selected spans, table
  filters live in the URL, navigation is just routing. This preserves deep links,
  makes browser back/forward correct, and means no global store, no redux-equivalent,
  no client-side cache invalidation problem is needed at the start. Leptos signals
  synced to the URL (or Dioxus router with query params) cover it.
- **Server state stays authoritative.** Opportunities/overview read from
  scheduler-prepared scans, not from the store; a client UI polling these endpoints
  inherits that freshness model unchanged. Add an SSE endpoint (`/api/events`) only
  when dashboards need push; it is additive server work either way.
- **Token handling.** Mutations (`/api/watchlist` POST/DELETE, `/api/fetch`) are
  bearer-gated: `expected_api_token` reads the env var named by
  `server.token_env` from *the server's* environment, with empty/unset meaning open
  mode (documented developer behavior). A WASM UI cannot read server env, so:
  - Keep the token **in memory only** (or `sessionStorage` at most) — never
    `localStorage` — entered on first 401 (the API already answers with
    `WWW-Authenticate: Bearer`, which the UI can probe for), then attached as
    `Authorization: Bearer …` on mutating fetches. The comparison is server-side and
    constant-time; nothing about it changes.
  - The UI must handle open mode gracefully: no prompt when the first mutation
    succeeds without a token. Probing on first mutation rather than on load keeps
    read-only visitors unaffected (reads are unauthenticated by design).
  - Alternative worth considering for phase one: keep mutations on the
    server-rendered pages (plain HTML forms already work) and ship the WASM UI
    read-only. This is the lowest-risk first increment and defers the token-prompt
    UX entirely.

---

## 6. Migration strategy

**Recommended: strangler, page by page, with the JSON API completed first.**

1. **Phase 0 (do regardless of any swap decision):** fill the API gaps of §3.3 with
   JSON tests. This is useful to the current UI work (a `/snapshots` JSON endpoint
   and paging are wanted anyway) and is the hard prerequisite for everything after.
2. **Phase 1 — first WASM page:** the item detail page
   (`/items/{target_id}/{item_id}`). It is the most chart-heavy, most likely to grow
   interactivity, and least navigation-critical — the ideal strangler candidate.
   Scaffold `ui/`, Trunk + `rust-embed` serving, server route kept live underneath.
3. **Phase 2 — read-only dashboards:** opportunities and overview (or a merged
   dashboard view) once live-refresh matters. Still no mutations; token UX deferred.
4. **Phase 3 — mutations + remaining pages:** watchlist management, search/browse,
   snapshots; decide the token prompt UX (§5).
5. **Phase 4 — only if ever:** remove server-rendered page handlers. The `.lua`
   feed routes and `/api/*` are permanent.

Why not a clean cut-over: it couples the swap to the whole API-gap list landing
atomically, blocks shipping the design-system work for weeks, and risks a big-bang
regression across all six pages at once. The strangler path keeps every commit
shippable, keeps the server pages as the always-working fallback (they are the
same handlers serving the JSON the new UI calls), and lets the swap die quietly at
any phase with value already banked. The one discipline it requires: the server
page and the WASM page must not both be "the" URL for long — ship the WASM page at
the same route with the server page demoted to a fallback (e.g. `?render=server`)
or removed per page, so state and links don't fork.

---

## 7. Effort estimate and risks

**Effort** (solo maintainer, part-time, order-of-magnitude):

- Phase 0 API completion: 1–2 weeks. (Wanted anyway.)
- Phase 1 scaffold: `ui/` crate, Trunk pipeline, `rust-embed` serving, CI `ui` job,
  Dockerfile stage, second cargo-deny pass: 3–5 days.
- Each subsequent page port: 1–3 days, faster as shared components accumulate.
- Full swap through Phase 3: roughly 4–6 calendar weeks part-time. The server-rendered
  improvement pass, by contrast, is days per page — that gap *is* the decision.

**Risks, honestly stated:**

- **Framework churn.** Both projects are young (Leptos hit 1.0 in 2025; Dioxus is on
  a fast 0.x cadence with breaking changes between minors). Budget for a
  follow-the-migration-guide upgrade roughly once or twice a year.
- **Dependency weight.** `wasm-bindgen`/`web-sys`/`js-sys` plus the runtime is a
  second dependency universe with its own advisory surface, its own lockfile, and
  its own version-pinning rules (wasm-bindgen CLI vs crate). This repo's root
  `Cargo.toml` shows how much the owner already invests in keeping the tree lean;
  the wasm tree runs counter to that instinct and must justify itself.
- **Build times.** CI gains a wasm lane; Docker image builds gain minutes
  (bindgen + `wasm-opt`). Cache-key discipline keeps this bounded but nonzero.
- **Debugging.** Panics in wasm surface as console errors; `tracing-wasm` and
  source maps help but it is strictly worse than a Rust backtrace in a terminal.
  The workspace's `panic = "deny"` culture helps here — adopt it in `ui/` too.
- **Team familiarity.** This is effectively a solo project; whichever framework is
  chosen, its idiom (signals vs VDOM, effect semantics, hydration pitfalls — none
  needed here since no SSR) is a learning curve measured in the first ported page.
- **Lint friction.** If `ui/` joined the workspace, pedantic clippy +
  `unwrap/expect/panic = deny` would fight some framework ergonomics; the
  exclusion in §3.1 is also a deliberate policy exception that should be recorded
  in the decision.

---

## 8. Decision checklist — signals it is time

Swap (start Phase 1) when **at least three** of these hold:

- [ ] A page needs sub-second or push-based updates (SSE + vanilla JS was tried
      first and proved insufficient or uglier than a framework).
- [ ] Charts have graduated from static rendering to interactive cross-filtering /
      zoom / multi-item comparison.
- [ ] Client-side state spans ≥ 3 pages and URL-params are genuinely awkward
      (composable watchlist dashboards, saved views).
- [ ] The inline/asset JS in server pages has grown past ~500 lines or a second
      person needs to modify it — i.e. the "no framework" tax is already being paid.
- [ ] Offline or installable use has become a requirement.
- [ ] The §3.3 API gaps are closed (prerequisite, not a signal by itself).

Stay server-rendered while fewer than three hold — and specifically: **do not swap as
part of, or as a prerequisite of, the current design-system improvement pass.** The
design system, the shared component helpers, and the completed JSON API are all
forward-compatible with either future.
