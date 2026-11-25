# SPEC — AuctiMogger Database

Status: **in progress**. Batches 1 to 7 are implemented: the domain types,
the record codec, the carrier and JSON-twin renderers, the golden fixtures,
the tracker side that feeds them — the export service, publication and
the v4 routes, feed v3 being gone — the addon's core, its tooltip, the
`/amdb` command and the options panel, the tracker's addon bundle with
the `addon-sync` command, and the sniper reading the database through its
`MarketDatabasePort`. The sniper's tooltip removal (batch 8) follows in
§11. This file is the single source of truth for the AuctiMogger Database
addon and for the tracker feed (v4) it consumes. When this file and the code
disagree, the code wins — fix the doc in the same change.

Scope: everything under `addon/` plus the tracker-side feed export, publication
and sync command. The sniper addon (`Riketta/AuctiMogger`) has its own SPEC; §8
lists the changes made there.

## Contents

1. [Purpose](#1-purpose)
2. [Decisions](#2-decisions)
3. [Vocabulary](#3-vocabulary)
4. [System overview](#4-system-overview)
5. [Data contract (feed v4)](#5-data-contract-feed-v4)
6. [Tracker implementation](#6-tracker-implementation)
7. [Addon implementation](#7-addon-implementation)
8. [Sniper migration](#8-sniper-migration)
9. [Quality gates](#9-quality-gates)
10. [Test plan](#10-test-plan)
11. [Batches and steps](#11-batches-and-steps)
12. [Risks and mitigations](#12-risks-and-mitigations)
13. [Known limitations and later work](#13-known-limitations-and-later-work)
14. [Appendix A — research evidence](#appendix-a--research-evidence)
15. [Appendix B — extraction inventory](#appendix-b--extraction-inventory)
16. [Appendix C — references](#appendix-c--references)

## 1. Purpose

AuctiMogger Database is an in-game World of Warcraft addon, owned by this
repository, that turns the tracker's market data into **detailed item
tooltips** and exposes the same data through a **public query API**.

Goals:

- Every item tooltip (bags, bank, mail, chat links, auction house rows) shows
  what the item is worth, what it costs right now, how reliably it sells and
  whether it is a deal — the numbers the web UI already computes.
- One store serves the tooltip, the sniper addon and any third-party addon.
- The tracker repository owns the whole contract: encoder, decoder, tooltip,
  tests. One CI proves both sides agree.

Non-goals:

- Buying, sniping or any auction-house query. The addon never calls
  `C_AuctionHouse`, so it uses no search budget and touches nothing protected.
- Recording live observations (the sniper keeps its live tail).
- Reloading data without a `/reload`.

## 2. Decisions

Locked with the project owner; do not reopen without a new decision.

| # | Decision | Why |
|---|---|---|
| 1 | The addon lives at `addon/AuctiMoggerDatabase/` in this repo, titled "AuctiMogger Database". The `AuctiMoggerTracker` carrier addon is retired. | Contract, codec and reader ship from one repo. |
| 2 | The sniper repository is changed in the same effort (§8). | It is the only consumer of the old feed. |
| 3 | The sniper's dependency is **optional** (`## OptionalDeps`). | The sniper still works on its own live evidence without the database. |
| 4 | Records are **packed** into printable strings, not Lua tables. | Verbose tables cost 42 MB per market today (Appendix A); packed is about 2 MB. Loading several markets together is only viable packed. |
| 5 | **One data file per market.** The addon has six slots; any subset loads. The primary market is bound per character; the others show as cross-market lines. | Markets are independent; the owner runs three. |
| 6 | The tooltip is an **attached frame of our own** (TSM / OribosExchange style). The game's tooltip is never modified. | Keeps the game tooltip clean and avoids the re-entrancy hazard of inline rendering. |
| 7 | Target clients: **Forever (interface 16001) and retail**, both tooltip hook paths. | Owner runs Forever; retail stays compatible. |
| 8 | Every optional line can be switched off; presets `basic` and `standard`. | "Keep the tooltip clear if the user wants just basic auction info." |
| 9 | **Feed v3 is removed now**, with no compatibility period. | No real users exist. |

## 3. Vocabulary

- **Tracker** — `auctimogger-tracker`, the Rust server in this repository.
- **Market** — one tracker target: one auction house (or connected realm).
  One market produces exactly one data file.
- **Carrier** — the generated Lua data file for one market.
- **Slot** — one of `Market1.lua` … `Market6.lua` in the addon folder. A
  carrier is installed by copying it over a slot.
- **Record** — the packed string holding every statistic of one identity in one
  market category.
- **Primary market** — the market whose numbers the tooltip shows; the others
  appear as cross-market lines.
- **Reference value** — what an item is worth: the 14-day sold median when
  enough priced units sold, else the 7-day listing value.
- **bp** — basis points: a fraction stored as an integer, `round(x × 10000)`.

## 4. System overview

```
tracker (Rust)                                        game client
+---------------------------------------------+       +----------------------------------------------+
| prepared scans, metric blocks, crafts       |       | AuctiMoggerDatabase (addon)                  |
|   +-- DatabaseExportService                 |       |   Market1.lua ... Market6.lua  (slots)       |
|        +-- DatabaseCodec                    |       |     +-- market store (packed strings)        |
|              +-- feed.lua      (packed)     | copy  |          +-- tooltip   (own attached frame)  |
|              +-- feed.json     (decoded)    | ----> |          +-- AuctiMoggerDatabase  (API)      |
| scheduler: refresh scans, then publish      |  or   |                +-- AuctiMogger  (sniper)     |
|                                             | addon |                                              |
+---------------------------------------------+ -sync +----------------------------------------------+
```

- The scheduler publishes each market's `feed.lua` and `feed.json` after
  successful ingest cycles, throttled by `[server].feed_publish_secs`.
- The carrier reaches the client by file copy or by
  `auctimogger-tracker addon-sync` (§6.7). A `/reload` loads it.
- The addon decodes one record per lookup. Nothing is persisted in
  SavedVariables except settings.

## 5. Data contract (feed v4)

### 5.1 Files

Per target, under `[storage].feed_dir` (unchanged config key, unchanged paths):

| File | Content | Content type |
|---|---|---|
| `<feed_dir>/<target>/feed.lua` | Carrier: market header + packed records | `text/x-lua` |
| `<feed_dir>/<target>/feed.json` | Decoded twin of the same data | `application/json` |

`feed.json` is the debugging lens for the opaque carrier and the oracle of the
cross-language test. Served at `GET /feed.lua?target=` and
`GET /api/feed.json?target=` as today. `/feed-slim.lua` is removed.

### 5.2 Carrier file

```lua
-- AuctiMogger Database market file - GENERATED by auctimogger-tracker 0.1.0. Do not hand-edit.
-- Install: copy over a slot of the AuctiMoggerDatabase addon (Market1.lua .. Market6.lua), then /reload.
local _, A = ...
local m = A.BeginMarket({
    version = 4,
    trackerVersion = "0.1.0",
    target = "forever-eu-pvp-alliance",
    flavor = "forever",
    region = "eu",
    rulesetId = 1,
    houseId = 111,
    generatedAt = 1795559034,
    auctionsAt = 1795558000,
    commoditiesAt = 1795556000,
})
m.items[194784] = "0Az9Qm..."
m.commodities[191460] = "1Bz..."
m.pets[2546] = "7Q..."
A.EndMarket()
```

Rules:

- The file is pure ASCII. Record strings use only the alphabet of §5.4, so
  no escaping is ever needed.
- One record per line, in fixed order: `items` (ascending id), then
  `commodities` (ascending id), then `pets` (ascending species id).
- `...` is the client's standard addon argument pair; the addon's namespace
  table is the second value. Specs load a carrier with
  `loadfile(path)("AuctiMoggerDatabase", namespace)`.
- The tracker renders a given snapshot byte-for-byte identically every time.

Market header fields:

| Field | Type | Required | Meaning |
|---|---|---|---|
| `version` | integer | yes | Contract version; only `4` is accepted. |
| `trackerVersion` | string | yes | Tracker version that generated the file (diagnostics). |
| `target` | string | yes | Tracker target id — the market's key. It must match `[a-z0-9][a-z0-9._-]*`, the tracker's own id grammar; anything else is a malformed header. |
| `flavor` | string | no | Catalog flavor: `forever` or `retail`. |
| `region` | string | no | API region slug. |
| `rulesetId` | integer | no | Forever ruleset id. |
| `houseId` | integer | no | Forever auction house id. |
| `connectedRealmId` | integer | no | Connected realm id (retail targets). |
| `generatedAt` | integer | yes | Newest source `Last-Modified` across payload kinds, else publish time. The market's freshness stamp. |
| `auctionsAt` | integer | no | Fetch time of the newest auctions snapshot. |
| `commoditiesAt` | integer | no | Fetch time of the newest commodities snapshot. |

### 5.3 Identity and keys

- `items[itemID]` — plain items traded in the item auction house.
- `commodities[itemID]` — stackables traded in the commodity auction house.
- `pets[speciesID]` — all variants of a battle-pet species pooled, as in v3.
- The same item id may exist in both `items` and `commodities`: they are
  separate evidence pools.
- Lookup order when no category is given: `items`, then `commodities`.
  Today's feed has no id in both tables; callers that care pass a category.

### 5.4 Packing primitives

- **Byte stream:** every field is an unsigned LEB128 varint (7 bits per byte,
  low group first, continuation bit `0x80`).
- **Signed values:** zigzag (`0, -1, 1, -2, 2 …`) as a varint.
- **Text:** the byte stream is read as a big-endian bit string and cut into
  6-bit symbols; the last symbol is zero-padded on the right. Each symbol maps
  to one character of
  `ALPHABET = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz+."`
  (index 0–63). The alphabet contains no quote, backslash, bracket, space or
  control character.
- **End of record:** after the last field at most 4 bits remain — a byte stream
  is padded with 0, 2 or 4 zero bits — and they are all zero. Anything else is a
  malformed record.
- **Integers:** every value is below 2^53 in magnitude, signed or not, so Lua
  5.1 doubles hold it exactly. A zigzag code is up to `2^54 − 2`: a decoder
  keeps the sign bit apart and accumulates `floor(code / 2)` as the magnitude,
  so no intermediate exceeds 2^53. That accumulated magnitude is not yet the
  value: a code with the sign bit set decodes to `−(magnitude + 1)` — codes
  `1, 3, 5` are `−1, −2, −3` — never to `−magnitude`.

The Rust decoder (`crates/infra/src/record.rs`) is the reference; the addon's
Lua decoder refuses exactly the same things:

| Malformed when |
|---|---|
| A character is not in `ALPHABET`. |
| A varint is longer than 10 bytes, or not minimal (a trailing zero group, as in `80 00`). |
| An integer is `2^53` or more in magnitude. |
| The padding is more than 4 bits or not all zero. |
| A flag word sets a bit with no meaning, or `qtySum` is set without `listed`. |
| An `evidence` or `position` group is announced in `flags` with a flag word of zero: the encoder leaves such a group out. |
| `supplyDays` or `lowRank30d` is above 65535; `rank`, `scoreBp` or `discountBp` is above `2^32 − 1`; the `lows` count is above 30. |
| No span is present, or bytes follow the last field. |

Every text the decoder accepts re-encodes to exactly the same text, which is
how the two decoders are held equal.

### 5.5 Record layout

```
flags      varint   group presence: 0x01 live   0x02 value   0x04 evidence
                                 0x08 position 0x10 vendor  0x20 craft  0x40 deal
spans      varint   span presence:  0x01 d1  0x02 d7  0x04 d14  0x08 d30

per present span, in order d1, d7, d14, d30:
  sflags  varint   0x01 saleRate  0x02 saleRate180d  0x04 daysSinceLastSale
                   0x08 closuresSinceLastSale  0x10 qtySold
  n, min, p05, p25, median, p75, p95, max, mad, vwap, qtySum, listingCount
  [saleRate bp] [saleRate180d bp] [daysSinceLastSale] [closuresSinceLastSale] [qtySold]

live:
  lflags  varint   0x01 listed  0x02 qtySum  0x04 seenToday
  [floor] [floorQty] [listed]      [qtySum]      [seenFloor] [seenQty]

value:
  vflags  varint   0x01 soldMedian14  0x02 trendBp
  [value7] [soldMedian14] [samples7] [trendBp zigzag]

evidence:
  eflags  varint   0x01 soldCount  0x02 expiredCount  0x04 supplyDays
  [soldCount] [expiredCount] [supplyDays]

position:
  pflags  varint   0x01 low30  0x02 lowRank30d  0x04 lows
  [low30] [lowRank30d bp] [lowsCount] [lowsCount bytes]

vendor:     [vendorPrice]

craft:      [craftCost] [profit zigzag] [expectedProfit zigzag]

deal:       [rank] [scoreBp] [discountBp]
```

The span block carries exactly the seventeen v3 fields, so the sniper's math is
unchanged. A record exists only when it has at least one span.

Brackets mark a field that is written when its flag is set; the other fields
of a present group are always written. Three flags cover more than one field,
or depend on another:

- `listed` writes `floor`, `floorQty` and `listed` together. An item seen today
  and already gone has a `live` group without it, and then has no floor.
- `qtySum` describes that listing summary, so it is only written inside a
  `listed` group.
- `seenToday` writes `seenFloor` and `seenQty` together.

### 5.6 Field definitions

Every field is copper or a count unless stated. "Optional" means absent when
the tracker has no evidence — never zero.

| Group | Field | Source in the tracker | Meaning |
|---|---|---|---|
| spans | `n` … `listingCount`, `saleRate`, `saleRate180d`, `daysSinceLastSale`, `closuresSinceLastSale`, `qtySold` | metric blocks | The v3 span block. Rates in bp. |
| live | `floor` | newest snapshot | Cheapest unit price listed right now. |
| live | `floorQty` | newest snapshot | Quantity listed at exactly that price. |
| live | `listed` | newest snapshot | Listings of the item in the snapshot. |
| live | `qtySum` | newest snapshot | Total quantity listed. |
| live | `seenFloor`, `seenQty` | prepared scan | Today's cheapest listing that is no longer listed (bought or cancelled). |
| value | `value7` | prepared scan | Quantity-weighted 7-day median listing price. Present only when > 0. |
| value | `soldMedian14` | prepared scan | 14-day quantity-weighted median of sold prices. Present only once ≥ 3 priced units sold. |
| value | `samples7` | prepared scan | Unique listings behind `value7`. |
| value | `trendBp` | prepared scan | Today's listing median against the previous six days, in bp (signed). |
| evidence | `soldCount` | prepared scan | Listings credited sold in 14 days. |
| evidence | `expiredCount` | prepared scan | Credit-eligible closes that ran out unsold in 14 days. |
| evidence | `supplyDays` | prepared scan | Listed quantity ÷ units sold per day. Saturates at 65535 (shown as ∞). |
| position | `low30` | prepared scan | Lowest ask of the previous 30 days. |
| position | `lowRank30d` | prepared scan | Share of those 30 days whose lowest ask sat above the live floor, in bp. Needs a live floor and ≥ 5 days of history. |
| position | `lows` | prepared scan | The previous 30 days' lowest asks, oldest first. Each day is `clamp(round(low × 128 ÷ reference), 0, 255)`. Sparkline only. |
| vendor | `vendorPrice` | item catalog | Vendor sell price. |
| craft | `craftCost` | craft service | Reagent cost per make. |
| craft | `profit` | craft service | Output value − house cut − cost, per make (signed). |
| craft | `expectedProfit` | craft service | Profit × sale rate, rounded (signed). |
| deal | `rank` | opportunities | 1-based rank by score among the category's live deals. |
| deal | `scoreBp` | opportunities | The ranking score, in bp. |
| deal | `discountBp` | opportunities | `1 − floor ÷ reference`, in bp. |

Rules the encoder guarantees:

- The `value` group exists only with a `value7` above zero, and always carries
  `samples7`; `soldMedian14` and `trendBp` ride along when known.
- The `position` group is present only when the `value` group is (the sparkline
  needs a reference) and the item has at least one daily low.
- The `craft` group attaches to the output's record in the category the craft
  was priced in, taking the ranked recipe with the highest expected profit.
- The `deal` group attaches only to ranked live deals.
- A pet species pooled from several variants exports spans only (no other
  group). A species with a single variant gets full groups.
- The record encoder never fails and never wraps. A negative value in an
  unsigned field (a tracker bug, never real data) is written as 0; a value
  above `2^53 − 1`, and a signed value beyond `±(2^53 − 1)`, saturates there. A
  non-finite rate is left absent rather than written as a zero rate.
- `supplyDays` and `lowRank30d` are `u16` on both sides; `rank`, `scoreBp` and
  `discountBp` are `u32`.

### 5.7 Absence semantics

Absent and zero are different for every optional field:

- `live` absent — the item is not listed and was not seen today.
- `live` present without `listed` — seen today, already gone.
- `soldCount` absent — no credit-eligible evidence. `soldCount = 0` — evidence
  exists and nothing sold.
- `supplyDays` absent — no sold evidence, or nothing sold: such stock never
  clears and is shown as ∞. `65535` — at least 65535 days of stock, also shown
  as ∞.
- `daysSinceLastSale` absent — the ledger never credited a sale.
- `lowRank30d` absent — fewer than 5 days of history or no live floor.
- The decoder returns `nil`, never `false` or `0`, for an absent field.

### 5.8 Values derived by the decoder

These are computed identically in Lua so the tooltip and the API agree:

| Derived | Rule |
|---|---|
| `reference()` | `soldMedian14` if > 0, else `value7` if > 0, else the first of d7, d14, d30, d1 `median` that is > 0. |
| `valueSource()` | `sold`, `listed`, or the span name used by `reference()`. |
| `saleRate14()` | First of d1, d7 `saleRate`. |
| `saleRate180d()` | First of d1, d7, d14, d30 `saleRate180d`. |
| `daysSinceLastSale()`, `closuresSinceLastSale()` | First present across d1, d7, d14, d30. |
| `soldPerDay()` | d14 `qtySold` ÷ 14, else d7 ÷ 7, else d1. |
| `atVendorFloor()` | `vendorPrice` present and `floor ≤ vendorPrice`. |
| Supply is unbounded | `supplyDays` absent while `soldCount` is present and 0. |

### 5.9 Validation rules

Applied by the addon when a carrier registers:

| Condition | Result |
|---|---|
| `version` missing or ≠ 4 | Market rejected; reported in chat and `/amdb status`. |
| Required header field missing or malformed, or `target` outside the tracker's id grammar | Market rejected, reported. An optional field of the wrong type is dropped instead. |
| `generatedAt` more than 5 minutes in the future | Market rejected (diverged generator), reported. |
| `generatedAt` older than 24 hours | Market **kept and flagged stale**: tooltips render with a red age badge; `GetSpans` answers `nil`, so a caller that values on spans gets no stale evidence. (The sniper reads `GetMarketInfo` and applies its own ceiling, §8.) |
| Same `target` already registered from another slot | The later slot replaces the earlier one, keeps its own (later) slot number and moves to the end of the slot order; reported as a warning. |
| A record fails to decode | That record is dropped and counted; the market stays. Reported once per market at login. |
| Slot file is a placeholder or fails to compile | Nothing registers; a slot that fails to compile shows the client's own load error. |

Every commit takes the next slot number in load order, a rejected one included,
so the numbers match `Market1.lua` … `Market6.lua`. A rejected market is
remembered for `/amdb status`. A commit's reports are recorded while the slot
files load and printed once at login.

Stale data is shown rather than hidden on purpose: a tooltip saying "2 days
old" is more useful than an empty one, and the age is always visible.

### 5.10 JSON twin

```json
{
  "version": 4,
  "trackerVersion": "0.1.0",
  "target": "forever-eu-pvp-alliance",
  "market": { "region": "eu", "rulesetId": 1, "houseId": 111, "flavor": "forever" },
  "generatedAt": 1795559034,
  "auctionsAt": 1795558000,
  "commoditiesAt": 1795556000,
  "items": [
    {
      "itemID": 194784,
      "category": "commodity",
      "spans": { "d1": { "n": 58, "min": 575, "...": "..." } },
      "live": { "floor": 3805000, "floorQty": 214, "listed": 618, "qtySum": 1234 },
      "value7": 4123500,
      "soldMedian14": 4021000,
      "samples7": 214,
      "trendBp": -300,
      "soldCount": 34, "expiredCount": 60, "supplyDays": 39,
      "low30": 3500000, "lowRank30d": 8000, "lows": [128, 130, 127],
      "vendorPrice": 125000,
      "craft": { "cost": 21000000, "profit": 19000000, "expectedProfit": 6840000 },
      "deal": { "rank": 12, "scoreBp": 7100, "discountBp": 3800 }
    }
  ]
}
```

Pets use `"kind": "pet", "speciesID": 2546` instead of `itemID`. Absent groups
are `null`; absent scalars are omitted.

The twin is rendered from the decoded records, so it shows exactly what a
decoder reads: a rate the ranking computes as `34 / 94` appears as `0.3617`. A
record seen today and gone has `"live": { "seenFloor": …, "seenQty": … }` and
no floor. Keys come in the order shown above (top level, market, entry, span
block `n` … `qtySold`, live, craft, deal), and span blocks as `d1`, `d7`,
`d14`, `d30`. The file is pretty-printed with two-space indentation and no
trailing newline.

### 5.11 Golden fixtures

Three committed files under `fixtures/`, all rendered from one hand-written
snapshot (`crates/infra/tests/database_golden.rs`): the market
`forever-eu-pvp-alliance` with nine records.

| File | Content | Consumer |
|---|---|---|
| `database_v4.lua` | The carrier. | The Lua spec decodes every record. |
| `database_v4.json` | Its JSON twin. | Humans, agents, the Rust JSON test. |
| `database_v4_decoded.lua` | The oracle: `return { meta = {…}, items = { [id] = record, … }, commodities = {…}, pets = {…} }` — every record as a decoder must read it, rates written as `bp / 10000`. | The Lua spec compares field for field. |

The nine records:

| Table and key | What it exercises |
|---|---|
| `items[25]` | A steady seller: four spans, listed, a rising trend, sold evidence, a 12-day sparkline, a vendor price. |
| `items[117]` | A listing that never sells: prices near 10 000 000 000 copper (five-byte varints), a present zero sale rate and zero sold, `supplyDays` 65535, a trend of 0, the top deal. |
| `items[2589]` | Seen today and gone: a `live` group with `seenFloor`/`seenQty` and no floor; a sold-source value. |
| `items[194784]` | One side of the dual-category id: spans only, never a credited sale. |
| `commodities[191460]` | A losing craft, a listing without `qtySum`, a one-day sparkline, a rising trend. |
| `commodities[191461]` | Bulk: quantities past a billion, a full 30-day sparkline, `lowRank30d` 10000, a falling trend. |
| `commodities[194784]` | The other side of the dual id, and the record with every group: the example of §5.10. |
| `pets[293]` | A pooled species: one span, no ledger field. |
| `pets[2546]` | A single-variant species with full groups. |

- A Rust test re-renders all three and asserts they are byte-identical to the
  committed files. `AMTRACKER_UPDATE_GOLDEN=1 cargo test -p amtracker-infra
  --test database_golden` rewrites them.
- Two more Rust tests keep the fixture honest: every carrier record must decode
  back to the snapshot, and an edit that drops a case (a flag never set, a sign
  never used) fails a coverage test.
- `.gitattributes` keeps the three files LF in every working tree; the byte
  comparison would otherwise fail on a CRLF checkout.
- Checked under Lua 5.1.5: the carrier loads through `A.BeginMarket`, its
  header arrives intact, and all nine words use the alphabet only.
- A Lua spec loads `database_v4.lua`, decodes each record and asserts
  field-for-field equality with `database_v4_decoded.lua`.
- Any change to the layout therefore fails one side unless both move together.

## 6. Tracker implementation

### 6.1 Domain types

In `crates/core/src/domain.rs` (added in batch 1; batch 2 removed the v3
payload types):

```rust
pub struct DatabaseSnapshot {
    pub target: TargetId,
    pub market: MarketMeta,
    pub tracker_version: String,
    pub generated_at: i64,
    pub auctions_at: Option<i64>,
    pub commodities_at: Option<i64>,
    pub entries: Vec<DatabaseEntry>,          // sorted by item, then category
}
// `DatabaseSnapshot::FEED_VERSION` is the contract version, 4.
pub struct MarketMeta {
    pub region: Option<String>,
    pub ruleset_id: Option<i64>,
    pub house_id: Option<i64>,
    pub connected_realm_id: Option<i64>,
    pub flavor: Flavor,
}
pub struct DatabaseEntry {
    pub item: ItemRef,                        // canonical grain (pets: level/quality/breed zeroed)
    pub category: Category,
    pub record: ItemRecord,
}
pub struct ItemRecord {
    pub spans: BTreeMap<Span, MetricBlock>,   // unchanged v3 block
    pub live: Option<LiveQuote>,
    pub value: Option<ValueQuote>,
    pub sold_count: Option<i64>,
    pub expired_count: Option<i64>,
    pub supply_days: Option<u16>,
    pub low30: Option<i64>,
    pub low_rank_30d_bp: Option<u16>,
    pub lows: Option<Vec<u8>>,
    pub vendor_price: Option<i64>,
    pub craft: Option<CraftQuote>,            // cost, profit, expected_profit
    pub deal: Option<DealMark>,               // rank, score_bp, discount_bp
}

// A group whose fields always travel together is one struct, so the encoder
// cannot be handed half of it. The JSON twin and the Lua record stay flat.
pub struct LiveQuote {
    pub listed: Option<LiveListing>,          // None: seen today, already gone
    pub seen: Option<SeenListing>,
}
pub struct LiveListing {
    pub floor: i64,
    pub floor_qty: i64,
    pub listed: i64,
    pub qty_sum: Option<i64>,
}
pub struct SeenListing {
    pub floor: i64,                           // seenFloor on the wire
    pub qty: i64,                             // seenQty on the wire
}
pub struct ValueQuote {
    pub value7: i64,                          // always above zero
    pub samples7: i64,
    pub sold_median14: Option<i64>,
    pub trend_bp: Option<i64>,
}
pub struct CraftQuote {
    pub cost: i64,
    pub profit: i64,
    pub expected_profit: i64,
}
pub struct DealMark {
    pub rank: u32,
    pub score_bp: u32,
    pub discount_bp: u32,
}
```

Batch 2 removed `FeedEntry` and `FeedSnapshot`. Kept under their names because
they are generic transport ports: `FeedFormat`, `FeedRenderer`, `FeedPublisher`,
`PublishedFeed`; `FeedRenderer::render` takes a `DatabaseSnapshot`. `CraftQuote`
and `DealMark` are `Copy`.

### 6.2 Codec modules

- `crates/infra/src/pack.rs` — `ALPHABET`, `MAX_INTEGER`, `put_varint`,
  `put_zigzag`, `zigzag`/`unzigzag`, `to_text`, `from_text`, a `Reader`
  (`varint`, `zigzag`, `uint`, `byte`, `flag`, `finish`) and `PackError`. The raw
  varint reader is exact over the whole `u64` range; the `2^53 − 1` ceiling is
  applied to the integer a field carries. `basis_points` and `saturate_u16`
  are defined in `crates/core/src/domain.rs` — the export service converts
  ranks and rates with the same definitions the codec writes — and
  re-exported here.
- `crates/infra/src/record.rs` — `encode(&ItemRecord) -> String` and
  `decode(&str) -> Result<ItemRecord, PackError>`. The decoder is the Rust
  reference used by tests; the game runs the Lua one.
- `crates/infra/src/feed.rs` — `FeedCodec::carrier` and `FeedCodec::json_twin`
  render a `DatabaseSnapshot`; `FeedCodec` implements `FeedRenderer` over them
  (`Lua` is the carrier, `Json` the twin) and `FileFeedPublisher` is unchanged.
  The v3 renderers are deleted.
- Both renderers refuse a snapshot that would publish a wrong file rather than
  publish it: an identity filed under the wrong table, a pet below species
  grain, a record without a span, or keys that do not ascend (in Lua a
  duplicate key would silently overwrite another record).
- Header strings come from configuration, so they are escaped into pure ASCII
  (`lua_string_literal`): no value can end its line, or the file, early.

### 6.3 Export service

`crates/core/src/services/database_export.rs` replaces `feed_export.rs`:
`DatabaseExportService { history, snapshots, metrics, opportunities, crafts,
clock, tracker_version }` with
`build(&TargetConfig, &ExportOptions { spans }) -> Result<DatabaseSnapshot, _>`.
The target configuration supplies the market header (`MarketMeta`: region,
ruleset, house or connected realm, flavor); the composition root passes the
tracker version it writes into every header.

Algorithm:

1. `generated_at` — as v3. `auctions_at` / `commodities_at` — the `fetched_at`
   of each payload kind's newest snapshot.
2. Per category: identities from `history.items_with_data` over the longest
   span window; group by feed grain (pets by species); span blocks through the
   cached bulk paths — all exactly as v3. A group without any span block is
   omitted.
3. Facts from the prepared scan through a read accessor on
   `OpportunityService`: `facts(target, category)` answers a `ScanFacts` view
   and its `get(&identity)` an `ItemFacts` — `value7`, `sold_median14`,
   `samples7`, `median_now` and `median_prev` (for the trend), today's
   sighting, sold/expired counts, sold units, the 30 daily lows (oldest day
   first) and the vendor price. An identity without a scan row has no facts
   and carries only what the snapshot knows. (The ledger recency is not
   exposed: the span blocks already carry `daysSinceLastSale` and
   `closuresSinceLastSale`.)
4. Live group from `snapshots.floors(target, kind, snapshot_id)` of the newest
   snapshot — **read directly, not from the scan**, so the min buyout never
   lags an ingest cycle. `seenFloor`/`seenQty` attach when today's fold floor
   undercuts the live floor (an equal floor is that listing, still up) and
   the newest snapshot lists anything at all — an empty snapshot proves
   nothing is gone, exactly as in the missed-deals list.
5. `low30` and `lowRank30d` computed with the ranking's own `low_rank`; the
   sparkline scales each low as `round(low × 128 ÷ reference)`, so the
   reference sits at mid-scale and anything above twice it reads as 255.
6. Crafts: `crafts.crafts(target).ranked`, first row per output, attached to
   the output's record in the category the craft was priced in.
7. Deals: `opportunities_in(&facts, category, 0.0, MAX, Score)` — the ranking
   over the very view the facts came from, so both describe one scan — the
   index in that order is the rank.
8. Attach groups per the rules of §5.6, sort, return.

Cost: span blocks and scan facts are cached; the only new store reads are the
two floor queries per publish. Measured on a 7,570-record market: about 11 s
for the first export of a market (every span block computed), about 2 s
afterwards.

Tests: the attachment rules over hand-built facts (`database_export.rs`);
store-level integration tests of every group, pet pooling, dual categories,
crafts and the lossless render (`crates/infra/tests/database_export.rs`); the
scheduler's ordering and the routes (`crates/server/tests/web.rs`).

### 6.4 Freshness and scheduler ordering

Before batch 2 `scheduler::run_pass` published the feed **before** refreshing
the prepared scans, and the manual `/api/fetch` path reuses `run_pass`. The
order since batch 2:

```
evaluate_alerts → run_prune_if_due → refresh scans (all categories)
                → publish feed (throttled)
```

Alerts stay first: they only need the cycle's own writes. A debounced scan
refresh may lag one scan TTL; that affects only slow-moving evidence, because
the live group comes from the snapshot directly.

### 6.5 Web routes

| Route | Change |
|---|---|
| `GET /feed.lua?target=` | Serves the v4 carrier. Unchanged path and content type. |
| `GET /api/feed.json?target=` | Serves the v4 JSON twin. |
| `GET /feed-slim.lua` | **Removed.** |
| `GET /api/addon/manifest` | The embedded addon bundle's manifest (§6.7). |
| `GET /api/addon/file/{path}` | One embedded addon file (§6.7). |

`ResponseCache::slim_feed` is removed, and with it the watchlist revision
counter that keyed the slim feed's cache (`WatchlistService::revision`).
`GET /api` links the addon manifest, the dashboard's feed section points at
it and at `addon-sync`, and the OpenAPI document, the `/api` directory and
the drift test cover every route: the file route is axum's wildcard
`{*path}` on the router and `{path}` in the document.

### 6.6 CLI

- `export-feed` loses `--slim`; it writes `feed.lua` and `feed.json` under
  `<out>/<target>/`.
- New `addon-sync` (§6.7): it needs no config and no store, only a running
  tracker and the game's addons folder.

### 6.7 Addon bundle and sync command

The server binary embeds the addon's static files with `include_str!` in
`crates/server/src/addon_bundle.rs` — the TOC and every file it loads, the six
slot placeholders included — so the served addon always matches the served
feed format.

- `GET /api/addon/manifest` —
  `{ "addon": "AuctiMoggerDatabase", "version": "0.1.0", "feedVersion": 4,
  "files": [ { "path", "sha256", "size" } ] }`. `version` is the addon's
  own version, read from its TOC (`## Version:`) and pinned by a test to the
  addon's `A.VERSION`; `feedVersion` is `DatabaseSnapshot::FEED_VERSION`. The
  files are listed in the table's order, the TOC first.
- `GET /api/addon/file/{path}` — one embedded file, verbatim: `text/x-lua`
  for a `.lua` file, `text/plain` for the TOC, `Cache-Control: no-cache` (the
  URLs carry no version, so every cache asks again). Any other path is 404:
  the lookup reads the embedded table and never the filesystem.
- A test fails when the embedded table and `addon/AuctiMoggerDatabase/` on
  disk disagree in either direction — a file on one side only, or bytes that
  differ — and when a file the TOC loads is missing from the table. A new
  addon file is added to the table in the change that creates it.

```
auctimogger-tracker addon-sync --url <base> --addons-dir <Interface/AddOns>
                              [--market <target>=<slot>]... [--no-addon]
                              [--no-markets] [--dry-run] [--timeout-secs N]
```

The use case is `crates/core/src/services/addon_sync.rs`, run over two
ports: the tracker over HTTP (`HttpTrackerAddon`, `crates/infra/src/addon_sync.rs`,
`ureq`) and the addon folder on disk (`AddonFolder`: a temporary file beside
the target, then a rename). Placement and verification are therefore
unit-tested without a network or a disk.

1. Write the addon's static files into `<addons-dir>/AuctiMoggerDatabase/`
   when their bytes differ from what the folder holds. The sync's client hands
   a download to the service only after it matched its manifest entry's
   SHA-256, and the service holds it to the entry's size besides.
   A slot file is the exception: it holds a market between syncs, so its
   placeholder is written only where no file exists — which is what
   completes a fresh install. The addons directory itself is never created:
   a mistyped path must not grow an AddOns tree of its own.
2. Markets: `--market` placements first; then every target reported by
   `GET /api/health`, in order, into the free slots 1–6. A target that finds
   no free slot is a failure of the run (choose with `--market`), never a
   silent skip. Slots no run claims are left as they are.
3. For each market fetch `feed.lua?target=` and verify before replacing,
   as the addon verifies at load (§5.2, §5.9): the banner line present,
   `version = 4`, `target` as expected and a tracker target id, a non-empty
   `trackerVersion`, `generatedAt` no more than five minutes ahead of the
   clock, and the closing `A.EndMarket()` (a truncated download). A carrier
   that fails leaves its slot exactly as it was.
4. Print a table (slot, target, bytes, age) and remind to `/reload`. Exit
   status 1 on any failure; whatever can still run after a failure runs.
   `--dry-run` reports what it would write and writes nothing;
   `--timeout-secs` (60) bounds each request. The manifest must name this
   addon and feed version 4, or nothing is done.

Tests: slot assignment, carrier verification and every path of the service
over in-memory ports (core); the wire decoders, the hash check, the folder,
and a carrier rendered by the real codec passing the verifier (infra); the
routes, the drift and the manifest read back by the sync's own client, and a
sync against a live loopback server (server).

The release workflow attaches `AuctiMoggerDatabase.zip` (the addon folder
with placeholder slots, zipped by its Linux leg, which waits for the addon
gates) for manual installs. The Docker build context keeps only the deployable
addon folder.

### 6.8 Deletions

- v3 renderers and their tests; `ExportOptions.watchlist_only`.
- `scripts/verify_feed.lua`; the `generated_lua_passes_the_contract_verifier`
  test; the CI step that installs lua5.4 for it.
- `WatchlistService`'s revision counter (it existed for the slim feed cache).
- `crates/infra/tests/feed_export.rs`, replaced by `database_export.rs`.

### 6.9 Documentation to update

- `README.md`: "Feed contract (v3)" becomes "(v4)" and points here;
  "Feed the addon" becomes "AuctiMogger Database"; API table rows for the feed
  and the two addon routes.
- The README's Forever example pairs `forever-eu-pvp-alliance` with
  `house_id = 113`, which is the Horde house (Alliance is 111): fix it.
- `crates/server/src/web/openapi.rs`.
- The README's CLI table row for `addon-sync` and its API table rows for the
  two addon routes landed with batch 6, with the routes.

## 7. Addon implementation

### 7.1 Folder and TOC

```
addon/
  SPEC.md                       this file
  .luacheckrc  .luarc.json  .stylua.toml  .styluaignore
  spec/                         busted specs
    helpers/                    loader (the client's `...` for specs) and encode
                                (a spec-side encoder; never shipped)
  scripts/                      check_toc, check_compat, run_tests,
                                assert_lls_clean, ci_local, bootstrap-tools
  tools/                        gitignored: bootstrap-tools puts StyLua and
                                LuaLS here
  AuctiMoggerDatabase/          deployable folder (symlink into Interface/AddOns)
    AuctiMoggerDatabase.toc
    bootstrap.lua               composition root, loads last
    Core/                       pure Lua: pack, record, market, settings,
                                format, resolve, tooltip_lines, anchor,
                                panel_rows
    Adapters/                   compat, market_files, extra_tip, tooltip,
                                options, public_api
    Market1.lua … Market6.lua   slots (tracked placeholders)
```

```toc
## Interface: 16001, 120100
## Title: AuctiMogger Database
## Notes: Auction house market database with detailed item tooltips.|n|cffff7d0a/amdb|r for options.
## Author: Riketta
## Version: 0.1.0
## Category: Auctions
## SavedVariables: AuctiMoggerDatabaseSettings
## SavedVariablesPerCharacter: AuctiMoggerDatabaseCharacter

# Core (pure Lua - no game API, CI-enforced)
Core/pack.lua
Core/record.lua
Core/market.lua
Core/settings.lua
Core/format.lua
Core/resolve.lua
Core/tooltip_lines.lua
Core/anchor.lua
Core/panel_rows.lua

# Market registration (must load before the slots)
Adapters/market_files.lua

# Market data slots (generated files land here - see addon/SPEC.md)
Market1.lua
Market2.lua
Market3.lua
Market4.lua
Market5.lua
Market6.lua

# Adapters
Adapters/compat.lua
Adapters/extra_tip.lua
Adapters/tooltip.lua
Adapters/options.lua
Adapters/public_api.lua

# Composition root (must be last)
bootstrap.lua
```

A slot placeholder is one line:
`-- AuctiMogger Database market slot N (empty). Copy a tracker-generated feed.lua over this file, then /reload.`

The block above is the complete TOC. A line is added by the batch that creates
its file, because `scripts/check_toc.lua` requires every listed file to exist:
batch 3 shipped it without the five tooltip and options lines, which batch 4
added with their files.

`addon/.styluaignore` exempts `AuctiMoggerDatabase/Market*.lua` from the
formatter: a carrier's record lines are longer than 120 columns by design, and
the tracker, not StyLua, owns their layout.

### 7.2 Module conventions

Carried over from the sniper's house style:

- Lua 5.1 only. Every module starts with `local _, A = ...` (or
  `local ADDON, A = ...` where it reads the addon name: an unread binding fails
  the unused-variable gates), registers into the namespace (`A.Core.Pack =
  Pack`) **and** returns its table. Nothing calls `require`: the client loads
  by TOC order. Specs load modules the way the client does, through
  `spec/helpers/loader.lua` (`loadfile(path)("AuctiMoggerDatabase",
  namespace)`), each from a fresh namespace, so a module that creates state at
  load time is re-created per spec.
- `Core/` references no game global; CI fails on any.
- EmmyLua classes with complete `@field` lists; `@param`/`@return` everywhere;
  no bare `table`/`any` where a shape is known.
- StyLua is canonical: spaces, indent 4, double quotes, column 120, LF.
- Every game API used is listed in `Adapters/compat.lua` and in the
  `.luacheckrc` / `.luarc.json` allowlists. luacheck (a real scope analysis,
  with an empty allowlist for `Core/`) and LuaLS enforce that code uses
  nothing else; `scripts/check_compat.lua` diffs the three lists against each
  other and scans the TOC-loaded files for unlisted events and `C_`
  namespaces. The global table is reached through `rawget` / `rawset`, never
  as `_G` by name.
- All persisted input is treated as untrusted: validated, dropped or clamped,
  never fatal at login.
- The clock is a seam: adapters read `A.clock()`, Core takes `now` as a
  parameter, and nothing else reads `time`.
- Comments and identifiers are plain ASCII. A user-facing string may carry the
  UTF-8 punctuation this spec shows (the em dash of §7.11).
- A Core module that needs another binds it under that module's own name
  (`local Settings = A.Core.Settings`): the compat guard reads a manifest name
  in `Core/` as the game global unless the file declares a local of that name
  first. The adapters' `Preferences` alias is for the files that could call
  the client's `Settings` API (§7.8).
- A dependency handed to an object that keeps its methods on the same table
  is never named like one of them: a dep named `status` once shadowed
  `Options:status()` and silenced `/amdb status`.
- A `---@class` block sits directly above the table it describes: the type
  checker binds it to the next definition, and a class separated from its
  table by constants or helpers has no fields.

### 7.3 Core modules

| Module | Responsibility |
|---|---|
| `Core/pack.lua` | `ALPHABET`, `MAX_INTEGER`; `fromText(text)` / `toText(bytes)`; `new(text)` gives a reader over one record: `varint(field)`, `zigzag(field)`, `uint(field, maximum)`, `byte(field)`, `flag(field, allowed)`, `finish()`. A reader raises on malformed input, with the Rust `PackError` text. |
| `Core/record.lua` | `Record.decode(text) -> record, nil` or `nil, message`; it never throws. Produces `AmdbRecord` (§7.3.1) with the derived methods of §5.8 and refuses exactly what the Rust decoder refuses. |
| `Core/market.lua` | `Market` (one loaded market) and `Store` (all markets): header validation, registration, lookups, primary binding, freshness tiers, change callbacks (§7.3.2). Pure: `now` is a parameter. |
| `Core/settings.lua` | `Settings.defaults()`, `coerce(saved)`, `coerceCharacter(saved)`, the per-line and switch setters, `applyPreset(settings, name)`. |
| `Core/format.lua` | `Format.money`, `moneySigned`, `percent`, `percentBp`, `signedPercentBp`, `count`, `rate`, `age`, `days`, `supplyDays`; `COLORS` (the §7.5.4 table as escape strings), `PREFIX`, `colorize(name, text)`; the tiers `ageTier(seconds)` (muted, warn, hot, stale at 2 h, 12 h, 24 h), `rateTier(fraction)` (good at 0.5 and up, warn at 0.2 and up, else bad) and `trendTier(bp)`; `ageColor(seconds)` is the house color of an age's tier, with `stale` in `bad` — the one table the login line, the tooltip header and `/amdb status` read. |
| `Core/resolve.lua` | `Resolve.item(value)`, `Resolve.species(value)` and `Resolve.resolve(value) -> kind, id` from a number, a hyperlink or an `item:` / `battlepet:` string. |
| `Core/tooltip_lines.lua` | `TooltipLines.build(record, context) -> AmdbTipRow[]`, the pure row builder of §7.6. A row is `left`, `right` (nil: a single line) and, for a sparkline, `bars`. The context carries the ages, the other markets' records, the collapse and Shift flags and the settings; the builder reads no clock. |
| `Core/anchor.lua` | `Anchor.side(ownerLeft, ownerRight, frameWidth, screenRight) -> "right" | "left"`, the pure side decision of §7.5.3; anything that is not four finite numbers answers the default side. `Anchor.point(side)` is the `SetPoint` arguments; `GAP` 8, `TOP_OFFSET` 10. |
| `Core/panel_rows.lua` | `PanelRows.build(context) -> AmdbPanelRow[]`, the row model of the options panel (§7.8): the master switch, a button per preset, one checkbox per optional line and Shift detail under a title, the primary-market picker under another, and the status line, each with its label and its hover help. `PanelRows.height(rows)` is the height the frame needs. Pure: the settings, the markets and the status line arrive in the context. |

#### 7.3.1 `AmdbRecord`

```lua
---@class AmdbRecord
---@field spans table<string, table>     -- d1/d7/d14/d30 -> the v3 span block
---@field live AmdbLive|nil              -- floor, floorQty, listed, qtySum, seenFloor, seenQty
---@field value7 integer|nil
---@field soldMedian14 integer|nil
---@field samples7 integer|nil
---@field trendBp integer|nil
---@field soldCount integer|nil
---@field expiredCount integer|nil
---@field supplyDays integer|nil         -- 65535 = unbounded
---@field low30 integer|nil
---@field lowRank30d integer|nil         -- bp
---@field lows integer[]|nil
---@field vendorPrice integer|nil
---@field craft AmdbCraft|nil            -- cost, profit, expectedProfit
---@field deal AmdbDeal|nil              -- rank, scoreBp, discountBp
```

An absent field is `nil`, never `false` or 0 (§5.7). The derived values of §5.8
are methods of this class: `reference()`, `valueSource()`, `saleRate14()`,
`saleRate180d()`, `daysSinceLastSale()`, `closuresSinceLastSale()`,
`soldPerDay()`, `atVendorFloor()` and `supplyUnbounded()`.

#### 7.3.2 `Store`

- `Store.new()`; `Store:begin(meta) -> builder, replaced` with `items`,
  `commodities`, `pets` (`replaced` is true when an unclosed builder was
  discarded); `Store:commit(now) -> market|nil, reports` validates and
  registers (§5.9). Every record is decoded once at commit: an undecodable
  entry is dropped, counted in `market.dropped`, and not kept. A rejected
  market is remembered: `Store:rejected()` returns `{ slot, target, reason }`
  entries.
- `Store:markets()` (slot order), `Store:get(target)`, `Store:primary()`,
  `Store:primaryTarget()`, `Store:explicitPrimary()`,
  `Store:primaryIsAmbiguous()`, `Store:setPrimary(target) -> ok, message` (the
  user action: it fails when the target is not loaded) and
  `Store:setExplicitPrimary(target|nil)` (the raw setter for a saved choice).
- Primary market resolution: the explicit per-character choice when that
  target is loaded; else the only loaded market; else the first market in slot
  order. With several markets and no choice the login status line says how to
  choose.
- A market replaced by a later slot keeps the later slot number and moves to
  the end of the slot order.
- `Store:record(itemID, category|nil, target|nil)` and
  `Store:petRecord(speciesID, target|nil)` decode on each call — about 30 µs,
  so there is no cache to invalidate. Callers get fresh tables and may keep
  them.
- `Store:onUpdate(callback) -> unsubscribe()`; `Store:fireUpdate()` runs the
  callbacks and returns the errors they raised (one failing callback is
  contained and the rest still run). Fired once after login, whenever the set
  of markets changes, and when `setPrimary` changes the effective primary.
- `Market:ageSeconds(now)` uses `generatedAt`;
  `Market:snapshotAge(kind, now)` uses `auctionsAt` / `commoditiesAt`;
  `Market:isStale(now)` is `age > 24 h`.
- `Store:stats(now)` — per market: slot, target, record counts (total and per
  table), age, stale, dropped records. The `Market` itself carries `slot`,
  `target`, the validated `meta`, `generatedAt` and `dropped`.

### 7.4 Adapters

**`Adapters/market_files.lua`** — creates the `Store` at load time (`A.store`)
and publishes `A.BeginMarket(meta)` / `A.EndMarket()`, the only functions a
carrier calls. `BeginMarket` replaces an unclosed builder (malformed file) and
reports it; `EndMarket` commits under `pcall`, so a store error is reported
and never fails the slot file. The module owns the seams every other adapter
reads: the clock (`A.clock()`, the only reader of `time`), the chat sink
(`A.chat`) and the report sink. Reports go through `A.Say(text, level)` with
level `info`, `warn` or `bad`: each is recorded in `A.reports` and printed at
once only when `A.verbose` is set; otherwise the bootstrap prints them once at
login through `A.TakeReports()`. `A.FormatReport(text, level)` is the one
place a report becomes a chat line. `A.SayNow(text, level)` is `A.Say` plus
that chat line at once: for something that goes wrong after login, when
nothing drains the record again and a quiet mode must not swallow it.

**`Adapters/compat.lua`** — the manifest of every game surface the addon
touches, in tiers, plus `check(env|nil) -> { ok, missing, absentOptional }`
for `/amdb status` (it reads the global table through `rawget` and never
throws). The module publishes the tiers as `A.Compat.requiredGlobals`,
`requiredEvents`, `optionalGlobals`, `optionalEvents`, `authored`, the same
five arrays under `A.Compat.manifest` (what `scripts/check_compat.lua` reads)
and a `fallbacks` table with one sentence per optional entry:

| Tier | Surface |
|---|---|
| Required globals | `CreateFrame`, `GameTooltip`, `UIParent`, `time`, `SlashCmdList` |
| Required events | `ADDON_LOADED`, `PLAYER_LOGIN` |
| Optional globals | `TooltipDataProcessor`, `Enum`, `ItemRefTooltip`, `issecretvalue`, `IsShiftKeyDown`, `C_Timer`, `Settings`, `MenuUtil`, `print` |
| Optional events | `MODIFIER_STATE_CHANGED`, `PLAYER_LOGOUT` |
| Authored | `SLASH_AMDB1`, `SLASH_AMDB2`, the namespace table `AuctiMoggerDatabase`, the attached frame `AuctiMoggerDatabaseTip` (a named frame: the client creates its global, and those of its font strings) |

No `C_AuctionHouse` function appears anywhere, not even in a string.

**`Adapters/extra_tip.lua`** — the attached frame, `A.ExtraTip.new()`.

- One `GameTooltip` created with `CreateFrame("GameTooltip",
  "AuctiMoggerDatabaseTip", UIParent, "GameTooltipTemplate")`, stratum
  `TOOLTIP`, `SetClampedToScreen(true)`, owner `UIParent`, anchor `ANCHOR_NONE`.
- `ExtraTip.new()`, `:show(owner, rows)`, `:hide()`, `:isShown()`. Every
  show starts by hiding, then clears the lines: nothing of the previous
  tooltip lingers.
- Rows render through the native `AddLine` / `AddDoubleLine`. A sparkline row
  is a caption line plus one `Texture` a day (at most 30, 3 px wide, 4 px
  apart, 1 to 10 px tall by the day's byte), created on demand and anchored
  to the row's own font string (`<frame>TextLeft<n>`), so the bars follow the
  row wherever the client lays it out. A client that cannot draw them skips
  them; nothing raises for decoration.
- Every layout read is pcall'd and `issecretvalue`-screened. A number that
  cannot be read is missing, and a missing number leaves the frame on its
  default side.
- Positioning (§7.5.3) happens inside `show`, which the tooltip adapter calls
  one frame after the fill, once the owner has its final size.

**`Adapters/tooltip.lua`** — hooks, lifecycle, Shift, re-entrancy guard
(§7.5). `Tooltip.new({ store, settings, say })` creates the attached frame
and hooks nothing; `register()` installs the hooks once and answers the path
in use (`processor`, `script` or `none`, kept in `path` for `/amdb status`).
`onTooltip(owner, data)` is the hook entry, `onModifier(key)` the Shift
rebuild, `setEnabled` / `isEnabled` the master switch, `hide` / `isShown` the
frame's state.

**`Adapters/options.lua`** — the `/amdb` slash command (§7.7) and the options
panel (§7.8). `Options.new({ store, settings, character, tooltip, say,
persist, statusLine })`, `register()`, `run(msg)`, and for the panel
`registerPanel()` and `refreshPanel()`. Every change of either goes through
one `apply` method (`applyEnabled`, `applyLine`, `applyPreset`, `applyShift`,
`applyMarket`) that changes the setting, writes it back and refreshes the
panel; the command then answers in chat, and the panel says nothing.

**`Adapters/public_api.lua`** — publishes the namespace table as the global `AuctiMoggerDatabase`, and `GetAuctionBuyout` as a global function of its own (§7.9).

**`bootstrap.lua`** — the composition root; owns one frame and the
SavedVariables lifecycle.

- `ADDON_LOADED` (its own addon) → `loadSettings`: the two saved tables are
  coerced (§7.7) into the live `A.settings` and `A.character`, written back
  in their clean shape, and this character's chosen primary market is
  applied. `A.PersistSettings()` writes both back; it runs after every change
  and again at `PLAYER_LOGOUT`.
- `PLAYER_LOGIN` → the tooltip adapter, the options and the options panel
  are built once, each under `pcall`: what cannot start is reported in bad
  and left out, and the login goes on. A client without the `Settings` API
  gets the one-time note of §7.8 instead. Then the status line of §7.11 (a
  pure function of the store and `now`, handed to the panel as well), the
  recorded reports, and `fireUpdate()` with each callback error reported
  once in bad. A login that no `ADDON_LOADED` preceded (a client whose event
  was refused) loads the settings itself.
- `MODIFIER_STATE_CHANGED` → the tooltip adapter's `onModifier`.
- The optional events are registered under `pcall`: a client that refuses one
  loses the feature it carries and keeps the login.

### 7.5 Tooltip design

#### 7.5.1 Hook registration

- Preferred: `TooltipDataProcessor.AddTooltipPostCall(Enum.TooltipDataType.Item, fn)`
  for the `GameTooltip` and `ItemRefTooltip` globals only. Comparison
  tooltips are skipped, like the sniper's tooltip did.
- Fallback when the processor is absent: `HookScript("OnTooltipSetItem")` on
  the same two frames. Never both.
- Battle pets: where `Enum.TooltipDataType.BattlePet` exists, the same
  post-call resolves the species from `data.id`. Nil-guarded, optional.
- Identity: the processor payload's `id`, else a pcall'd `GetItem()` link parse
  with every read `issecretvalue`-screened.
- Only the `GameTooltip` and `ItemRefTooltip` globals are attached to, both
  read at call time. The record is looked up in the primary market, items
  before commodities (§5.3); the other markets are read in that same pool.
- The pet post-call is a second registration of the same entry point. Where
  `BattlePet` is missing, only that registration is left out; a processor
  that raises on the item type falls through to the script path.

#### 7.5.2 Lifecycle

1. Post-call → resolve identity → `Store:record()`. No record: hide the frame.
   The master switch off, a frame that is not an owner, an identity that
   cannot be read: the frame is hidden all the same.
2. Build rows with `Core/tooltip_lines.lua`; schedule display on the next
   frame (`C_Timer.After(0, …)`; at once where the client has no timer).
3. On display: if the owner is no longer shown, stay hidden. Otherwise position
   and show. Only the latest request is ever displayed: each carries a
   sequence number, and a display scheduled before it does nothing.
4. Hide on the owner's `OnTooltipCleared` and `OnHide` (instance hooks
   installed once at login) and before every re-show.
5. `MODIFIER_STATE_CHANGED` for Shift rebuilds a shown frame at the new detail
   level, on the next frame like any display. The owner is never refreshed, so
   no re-entrancy is possible.
6. Rendering is pcall-wrapped behind a busy guard; the first error is reported
   once, and a render that raised takes the frame down.

Taint: the frame is ours and unprotected; every write is cosmetic. The owner is
only read. Nothing is combat-gated.

#### 7.5.3 Anchoring

`Core/anchor.lua` decides the side from four numbers in one coordinate space:
the owner's left and right edges, the frame's width and the screen's right
edge. The adapter converts with `GetEffectiveScale()`.

- Default: frame's top-left at the owner's top-right, 8 px gap.
- If that would cross the screen's right edge: mirror to the owner's left.
- Vertically the frame's top aligns with the owner's top minus 10 px.

`Anchor.point` turns the side into those `SetPoint` arguments:
`TOPLEFT` / `TOPRIGHT` / `+8` / `-10` on the right, `TOPRIGHT` / `TOPLEFT` /
`-8` / `-10` on the left. The screen's right edge is `UIParent`'s. A number
that cannot be read leaves the frame on the default side; the frame is
clamped to the screen either way.

#### 7.5.4 Colors

| Name | RGB | Used for |
|---|---|---|
| brand | `#ff7d0a` | Header title |
| muted | `0.60, 0.63, 0.70` | Labels, secondary text |
| dim | `0.40, 0.43, 0.50` | Hints, footer |
| good | `0.24, 0.78, 0.45` | High sale rate, gains, deal badge |
| warn | `1.00, 0.82, 0.00` | Medium sale rate, 2–12 h age |
| hot | `1.00, 0.50, 0.25` | 12–24 h age |
| bad | `1.00, 0.25, 0.25` | Low sale rate, losses, stale |

Age badge of the record's own snapshot (auctions or commodities): under 2 h
muted, 2–12 h warn, 12–24 h hot, over 24 h `stale` in bad. In the tooltip
header the badge is that snapshot's age — the commodities stamp for a
commodity, the auctions stamp for everything else, pets included — and the
market's own `generatedAt` age when the header lacks the stamp. Labels are
muted, hints and the footer dim.

### 7.6 Tooltip lines

Standard level, every optional line on:

```
 AuctiMogger Database                    forever-eu-pvp-alliance · 58m
 Min buyout                 380g 50s  ×214
 Market value               402g 10s  sold  -3%
 Sale rate (14d)            36%  (34 of 94 sold)
 Sold                       31.6/day · supply 39d
 Last sale                  2d ago
 30-day low                 350g  · 80% of days higher
 30 days                    [ sparkline: 30 bars, lowest daily ask ]
 Crafting                   2100g → +1900g  (exp. +684g)
 Deal                       #12 · 38% under value · score 0.71
 forever-eu-pvp-horde       442g 31s  +10%
 forever-eu-pvp-blackwater  382g  -5%
```

Both samples of this section are the record of §5.10 — completed where its
JSON writes `...` with a fortnight of 443 units sold and two more markets —
25 minutes after its auctions stamp and 58 after its commodities stamp. A
spec pins each line for line.

| Line | Shown when | Text |
|---|---|---|
| Header | always | Title in brand; right: `target · age`, the age in its tier color (§7.5.4). |
| **Min buyout** | always | `money(floor)` + dim `  ×floorQty`; good `  ≤ vendor` when `atVendorFloor()`. Not listed but seen today: dim `none listed · seen today money(seenFloor) ×seenQty`. Otherwise dim `not listed`. |
| **Market value** | always | `money(reference)` + dim source word (`sold` / `listed` / span name) + signed `trendBp` percent in good or bad (muted at zero) when present; dim `unknown` without a value. |
| **Sale rate (14d)** | always | Percent in the rate color + dim `  (soldCount of soldCount + expiredCount sold)` when both counts exist. Without a 14-day rate the label reads `Sale rate (180d)` and the counts are left out: they describe the fortnight. Dim `no evidence` without any rate. |
| Sold | `sold` on, `soldPerDay()` known | `x.x/day`, then ` · supply Nd` when `supplyDays` is known, or ` · supply ∞` when it is saturated or unbounded. |
| Last sale | `lastSale` on, `daysSinceLastSale()` known | `today`, `1d ago`, `Nd ago`. |
| 30-day low | `low` on, `low30` known | `money(low30)` + dim `  · N% of days higher` from `lowRank30d`. |
| Sparkline | `sparkline` on, `lows` present | Caption `30 days`; bars from `lows`. |
| Crafting | `craft` on, `craft` present | `money(cost) → ±money(profit)` + dim `(exp. ±money(expectedProfit))`; losses in bad. |
| Deal | `deal` on, `deal` present | good `#rank · N% under value · score x.xx`. |
| Other markets | `markets` on | One line per other loaded market that has the item in the same pool: its target id as a muted label; `money(reference)` and, against the primary's reference, a signed percent. |

Shift-held detail replaces the optional lines with:

```
 Sold median (14d)          402g 10s
 Value basis                7-day listing value  · 214 listings
 Listed                     618 listings  · 1,234 units
 Vendor sell                12g 50s
 Supply                     39 days  (1,234 listed ÷ 31.6/day)
 Sale rate (180d)           31%
 Closures since last sale   61
 Snapshots                  auctions 25m · commodities 58m ago
 d1   median 401g           p05 380g · p95 420g · 58 seen · 9 sold
 d7   median 431g           p05 380g · p95 512g · 214 seen · 210 sold
 d14  median 431g           p05 385g · p95 512g · 300 seen · 443 sold
 194784 · commodity · feed v4
```

Each line is omitted when its data is absent. `Supply` explains the tracker's
division (§5.6, listed quantity ÷ the daily pace) with the two numbers it
divided, and reads `at least 65535 days` for the saturated figure. `Sale rate
(180d)` is left out when the basic line already shows that rate. `Snapshots`
names each stamp the header carries: `auctions 25m ago` when it is the only
one. The span rows follow wire order, with `· N sold` only where `qtySold` is
known. The footer names the item id ungrouped (a player pastes it into a
lookup), the category and the carrier's version. A collapsed owner tooltip
(`IsTooltipCollapsed()`) always renders the three basics only, whatever is
held or switched on.

### 7.7 Settings and slash command

Saved in `AuctiMoggerDatabaseSettings` (account):

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Master switch. |
| `lines` | absent | Optional line overrides; absent key = on. Keys: `sold`, `lastSale`, `low`, `sparkline`, `craft`, `deal`, `markets`. |
| `shiftDetail` | `true` | Shift shows the detail level. |

`AuctiMoggerDatabaseCharacter` (per character): `primaryMarket` (string, the
chosen target id).

Coercion is strict: only literal `true` / `false` count; anything else is the
default. The three basics have no switch.

The bootstrap loads the two tables into `A.settings` and `A.character` and
writes both back after every change and again at the logout: nothing a player
chose waits for the logout to be saved.

Every change the command makes goes through an `apply` method that the
options panel shares (§7.8), so the two cannot disagree.

`/amdb` (alias `/auctimoggerdb`):

| Command | Effect |
|---|---|
| `on` / `off` | Master switch. |
| `status` | Markets (slot, target, records, age, stale, dropped), primary market, switch, preset. |
| `market` / `market <target>` | List markets / choose this character's primary market. |
| `lines` | List optional lines with their state. |
| `basic` / `standard` | Presets: optional lines all off / all on. |
| `<line> on` / `<line> off` | Toggle one optional line. |
| `shift on` / `shift off` | Shift detail. |
| `decode <item>` | Debug: print every decoded field for the item. |
| `help` | The list above. |

`/amdb` alone and `help` print the list; an unknown command says so, in warn,
and prints it. Answers reach chat at once: data rows plain, a reply behind the
addon title. A command that raises says so instead of showing a Lua error.

`/amdb status` opens like the login line (`AuctiMogger Database: on · preset
standard · shift detail on · tooltip hooks: processor`), prints one row per
market (slot, target, records by table, age, `primary`, `stale`, dropped
records), then every market a slot file could not register, the choice to make
when the primary is ambiguous or names an unloaded market, each client surface
the manifest lists and the addon lacks (a missing required one in bad, each
absent optional one with its fallback sentence), and the reports recorded
since the login line. The preset reads `basic`, `standard` or `custom`.
`decode <item>` takes an id, a link or an `item:` / `battlepet:` string and
prints the raw numbers, an absent field as `-`.

### 7.8 Options panel

Interface → AddOns → AuctiMogger Database. A canvas category of our own,
registered at the login with `Settings.RegisterCanvasLayoutCategory(frame,
"AuctiMogger Database")` and `Settings.RegisterAddOnCategory(category)`: the
two calls are all this addon needs from that API, and they exist unchanged
since the API appeared. Where the global is missing, or cannot register a
canvas, the panel is skipped with one muted note at the login — `no options
panel on this client — the /amdb command covers everything` — and the slash
command covers everything. A panel that raises while it is built (a template
this client lacks) is reported in bad and left out: nothing half-built reaches
the client, and the command stays.

**The frame.** `CreateFrame("Frame", nil, UIParent)`, 600 wide and as tall as
its rows, hidden until the Settings UI parents it to its canvas. Only its
top-left corner is anchored (`AddAnchorPoint("TOPLEFT", 16, -16)`), so the
height is the frame's own. It is built once, and registered only once every
control exists.

**The rows** come from `Core/panel_rows.lua` (§7.3), in this order, and the
adapter builds one region per row:

| Row | Control |
|---|---|
| Enable tooltips | A checkbox: the master switch, read as the tooltip adapter reads it (on unless literally off). |
| Preset | A button per preset of the settings module: `Basic`, `Standard`. |
| Tooltip lines | A title, in the brand color. |
| One checkbox per optional line | In the settings module's order and state; the label and the hover help of each are in the row model. |
| Hold Shift for detail | A checkbox: off unless literally on. |
| Market | A title, in the brand color. |
| Primary market | A picker button naming the primary market; see below. |
| The status line | The login line of §7.11, as the clock has it, wrapped within 560 pixels; unwrapped on a client that cannot wrap. |

Every control is a frame this file creates, on a template the sniper runs on
Forever as well: `UICheckButtonTemplate` for a checkbox (its label a font
string to the right of the box, inside the box's hit rectangle, so the label
takes the click too) and `UIPanelButtonTemplate` for a button. A checkbox's
template toggles nothing by itself: the new state is read before the box is
set. Each control that has help shows it on hover in the client's
`GameTooltip`, at the cursor (`ANCHOR_CURSOR`): the label in the brand color,
one line of body text. Help is decoration: a client without a usable tooltip
gets none and raises nothing.

**The primary-market picker.** Where the client has `MenuUtil`, a click opens
its own menu with one radio entry per loaded market, read from the store at
every opening; the chosen one goes through `applyMarket`, the command's own
path. Without a usable menu (the global absent, or it raises on opening) a
click moves to the next market in slot order, round to the first: the
sniper's pattern. With no market loaded the picker reads `none loaded` and is
disabled.

**The refresh.** A shown frame refreshes itself on its own `OnUpdate`, at
most once a second, and every `apply` refreshes it at once, so a change made
through `/amdb` while the panel is open shows without a reload and the status
line's age stays current. A refresh updates the controls in place; a row of
another kind at an index takes the old controls down (a frame cannot be
destroyed) and builds its own. The first error takes the panel out of service
and is reported once in bad, so a broken panel never breaks a command.

**Nothing runs on the open or close path.** The frame has no `OnShow`,
`OnHide`, `OnRefresh`, `OnCommit` or `OnDefault`, although the Settings API
documents `OnRefresh` for a canvas: Mastercraft ships its canvas the same way
on Forever, where addon code on that path got Log Out and Exit blocked. A
hidden frame is left alone, since a hidden frame's `OnUpdate` does not run.

Taint: the frame and its controls are ours and unprotected; the Settings UI
only shows, hides and reparents the frame.

The adapter binds `Core/settings.lua` under another name (`Preferences`): a
local named `Settings` would shadow the client's global in the one file that
calls it.

### 7.9 Public API

The addon's namespace table, published as the global `AuctiMoggerDatabase`:

> The shorter `AuctiMoggerDB` is deliberately not used: the sniper declares it
> as a SavedVariables name, and the client overwrites that global when the
> sniper loads.

| Function | Result |
|---|---|
| `VERSION`, `FEED_VERSION` | `"0.1.0"`, `4` |
| `GetMarketInfo(item, opts)` | An `AmdbMarketInfo` table, or `nil`. `opts.category` and `opts.market` are optional. |
| `GetSpans(item, category)` | The v3-shaped span table `{ d1 = {…}, … }`, or `nil` when unknown or stale. |
| `GetMarkets()` | Array of `{ target, slot, records, generatedAt, ageSeconds, stale }`. |
| `GetPrimaryMarket()` | The primary target id, or `nil`. |
| `OnUpdate(callback)` | Runs `callback()` once at login and on market changes; returns an unsubscribe function. |
| `GetAuctionBuyout(item)` | The reference value in copper (Tekkub's convention). Chains to a previously defined `GetAuctionBuyout` when unknown. |

`item` is an item id, an item link or an `item:` string; pets take a
`battlepet:` link or a species id through `GetMarketInfo`'s `opts.species`; for
a pet `itemID` is the species id and `category` is `pet`.
`snapshotAgeSeconds` is the age of the record's own snapshot (auctions;
commodities for a commodity), nil when the header lacks that stamp.

`AmdbMarketInfo` is flat, camelCase and fresh per call: `itemID`, `category`,
`market`, `generatedAt`, `ageSeconds`, `snapshotAgeSeconds`, `stale`,
`floorNow`, `floorQty`, `listedNow`, `seenFloor`, `value`, `valueSource`,
`trend`, `soldMedian14`, `saleRate`, `saleRate180d`, `soldCount`,
`expiredCount`, `soldPerDay`, `supplyDays`, `daysSinceLastSale`,
`closuresSinceLastSale`, `low30`, `lowRank30d`, `vendorPrice`, `craft`, `deal`,
`spans`. Fractions are plain numbers; absent fields are `nil`. `craft`, `deal`
and `spans` are fresh copies. Only `GetSpans` goes silent on a stale market:
`GetMarketInfo` still answers, with `stale` set.

### 7.10 SavedVariables

Only settings are persisted. No market data ever reaches SavedVariables.

### 7.11 Login status line

- `AuctiMogger Database: 2 markets · primary forever-eu-pvp-alliance · data 25m old`
- No data: `AuctiMogger Database: no market data — copy the tracker's feed.lua over Market1.lua` (bad).
- Several markets, no choice: append `· several markets loaded, pick one: /amdb market <id>`.
- Stale market: `· forever-eu-pvp-horde is 2d old — resync` (hot).

The count reads `1 market` or `N markets`. The age is the primary's, colored by
its age tier (§7.5.4). The resync suffix is appended once per stale market
other than the primary, whose age the line already shows. The reports recorded
while the slot files loaded follow on their own lines, each in the color of its
level, and the update callbacks run after them.

## 8. Sniper migration

Repository `Riketta/AuctiMogger`. The sniper keeps its live tail, its deals
ring and its seen marks. Everything that stored tracker data moves out.

**Batch 7 — read the database** (done in `Riketta/AuctiMogger`)

- TOC: `## OptionalDeps: AuctiMoggerDatabase`.
- `ports/outbound.lua`: the read-only `MarketDatabasePort` with
  `entry(keyString) -> { itemID, spans, generatedAt } | nil` and
  `describe() -> { loaded, market, ageSeconds }`. `MarketDataPort` lost
  `beginSnapshot`, `commitSnapshot` and `upsertMetrics`: the store's one
  write path is the live tail.
- `adapters/outbound/market_database.lua`: the only reader of the
  `AuctiMoggerDatabase` global. One `GetMarketInfo` per key string, cached
  (an unknown key is remembered too) and dropped on every `OnUpdate`. A pet
  key reads its species (`opts.species`); any other key reads its item id,
  so a variant pin reads the same record as the base key. Each call runs
  under pcall: a lookup that raises costs that lookup and is reported once
  per session. An addon present but not answering the four functions the
  adapter calls is treated as absent and said so once at login; an addon
  that loads late is bound by the first lookup.
- `adapters/outbound/history_db.lua`: the `market` map and its three write
  methods are gone; `trackerEvidence` reads the port. The valuation tiers
  and the 24-hour ceiling are unchanged, and they stay the reader's: the
  adapter applies no staleness rule of its own, and an old entry still shows
  its spans on the display paths while the valuation ignores it. A future
  stamp is no evidence for the sniper, even inside the database's
  five-minute tolerance. `invalidate()` drops the valuation memos; the
  composition root wires it to the adapter's `onInvalidate`.
- Deleted `domain/rest_ingestor.lua`, `adapters/inbound/tracker_feed.lua`,
  the `AuctiMoggerTracker/` folder and their specs. CI packages the one
  addon folder that remains and checks its TOC alone.
- `bootstrap.lua`: the ingestor, the feed adapter, `ingestAndLog`,
  `IngestTrackerFeed` and the re-ingest on list change are gone. So are the
  `/am ingest` command, which had nothing left to call, and the console
  adapter's never-wired `onSnipeListChanged` hook, which existed for the
  re-ingest.
- The sniper's own SavedVariables global `AuctiMoggerDB`: schema version 3.
  A version 2 blob migrates in place — `market`, `snapshots` and
  `counters.restKeysIngested` drop; the deals, resets, seen and observed
  marks and the live tail are kept. Any other version is wiped, as before.
- `status().market` became `status().database = { loaded, market, ageSeconds }`
  (the core's optional `database` dependency); the live-tail counters it
  carried moved to `status().live`. `/am status` and the Session tab name
  the primary market and its age, or say the database is not loaded.
- The sniper's SPEC.md, AGENTS.md (rule 2: one write path) and README are
  updated.

**Batch 8 — remove the sniper's tooltip**

- Delete `ui/item_tooltip.lua` and its spec; drop the Session-tab "Item tips"
  button, the `/am tooltip` command and the `AuctiMoggerUI.itemTooltip` key
  (pruned on load).
- Remove the tooltip's manifest and allowlist entries. (CI already packages
  only `AuctiMogger`, and `/am ingest` left with batch 7.)

## 9. Quality gates

**Tracker (Rust)** — unchanged: nightly `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace`, `cargo deny check`. The golden-fixture test joins
them.

**Addon (Lua)** — every gate scoped to `addon/`, because the tree also holds
Lua these gates must not judge: foreign copies and probes in the gitignored
`target/` (`scripts/verify_feed.lua`, the one tracked Lua file outside the
addon, was deleted in batch 2):

```sh
stylua --check addon
(cd addon && luacheck .)   # luacheck reads .luacheckrc only from the current directory
(cd addon && busted --verbose)
lua addon/scripts/check_toc.lua addon/AuctiMoggerDatabase/AuctiMoggerDatabase.toc
lua addon/scripts/check_compat.lua
lua-language-server --check=addon --checklevel=Warning > ./addon/lls.log 2>&1
lua addon/scripts/assert_lls_clean.lua ./addon/lls.log
```

- Pins: Lua 5.1, luacheck 1.2.0, busted 2.2.0, StyLua 2.6.0,
  lua-language-server 3.19.1 — the same as the sniper repository.
- `addon/scripts/bootstrap-tools.sh` downloads StyLua and LuaLS into the
  gitignored `tools/`; `addon/scripts/ci_local.sh` runs everything, with the
  Unix gates in WSL.
- `addon/.styluaignore` exempts the generated `Market*.lua` slots: their record
  lines are longer than 120 columns by design. Their luacheck rule lifts the
  line-length limit for the same reason.
- `.luarc.json` ignores the tool tree with `workspace.ignoreDir` (3.19.1 has
  no `ignore` key); the `.gitignore` line `/addon/tools/` covers the checker
  as well.
- `addon/scripts/run_tests.lua` is a plain-Lua harness with the busted API, for
  a machine without rocks. CI uses the real tools.
- CI: a new `addon` job on `ubuntu-latest` with the same pins; the `docker`
  job then needs `linux` **and** `addon`.
- `scripts/check_compat.lua` keeps `NON_EVENT_LITERALS`, the upper-case
  constants that are not events (`ANCHOR_NONE`, `ANCHOR_CURSOR`, `SLASH_AMDB1`,
  `SLASH_AMDB2`): a new one is listed there with its reason, never in the
  event tiers.

## 10. Test plan

**Rust**

- `pack.rs`: varint and zigzag round-trips at 0, 127, 128, 2^32 and the
  maximum; the documented byte layout; text vectors pinned against the Lua
  prototype; alphabet membership; padding; non-minimal and over-long varints.
- `record.rs`: encode → decode round-trip over 2^17 flag combinations and over
  every span set with every span flag; both signs, both saturations, a present
  zero; the wire layout written field by field. Malformed inputs are errors,
  never panics: every proper prefix of a record, every one-symbol mutation of
  a full record, and every text of up to three symbols (266 304 of them).
- `feed.rs`: the documented carrier text, table and key order, ASCII escaping,
  every refusal, and the twin's nulls, omissions and key order.
- Golden fixtures byte-stable (§5.11).
- `DatabaseExportService`: group attachment rules, pet pooling, rank and
  craft selection, entries without spans omitted, deterministic order.
- Scheduler: scans refresh before the feed publishes.
- Addon bundle: the embedded table matches the folder in both directions,
  byte for byte; every file the TOC loads is embedded; the TOC's version is
  the addon's `VERSION`; the manifest carries each file's hash and size, and
  the sync's own client parses it back.
- `addon-sync`: slot assignment (explicit placements first, the tracker's
  order, a repeated target, conflicting claims, no free slot); carrier
  verification (every refusal of §6.7, and the renderer's string escapes read
  back); the service over in-memory ports (a fresh install, an unchanged
  run, a changed and a failed file, a filled slot, markets, a dry run, the
  switches, a manifest of another feed version); the wire decoders, the
  download hash, the folder; a carrier rendered by the real codec through
  the verifier; the routes; and one sync against a live loopback router.
- Dashboard: the feed section links the addon manifest.

**Lua**

- `pack`, `record`: every field, every absence, malformed and truncated
  records, wrong padding.
- Golden trio (§5.11).
- `market`: version gate, future rejection, stale flag, duplicate target,
  slot order, primary binding, default category, update callbacks.
- `settings`: defaults, strict coercion both ways, presets.
- `panel_rows`: the row order of §7.8; every optional line and every preset of
  the settings module has its row, named and explained; the switch and Shift
  read as the tooltip reads them; the market row and the status line; the
  height.
- `format`, `resolve`, `anchor`: table tests.
- `tooltip_lines`: one test per line for shown and omitted, Shift detail,
  collapsed owner, each toggle; both samples of §7.6 line for line.
- `extra_tip`: creation, the rows, placement in pixels, secret and unreadable
  layout, the sparkline.
- `market_files`: the golden carrier loaded through a fake `...` and decoded by
  the real decoder; malformed header; unclosed builder; dropped records; the
  report sink; the clock.
- `compat`: the manifest tiers, a fallback per optional entry, `check()`.
- `bootstrap`: the status line, the login flow and the SavedVariables lifecycle
  through a stubbed frame, and the panel's three outcomes at the login.
- `tooltip`: registration on both paths (never both), lifecycle (hidden when the
  owner is not shown, hidden on clear), Shift rebuild, secret values, busy
  guard, errors reported once.
- `options`: every command of §7.7, the write-back after each change, and
  what `status` says without a feed.
- `panel` (a spec of its own): the registration against a recording Settings
  API, and the three ways it is left out; one control per row; what a click
  does; the refresh in place and the tick; a replaced row; the picker's menu
  and its click-cycle; the hover help; a refresh that fails.
- `public_api`: fresh tables per call, `GetAuctionBuyout` chaining, absent
  addon data.

**Sniper (batch 7, in its own repository)**

- `market_database`: inert without the addon; item, variant and pet keys;
  one lookup per key string, unknown keys included; the cache drops on
  `OnUpdate`; `describe()`; a raising API costs one lookup and is reported
  once; a malformed answer; an addon that does not answer the API; a late
  load; a key that names no item.
- `history_db` (v3): every tracker-tier case through a fake
  `MarketDatabasePort` — fresh, degraded, past 12 h, past 24 h, d7-only, the
  slow-window sale rate, variant and pet keys, the rollup blend, a key never
  observed — plus `invalidate()`, the v2 migration, the reset of any other
  version and `stats`.
- `core`, `gui`, `adapters`: `status().live` and `status().database`, the
  Session-tab line both ways, `/am status`.
- A cross-repository check run for batch 7 and not committed: the real addon
  loaded from `fixtures/database_v4.lua`, read through the real sniper
  adapter and history store — every record equal to the oracle, the
  valuations and the evidence ceiling on real numbers.

**In game** (owner, on Forever, `/console scriptErrors 1`)

1. Install the addon folder; copy one carrier over `Market1.lua`; add two more
   markets; `/reload`. Check the status line.
2. Hover: a bag item, a chat link, an auction-house browse row, a commodity
   row, an item without data, a pet cage, a battle-pet link.
3. Press and release Shift while hovering. Run `/amdb basic`, `/amdb standard`
   and each line toggle. `/amdb off`, `/amdb on`.
4. `/amdb market <id>`: cross-market lines follow.
5. Hover a bag item during combat: no blocked-action errors.
6. Narrow the window so the frame flips to the left side.
7. Interface → AddOns → AuctiMogger Database: hover a control for its help;
   toggle a line, a preset, the master switch, Shift detail and the market;
   make the same changes through `/amdb` with the window closed and reopen
   it; then close the window with Esc and confirm Log Out and Exit still work.
8. Sniper: `/am status` and the Session tab show the database's market and
   age; tracked items value on `tracker-d1`; a deal still arms; settings and
   the snipe list survive `/reload`; with the database addon disabled the
   sniper still values on its live tail and says the database is not loaded.
9. Retail client, if available: steps 1–3 and 7.

## 11. Batches and steps

Each batch ends with every gate green and one commit in its repository.

### Batch 1 — tracker: record codec and golden fixtures (additive)

1. Domain types of §6.1, documented.
2. `pack.rs` and `record.rs` with their unit tests.
3. Carrier and JSON renderers as pure functions over `DatabaseSnapshot`.
4. Golden-fixture test; generate and commit the three fixture files.
5. Gates. v3 still publishes.

Done when the fixtures are committed and the Rust gates pass.

**Status: done.** It also closed two gaps this spec had in §5: `floor` and
`floorQty` now ride under the `listed` flag (a record seen today and gone has
no floor to carry), and the `value` group gained a flag word for
`soldMedian14` and `trendBp`.

### Batch 2 — tracker: v4 export, publish, serve; delete v3

1. Facts accessor on `OpportunityService`.
2. `DatabaseExportService` (§6.3) and its tests.
3. Wire `AppState`, composition root and CLI.
4. `run_pass` ordering (§6.4).
5. Routes (§6.5); remove slim feed and its cache.
6. Delete v3 (§6.8).
7. Docs (§6.9).
8. Local check against the dev database: file size per market, publish time,
   decode with Lua 5.1 and compare against `feed.json`.

Done when a generated carrier is at most about 3 MB per market and decodes
losslessly.

**Status: done.** Measured on the live `forever-eu-pvp-alliance` market
(7,570 records, 29,916 span blocks; see the note below):

| Quantity | Value |
|---|---|
| Carrier | 1,888,151 bytes — about 250 per record, the largest record line 342 |
| JSON twin | 19.1 MB (pretty-printed, decoded) |
| Lua 5.1.5 load of the carrier | 17–20 ms |
| Lua 5.1.5 decode | every record field-for-field equal to `feed.json` |
| Export | about 11 s for a market's first export, about 2 s afterwards |

The development machine held no usable database (its copy predates the
v2 sighting schema), so the store was seeded from the market's real
`/api/history?days=30` day rows — every fold, the daily lows and the listing
counts are the live ones; the ledger and the live snapshot's quantities are
synthetic. On a real ledger far fewer records carry sold evidence. The
carrier was checked by a Lua 5.1 decoder written for the check and kept only
in the gitignored `target/` (`verify_v4.lua`);
the addon's own decoder (batch 3) is held to the golden trio instead.

It also settled four details this section had left open: the facts accessor
exposes no ledger recency (no field reads it); `basis_points` and
`saturate_u16` live in the core; the scan's daily lows are ordered by day,
oldest first, as the sparkline reads them; and §5.4 now spells out how a
negative zigzag code decodes.

### Batch 3 — addon core, specs, gates, CI

1. Folder, TOC, slot placeholders.
2. `Core/pack.lua`, `Core/record.lua`, `Core/market.lua`, `Core/settings.lua`,
   `Core/format.lua`, `Core/resolve.lua`.
3. `Adapters/market_files.lua`, `Adapters/compat.lua`,
   `Adapters/public_api.lua`, a first `bootstrap.lua` with the status line.
4. Specs: pack, record, golden trio, market, settings, format, resolve,
   public API, market files.
5. Lint and type configs; `addon/scripts/*`; CI `addon` job; docker `needs`.

After batch 2 the repository holds no other hand-written Lua (the two `.lua`
golden fixtures are Rust test outputs and stay outside the addon's gates), so
the gates cover every addon file.

Done when every Lua gate passes locally and in CI.

**Status: done.** Every §9 gate is green through
`addon/scripts/ci_local.sh` (574 tests); the CI `addon` job first runs with
this commit. Measured under Lua 5.1.5 on batch 2's live
`forever-eu-pvp-alliance` carrier (7,570 records, 1.89 MB):

| Quantity | Value |
|---|---|
| Load of the addon files before the slots | about 4 ms |
| Slot file: compile, validate and decode every record | 276 ms, 0 dropped |
| `Store:record` | about 32 µs, as §7.3.2 assumed |

The Lua decoder was also diffed once against the Rust decoder, in a throwaway
harness kept only in `target/`: 693,232 texts (every text up to three symbols,
every prefix and single-symbol mutation of ten records, random byte streams,
boundary records). Same verdict on every input, same message on every refusal,
identical fields on all 152,464 accepted records, and every accepted text
re-encodes to itself.

It also settled details this spec had left open:

- The module header is `local _, A = ...` unless the addon name is read, and
  specs load modules through the loader rather than `require` (§7.2).
- A TOC line arrives with the file it loads (§7.1).
- luacheck must run from `addon/`, generated slots are exempt from StyLua and
  the line-length limit, and LuaLS ignores the tool tree through
  `workspace.ignoreDir` (§9).
- Every commit takes a slot number, a rejected one included; a replaced market
  keeps its later slot; `setPrimary` fires the update callbacks (§5.9, §7.3.2).
- A carrier's `target` must match the tracker's id grammar (§5.2).
- An `evidence` or `position` group announced with a zero flag word is refused
  by both decoders, so every accepted text is an encoder's output (§5.4).
- The reader's methods take the field name, and `supplyUnbounded()` names the
  unbounded-supply rule of §5.8 (§7.3).
- The report sink's surface and the `PLAYER_LOGIN` half of the bootstrap (§7.4).

### Batch 4 — addon tooltip

1. `Core/anchor.lua`, `Core/tooltip_lines.lua`.
2. `Adapters/extra_tip.lua`, `Adapters/tooltip.lua`.
3. `Adapters/options.lua` slash command; SavedVariables.
4. The five TOC lines of §7.1, with their files.
5. Specs. In-game validation (§10): steps 2 to 6 are the owner's and were not
   run for this batch.

Done when every gate is green and the tooltip shows the sample of §7.6.

**Status: done.** Every §9 gate is green through `addon/scripts/ci_local.sh`
(795 tests). It also settled details this spec had left open:

- The two samples of §7.6 are pinned line for line by specs built from the
  §5.10 record; their numbers now agree with it and with each other (the
  header and `Snapshots` ages, the `Sold` and `Supply` figures, the other
  markets' prices).
- A header badge and the `Snapshots` line read the record's own stamp
  (§7.5.4).
- The sale-rate label names the window it shows, and the fortnight's counts
  stand only beside the 14-day rate (§7.6).
- The sparkline hangs on its row's own font string, not on the frame's bottom:
  its row is not always the last line (§7.4).
- The id in the footer and in `decode` prints ungrouped (§7.6, §7.7).
- `A.SayNow` (§7.4) and `Format.ageColor` (§7.3).
- A Core module binds `Core/settings.lua` under its own name, and a class
  annotation sits directly above its table (§7.2).
- The `SLASH_` globals set by name are curated non-events for the compat guard
  (§9).
- A login that no `ADDON_LOADED` preceded loads the settings itself (§7.4).

### Batch 5 — addon options panel

`Settings` panel rows of §7.8, on the options adapter batch 4 created; spec
for the row model.

Done when every gate is green and the panel shows the rows of §7.8.

**Status: done.** Every §9 gate is green through `addon/scripts/ci_local.sh`
(847 tests). In-game validation (§10), step 7, is the owner's and was not run
for this batch. It also settled details this spec had left open:

- The panel is a canvas of our own controls, and the row model is a Core
  module of its own, `Core/panel_rows.lua` (§7.3): the labels and the hover
  help live next to the settings module's line keys and presets, and a spec
  holds every line and every preset to its row.
- The frame carries no script on the Settings window's open or close path. A
  shown frame refreshes on its own `OnUpdate`, at most once a second, and
  every `apply` refreshes it at once (§7.8).
- The command and the panel share one `apply` method per change (§7.7).
- The primary-market picker is the client's menu where `MenuUtil` exists and
  a click-cycle otherwise; `MenuUtil` joined the optional globals with its
  fallback sentence (§7.4).
- The status line of §7.11 is the panel's last row: the bootstrap hands its
  composer to the options as the dependency `statusLine`.
- A dependency is never named like a method of the object that holds it
  (§7.2).
- `ANCHOR_CURSOR` is a curated non-event literal of the compat guard (§9).
- Two specs are new, `panel_rows_spec.lua` and `panel_spec.lua` (§10).

### Batch 6 — tracker: addon bundle and sync

1. `addon_bundle.rs` with the drift test.
2. The two addon routes; OpenAPI.
3. `addon-sync` command (§6.7) with unit tests for placement and verification.
4. Release workflow attaches the addon zip.
5. README.

**Status: done.** Every §9 gate is green: nightly `fmt --check`, clippy with
`-D warnings` over all targets, `cargo test --workspace` and
`cargo deny check`. The command was also run by hand against a local `serve`
(a fresh install of the 23 files, a slot pinned with `--market`, a dry run,
and unpublished markets reported as failures). It also settled details this
spec had left open:

- The file route is a wildcard: axum's `{*path}` on the router, `{path}` in
  the OpenAPI document; the drift test normalizes the two spellings (§6.5).
- The manifest's `version` is the addon's own, read from its TOC, and a test
  pins it to the addon's `A.VERSION` (§6.7).
- A slot's placeholder is written only where the slot is empty, so the addon
  step never replaces a market; slots no run claims are left as they are.
- Verification mirrors the addon's header rules: the closing line, the
  target grammar, a non-empty tracker version and the clock skew join the
  version and the market (§6.7).
- The sync is a core service over two ports, `TrackerAddonPort` and
  `AddonFolderPort`; its errors are `SyncError`; the tracker validates a
  download against its manifest entry before the service ever sees it.
- The addons directory is never created, a market without a free slot is a
  failure of the run, and a target list that cannot be read does not undo
  the addon step.
- The release job waits for the addon gates, and the Docker build context
  keeps only the deployable addon folder.

### Batch 7 — sniper reads the database (§8)

**Status: done**, in `Riketta/AuctiMogger`: every gate of that repository is
green through its own `scripts/ci_local.sh` (839 tests). In-game validation
(§10, step 8) is the owner's and was not run. It also settled details this
spec had left open:

- The live-tail counters `status().market` carried stay in view as
  `status().live`; only the database section is new (§8).
- The adapter applies no staleness rule of its own: one `GetMarketInfo`
  answers the item id, the spans and the market's stamp, where `GetSpans`
  would have needed a second call for the stamp, and the evidence ceiling
  stays where it was — in the history store's `trackerEvidence`.
- A key the database does not know is cached like a known one, an addon that
  loads after the sniper is bound by the first lookup, and every API call
  is pcall'd with one report per session (§8).
- `/am ingest` and the console adapter's ingest hook left with this batch,
  not the next.
- The store's schema v2 → v3 is its first in-place migration; every other
  version is still wiped.

### Batch 8 — sniper tooltip removal and cleanup (§8)

## 12. Risks and mitigations

| Risk | Mitigation |
|---|---|
| No interface dump exists for these clients, so game APIs are unverified. | Nil-guard every surface; both tooltip hook paths; the compat manifest records each fallback; in-game checklist in §10. |
| Retail path cannot be tested here. | Code paths are flavor-agnostic; retail is documented as untested until the owner runs step 9. |
| Packing bugs would corrupt every number. | Integers only, a printable-only alphabet, a Rust reference decoder, and the golden trio pinned from both languages. |
| Data goes stale between syncs. | Age is always visible; stale markets are flagged; the sniper's valuation refuses stale evidence (§8). |
| A market file copied into a git checkout over a tracked slot shows as modified. | Same as today's `feed.lua`; `git checkout` restores the placeholder. Documented in the README. |
| The attached frame mis-anchors under UI scale. | Side decision is a pure function over effective-scale-corrected numbers; clamped to screen; checked in step 6 of §10. |
| Addon code on the Settings window's open or close path can get Log Out and Exit blocked (Mastercraft's finding on Forever). | The canvas frame has no `OnShow`, `OnHide`, `OnRefresh`, `OnCommit` or `OnDefault`; it refreshes on its own `OnUpdate` (§7.8). Checked in step 7 of §10. |
| A new addon file is never added to the embedded table. | The drift test walks the folder and fails in both directions until it is, and `include_str!` rebuilds the server whenever an embedded file changes (§6.7). |
| A bad download replaces a working addon file or market. | The tracker verifies each addon download against its manifest hash, and the service verifies each carrier before it replaces a slot; a failure leaves the file as it was (§6.7). |
| Export reads a scan that lags the newest snapshot. | Live group is read from the snapshot directly; scheduler refreshes scans before publishing. |
| Tooltip font lacks special glyphs on Forever. | Signs are `+` / `-` text; the sparkline is textures. |

## 13. Known limitations and later work

- The sparkline is a gap-blind series of the observed days, oldest first: the
  daily lows carry no day stamps, so a day without a listing leaves no gap.
  Day-stamped lows need a new bulk read.
- The value, evidence and position groups need a scan row — a listing inside
  the ranking's 7-day window. An identity last seen 8–30 days ago exports
  its span blocks and nothing else.
- `vendorPrice` reaches a record only while the item is listed in the newest
  snapshot: the ranking's scan loads the vendor prices of the listed items
  alone, and the export adds no catalog read of its own.
- No "best time to buy" line: the buy-timing service is per item only; a
  whole-market export needs a pre-aggregate.
- Pets are per species; a multi-variant species exports spans only. The
  tracker holds variant-exact data and OribosExchange derives the breed from
  the link, so exact pets can be added later.
- Gear variants (item level, suffix) are not separated: the tracker is
  item-id grain.
- No in-game inspector window, deals browser or live auction-house refresh.
- Each market is a slot file. Per-market load-on-demand addons would avoid
  overwriting tracked placeholders.
- Automatic house detection is not attempted: OribosExchange's table
  disagrees with the owner's API-verified house ids (the README's example
  uses the verified ones).
- Retail is compatible by construction but untested in game.
- `addon-sync` places every target the tracker reports: `--market` pins a
  market to a slot but cannot leave one out, and a slot no run claims is
  never reset to its placeholder (delete the file, or copy one from the
  release zip).
- The addon routes send no validator (no ETag), and every sync downloads
  each addon file and carrier in full: about 300 KB for the whole addon and
  up to a few MB per market.

## Appendix A — research evidence

Measured on the live `forever-eu-pvp-alliance` feed on 2026-11-25 under
Lua 5.1.5 (x86-64), a proxy for the client's VM:

| Quantity | Verbose v3 (today) | Packed (prototype of v4) |
|---|---|---|
| File size | 8.59 MB | 1.41 MB (17 %) |
| Entries | 7,564 (6,106 item, 1,458 commodity, 0 pet) | same |
| Bytes per entry | about 1,140 | 136 raw, 182 as text |
| Load / compile time | 61 ms compile, plus table construction | 14 ms |
| Memory as Lua tables | 41.9 MB (feed), 44–47 MB retained by the store, 83.6 MB peak | 1.8 MB retained |
| Round-trip | — | lossless on all 7,564 records, 30,081 spans |

Other facts:

- 98 % of entries carry all four spans; no item id is exported in more than
  one category today.
- OribosExchange's Forever EU data file is 3.6 MB for about 55,000 items.
- The sniper's ingest is category-blind (it ignores `category` and replaces
  per key), which is latent today because no id is dual-category.
- All pinned Lua tools are present on the development machine: WSL has
  Lua 5.1.5, luacheck 1.2.0 and busted 2.2.0; the sniper repository's
  gitignored `tools/` has StyLua 2.6.0 and lua-language-server 3.19.1.

## Appendix B — extraction inventory

What moves out of the sniper repository (`Riketta/AuctiMogger` @ `83da07c`):

| Piece | Where it was | Disposition |
|---|---|---|
| Data carrier | `AuctiMoggerTracker/` (toc + placeholder `feed.lua`) | Replaced by the slot files. |
| Ingest | `domain/rest_ingestor.lua`, `adapters/inbound/tracker_feed.lua` | Replaced by `Core/record.lua` and `Adapters/market_files.lua`. |
| Store | `market` map in `adapters/outbound/history_db.lua` | Replaced by `Core/market.lua`; no SavedVariables. |
| Tooltip | `ui/item_tooltip.lua` | Replaced by `Adapters/tooltip.lua` + `Adapters/extra_tip.lua`. |
| Contract docs | SPEC.md sections `AuctiMoggerTrackerFeed`, `AuctiMoggerDB.market`, "Item tooltips" | Pointers to this file. |
| Specs | `rest_ingestor_spec.lua`, `tracker_feed_spec.lua`, `item_tooltip_spec.lua`, market parts of `history_db_spec.lua` | Deleted; replaced by the specs in §10. |
| Tooling | `check_toc.lua`, `check_compat.lua`, `run_tests.lua`, `assert_lls_clean.lua`, `ci_local.sh`, `bootstrap-tools.sh` | Copied and scoped to `addon/`. |
| Kept in the sniper | `domain/metrics.lua` and `spec/metrics_parity_spec.lua` | The live-tail statistics and their golden vectors stay. |

## Appendix C — references

- **OribosExchange** (v1.4.20261124) — the model for data delivery: binary
  price records inside Lua strings, decoded per hover; public
  `OEMarketInfo(item, table)` and Tekkub's `GetAuctionBuyout`; per-character
  line toggles; lines rendered in an attached tooltip.
- **TradeSkillMaster** — the line catalogue users expect: Min Buyout, Market
  Value with trend, Sale Rate, Avg Daily Sold, Crafting Cost with profit,
  Sold min/avg/max, Expired since last sale; every line toggleable.
- **Mastercraft** (Forever, interface 16001) — tooltip sections with switches.
- **AuctiMogger** (sniper) — house Lua conventions, the compat manifest, the
  re-entrancy lesson behind the busy guard.
