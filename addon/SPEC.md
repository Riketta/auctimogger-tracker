# SPEC — AuctiMogger Database

Status: **in progress**. Batch 1 is implemented: the domain types, the record
codec, the carrier and JSON-twin renderers and the golden fixtures. Nothing else
is; the rest follows the batches in §11. This file is the single source of
truth for the AuctiMogger Database addon and for the tracker feed (v4) it
consumes. When this file and the code disagree, the code wins — fix the doc in
the same change.

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
| `target` | string | yes | Tracker target id — the market's key. |
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
  keeps the sign bit apart and accumulates `floor(code / 2)`, so no
  intermediate exceeds 2^53.

The Rust decoder (`crates/infra/src/record.rs`) is the reference; the addon's
Lua decoder refuses exactly the same things:

| Malformed when |
|---|---|
| A character is not in `ALPHABET`. |
| A varint is longer than 10 bytes, or not minimal (a trailing zero group, as in `80 00`). |
| An integer is `2^53` or more in magnitude. |
| The padding is more than 4 bits or not all zero. |
| A flag word sets a bit with no meaning, or `qtySum` is set without `listed`. |
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
| Required header field missing or malformed | Market rejected, reported. |
| `generatedAt` more than 5 minutes in the future | Market rejected (diverged generator), reported. |
| `generatedAt` older than 24 hours | Market **kept and flagged stale**: tooltips render with a red age badge; `GetSpans` answers `nil` so the sniper gets no stale evidence. |
| Same `target` already registered from another slot | The later slot replaces the earlier one; reported. |
| A record fails to decode | That record is dropped and counted; the market stays. Reported once per market at login. |
| Slot file is a placeholder or fails to compile | Nothing registers; a slot that fails to compile shows the client's own load error. |

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

In `crates/core/src/domain.rs` (added in batch 1; the v3 payload types leave in
batch 2):

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

Batch 2 removes `FeedEntry` and `FeedSnapshot`. Kept under their names because
they are generic transport ports: `FeedFormat`, `FeedRenderer`, `FeedPublisher`,
`PublishedFeed`.

### 6.2 Codec modules

- `crates/infra/src/pack.rs` — `ALPHABET`, `MAX_INTEGER`, `put_varint`,
  `put_zigzag`, `zigzag`/`unzigzag`, `to_text`, `from_text`, a `Reader`
  (`varint`, `zigzag`, `uint`, `byte`, `flag`, `finish`), `basis_points`,
  `saturate_u16` and `PackError`. The raw varint reader is exact over the whole
  `u64` range; the `2^53 − 1` ceiling is applied to the integer a field carries.
- `crates/infra/src/record.rs` — `encode(&ItemRecord) -> String` and
  `decode(&str) -> Result<ItemRecord, PackError>`. The decoder is the Rust
  reference used by tests; the game runs the Lua one.
- `crates/infra/src/feed.rs` — `FeedCodec::carrier` and `FeedCodec::json_twin`
  render a `DatabaseSnapshot` (batch 1). The `FeedRenderer` implementation on
  `FeedCodec` still renders v3 until batch 2 moves the port to
  `DatabaseSnapshot`; `FileFeedPublisher` is unchanged.
- Both renderers refuse a snapshot that would publish a wrong file rather than
  publish it: an identity filed under the wrong table, a pet below species
  grain, a record without a span, or keys that do not ascend (in Lua a
  duplicate key would silently overwrite another record).
- Header strings come from configuration, so they are escaped into pure ASCII
  (`lua_string_literal`): no value can end its line, or the file, early.

### 6.3 Export service

`crates/core/src/services/database_export.rs` replaces `feed_export.rs`:
`DatabaseExportService { history, snapshots, metrics, opportunities, crafts, clock }`
with `build(target, &ExportOptions { spans }) -> Result<DatabaseSnapshot, _>`.

Algorithm:

1. `generated_at` — as v3. `auctions_at` / `commodities_at` — the `fetched_at`
   of each payload kind's newest snapshot.
2. Per category: identities from `history.items_with_data` over the longest
   span window; group by feed grain (pets by species); span blocks through the
   cached bulk paths — all exactly as v3.
3. Facts from the prepared scan through a new read accessor on
   `OpportunityService` exposing, per identity: `value7`, `sold_median14`,
   `samples7`, `median_now` and `median_prev` (for the trend), sold/expired
   counts, sold units, recency, the 30 daily lows and the vendor price.
4. Live group from `snapshots.floors(target, kind, snapshot_id)` of the newest
   snapshot — **read directly, not from the scan**, so the min buyout never
   lags an ingest cycle.
5. `low30` and `lowRank30d` computed with the ranking's own `low_rank`.
6. Crafts: `crafts.crafts(target).ranked`, first row per output.
7. Deals: `opportunities.opportunities(target, category, 0.0, MAX, Score)`,
   the index in that order is the rank.
8. Attach groups per the rules of §5.6, sort, return.

Cost: span blocks and scan facts are cached; the only new store reads are the
two floor queries per publish.

### 6.4 Freshness and scheduler ordering

Today `scheduler::run_pass` publishes the feed **before** refreshing the
prepared scans, and the manual `/api/fetch` path reuses `run_pass`. New order:

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
| `GET /api/addon/manifest` | New (§6.7). |
| `GET /api/addon/file/<path>` | New (§6.7). |

`ResponseCache::slim_feed` is removed. The OpenAPI document, the `/api`
directory, the dashboard's feed links and the drift test are updated.

### 6.6 CLI

- `export-feed` loses `--slim`; it writes `feed.lua` and `feed.json` under
  `<out>/<target>/`.
- New `addon-sync` (§6.7).

### 6.7 Addon bundle and sync command

The server binary embeds the addon's static files with `include_str!` in
`crates/server/src/addon_bundle.rs`, so the served addon always matches the
served feed format.

- `GET /api/addon/manifest` —
  `{ "addon": "AuctiMoggerDatabase", "version": "0.1.0", "feedVersion": 4,
  "files": [ { "path", "sha256", "size" } ] }`.
- `GET /api/addon/file/<path>` — one embedded file; any other path is 404.
- A test fails when the embedded table and `addon/AuctiMoggerDatabase/` on
  disk disagree in either direction.

```
auctimogger-tracker addon-sync --url <base> --addons-dir <Interface/AddOns>
                              [--market <target>=<slot>]... [--no-addon]
                              [--no-markets] [--dry-run] [--timeout-secs N]
```

1. Write the addon's static files into `<addons-dir>/AuctiMoggerDatabase/`
   when their hash differs (atomic: temp file, then rename). Slots are written
   as placeholders, so a fresh install is complete.
2. Markets: `--market` placements first; then every target reported by
   `GET /api/health`, in order, into the free slots 1–6.
3. For each market fetch `feed.lua?target=` and verify before replacing:
   header line present, `version = 4`, `target` as expected.
4. Print a table (slot, target, bytes, age) and remind to `/reload`.
   Non-zero exit on any failure; HTTP through the `ureq` already in `infra`.

The release workflow additionally attaches `AuctiMoggerDatabase.zip` (the addon
folder with placeholder slots) for manual installs.

### 6.8 Deletions

- v3 renderers and their tests; `ExportOptions.watchlist_only`.
- `scripts/verify_feed.lua`; the `generated_lua_passes_the_contract_verifier`
  test; the CI step that installs lua5.4 for it.

### 6.9 Documentation to update

- `README.md`: "Feed contract (v3)" becomes "(v4)" and points here;
  "Feed the addon" becomes "AuctiMogger Database"; API table rows for the feed
  and the two addon routes.
- The README's Forever example pairs `forever-eu-pvp-alliance` with
  `house_id = 113`, which is the Horde house (Alliance is 111): fix it.
- `crates/server/src/web/openapi.rs`.

## 7. Addon implementation

### 7.1 Folder and TOC

```
addon/
  SPEC.md                       this file
  .luacheckrc  .luarc.json  .stylua.toml
  spec/                         busted specs
  scripts/                      check_toc, check_compat, run_tests,
                                assert_lls_clean, ci_local, bootstrap-tools
  AuctiMoggerDatabase/          deployable folder (symlink into Interface/AddOns)
    AuctiMoggerDatabase.toc
    bootstrap.lua               composition root, loads last
    Core/                       pure Lua: pack, record, market, settings,
                                format, resolve, tooltip_lines, anchor
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

# Market registration (no game API; must load before the slots)
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

### 7.2 Module conventions

Carried over from the sniper's house style:

- Lua 5.1 only. Every module starts with `local ADDON, A = ...`, registers into
  the namespace (`A.Core.Pack = Pack`) **and** returns its table, so specs can
  `require("AuctiMoggerDatabase.Core.pack")` from `addon/`.
- `Core/` references no game global; CI fails on any.
- EmmyLua classes with complete `@field` lists; `@param`/`@return` everywhere;
  no bare `table`/`any` where a shape is known.
- StyLua is canonical: spaces, indent 4, double quotes, column 120, LF.
- Every game API used is listed in `Adapters/compat.lua` and in the
  `.luacheckrc` / `.luarc.json` allowlists; a CI guard diffs usage against the
  manifest.
- All persisted input is treated as untrusted: validated, dropped or clamped,
  never fatal at login.

### 7.3 Core modules

| Module | Responsibility |
|---|---|
| `Core/pack.lua` | `ALPHABET`; a `Reader` over a record string: `varint()`, `zigzag()`, `byte()`, `finish()`. |
| `Core/record.lua` | `Record.decode(text) -> record, error`. Produces `AmdbRecord` (§7.3.1) with the derived methods of §5.8. |
| `Core/market.lua` | `Market` (one loaded market) and `Store` (all markets): registration, lookups, primary binding, freshness tiers, change callbacks. Pure: `now` is a parameter. |
| `Core/settings.lua` | Defaults, strict coercion, presets, per-line visibility. |
| `Core/format.lua` | Money, percent, signed percent, count and age text with the house colors. No game API. |
| `Core/resolve.lua` | Item identity from a number, hyperlink or `item:`/`battlepet:` string. |
| `Core/tooltip_lines.lua` | Pure line builder: record + context → lines. |
| `Core/anchor.lua` | Pure side decision for the attached frame. |

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

#### 7.3.2 `Store`

- `Store.new()`; `Store:begin(meta) -> builder` with `items`, `commodities`,
  `pets`; `Store:commit()` validates and registers (§5.9) and assigns the next
  slot number in load order.
- `Store:markets()`, `Store:primary()`, `Store:setPrimary(target)`,
  `Store:setExplicitPrimary(target|nil)`.
- Primary market resolution: the explicit per-character choice; else the only
  loaded market; else slot 1. With several markets and no choice, the login
  status line says how to choose.
- `Store:record(itemID, category|nil, target|nil)` and
  `Store:petRecord(speciesID, target|nil)` decode on each call — about 30 µs,
  so there is no cache to invalidate. Callers get fresh tables and may keep
  them.
- `Store:onUpdate(callback)`; fired once after login and whenever the set of
  markets changes.
- `Market:ageSeconds(now)` uses `generatedAt`;
  `Market:snapshotAge(kind, now)` uses `auctionsAt` / `commoditiesAt`;
  `Market:isStale(now)` is `age > 24 h`.
- `Store:stats()` — per market: slot, target, record counts, age, stale,
  dropped records.

### 7.4 Adapters

**`Adapters/market_files.lua`** — creates the `Store` at load time and
publishes `A.BeginMarket(meta)` / `A.EndMarket()`, the only functions a carrier
calls. `BeginMarket` replaces an unclosed builder (malformed file) and reports
it. Reports go through one `say(text)` sink, silent unless verbose.

**`Adapters/compat.lua`** — the manifest of every game surface the addon
touches, in tiers, plus a `check()` used by `/amdb status`:

| Tier | Surface |
|---|---|
| Required globals | `CreateFrame`, `GameTooltip`, `UIParent`, `time`, `SlashCmdList` |
| Required events | `ADDON_LOADED`, `PLAYER_LOGIN` |
| Optional globals | `TooltipDataProcessor`, `Enum`, `ItemRefTooltip`, `issecretvalue`, `IsShiftKeyDown`, `C_Timer`, `Settings`, `print` |
| Optional events | `MODIFIER_STATE_CHANGED`, `PLAYER_LOGOUT` |
| Authored | `SLASH_AMDB1`, `SLASH_AMDB2`, the namespace table `AuctiMoggerDatabase` |

Each optional entry has a documented fallback. No `C_AuctionHouse` function
appears anywhere.

**`Adapters/extra_tip.lua`** — the attached frame.

- One `GameTooltip` created with `CreateFrame("GameTooltip",
  "AuctiMoggerDatabaseTip", UIParent, "GameTooltipTemplate")`, stratum
  `TOOLTIP`, `SetClampedToScreen(true)`, owner `UIParent`, anchor `ANCHOR_NONE`.
- `ExtraTip:show(owner, lines)`, `:hide()`, `:isShown()`.
- Lines render through the native `AddLine` / `AddDoubleLine`. The sparkline is
  a row of `Texture` regions parented to the frame and anchored to its bottom.
- Positioning (§7.5.3) runs one frame after the fill, once the owner has its
  final size.

**`Adapters/tooltip.lua`** — hooks, lifecycle, Shift, re-entrancy guard
(§7.5). `Tooltip.new({ store, settings, say })`; `setEnabled`, `isEnabled`.

**`Adapters/options.lua`** — the `/amdb` slash command and the settings panel
(§7.7, §7.8).

**`Adapters/public_api.lua`** — publishes the namespace table as the global `AuctiMoggerDatabase` (§7.9).

**`bootstrap.lua`** — builds the adapters in order; owns the SavedVariables
lifecycle (`ADDON_LOADED` → load settings; `PLAYER_LOGIN` → create the frame,
register hooks and the panel, print the status line, fire update callbacks).

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

#### 7.5.2 Lifecycle

1. Post-call → resolve identity → `Store:record()`. No record: hide the frame.
2. Build lines with `Core/tooltip_lines.lua`; schedule display on the next
   frame (`C_Timer.After(0, …)`).
3. On display: if the owner is no longer shown, stay hidden. Otherwise position
   and show.
4. Hide on the owner's `OnTooltipCleared` and `OnHide` (instance hooks
   installed once at login) and before every re-show.
5. `MODIFIER_STATE_CHANGED` for Shift rebuilds a shown frame at the new detail
   level. The owner is never refreshed, so no re-entrancy is possible.
6. Rendering is pcall-wrapped behind a busy guard; the first error is reported
   once.

Taint: the frame is ours and unprotected; every write is cosmetic. The owner is
only read. Nothing is combat-gated.

#### 7.5.3 Anchoring

`Core/anchor.lua` decides the side from four numbers in one coordinate space:
the owner's left and right edges, the frame's width and the screen's right
edge. The adapter converts with `GetEffectiveScale()`.

- Default: frame's top-left at the owner's top-right, 8 px gap.
- If that would cross the screen's right edge: mirror to the owner's left.
- Vertically the frame's top aligns with the owner's top minus 10 px.

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
muted, 2–12 h warn, 12–24 h hot, over 24 h `stale` in bad.

### 7.6 Tooltip lines

Standard level, every optional line on:

```
 AuctiMogger Database                    forever-eu-pvp-alliance · 25m
 Min buyout                 380g 50s  ×214   ≤ vendor
 Market value               412g 35s  sold  -3%
 Sale rate (14d)            36%   (34 of 94 sold)
 Sold                       2.4/day · supply 39d
 Last sale                  2d ago
 30-day low                 350g  · 80% of days higher
 30 days                    [ sparkline: 30 bars, lowest daily ask ]
 Crafting                   210g → +190g  (exp. +68g)
 Deal                       #12 · 38% under value · score 0.71
 forever-eu-pvp-horde       455g   +10%
 forever-eu-pvp-blackwater  391g   -5%
```

| Line | Shown when | Text |
|---|---|---|
| Header | always | Title; right: `target · age`. |
| **Min buyout** | always | `money(floor)` + dim ` ×floorQty`; green `≤ vendor` when `atVendorFloor()`. Not listed but seen today: dim `none listed · seen today money(seenFloor) ×seenQty`. Otherwise dim `not listed`. |
| **Market value** | always | `money(reference)` + dim source word (`sold` / `listed` / span name) + signed `trendBp` percent in good/bad when present. |
| **Sale rate (14d)** | always | Percent in the rate color; dim `(soldCount of soldCount + expiredCount sold)`, or `180d` rate when the 14-day rate is absent. |
| Sold | `sold` on, `soldPerDay()` known | `x.x/day · supply Nd`, `supply ∞` when unbounded. |
| Last sale | `lastSale` on, `daysSinceLastSale()` known | `today`, `1d ago`, `Nd ago`. |
| 30-day low | `low` on, `low30` known | `money(low30)` + dim `· N% of days higher` from `lowRank30d`. |
| Sparkline | `sparkline` on, `lows` present | Label `30 days`; bars from `lows`. |
| Crafting | `craft` on | `money(cost) → ±money(profit)` + dim `(exp. ±money(expectedProfit))`; losses in bad. |
| Deal | `deal` on | good `#rank · N% under value · score x.xx`. |
| Other markets | `markets` on, more than one loaded | One line per other market that has the item: its target id; `money(reference)` and the difference against the primary reference. |

Shift-held detail replaces the optional lines with:

```
 Sold median (14d)          402g 10s
 Value basis                7-day listing value · 214 listings
 Listed                     618 listings · 1,234 units
 Vendor sell                12g 50s
 Supply                     39 days (94 listed ÷ 2.4/day)
 Sale rate (180d)           31%
 Closures since last sale   61
 Snapshots                  auctions 25m · commodities 1h 05m ago
 d1   median 401g           p05 380g · p95 420g · 58 seen · 9 sold
 d7   median 431g           p05 380g · p95 512g · 214 seen · 210 sold
 194784 · commodity · feed v4
```

Each line is omitted when its data is absent. A collapsed owner tooltip
(`IsTooltipCollapsed()`) always renders the three basics only.

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

### 7.8 Options panel

Interface → AddOns → AuctiMogger Database, built with
`Settings.RegisterCanvasLayoutCategory` when `Settings` exists. Rows are
data-driven and the panel height follows the row count: enable, preset
buttons, one checkbox per optional line, Shift detail, the primary-market
dropdown (loaded markets) and a status line. Without the `Settings` API the
panel is skipped with a one-time note; the slash command covers everything.

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
`battlepet:` link or a species id through `GetMarketInfo`'s `opts.species`.

`AmdbMarketInfo` is flat, camelCase and fresh per call: `itemID`, `category`,
`market`, `generatedAt`, `ageSeconds`, `snapshotAgeSeconds`, `stale`,
`floorNow`, `floorQty`, `listedNow`, `seenFloor`, `value`, `valueSource`,
`trend`, `soldMedian14`, `saleRate`, `saleRate180d`, `soldCount`,
`expiredCount`, `soldPerDay`, `supplyDays`, `daysSinceLastSale`,
`closuresSinceLastSale`, `low30`, `lowRank30d`, `vendorPrice`, `craft`, `deal`,
`spans`. Fractions are plain numbers; absent fields are `nil`.

### 7.10 SavedVariables

Only settings are persisted. No market data ever reaches SavedVariables.

### 7.11 Login status line

- `AuctiMogger Database: 2 markets · primary forever-eu-pvp-alliance · data 25m old`
- No data: `AuctiMogger Database: no market data — copy the tracker's feed.lua over Market1.lua` (bad).
- Several markets, no choice: append `· several markets loaded, pick one: /amdb market <id>`.
- Stale market: `· forever-eu-pvp-horde is 2d old — resync` (hot).

## 8. Sniper migration

Repository `Riketta/AuctiMogger`. The sniper keeps its live tail, its deals
ring and its seen marks. Everything that stored tracker data moves out.

**Batch 7 — read the database**

- TOC: `## OptionalDeps: AuctiMoggerDatabase`.
- `ports/outbound.lua`: new read-only `MarketDatabasePort` with
  `entry(keyString) -> { itemID, spans, generatedAt } | nil` and
  `describe()`. `MarketDataPort` loses `beginSnapshot`, `commitSnapshot`,
  `upsertMetrics`.
- New `adapters/outbound/market_database.lua`: wraps the `AuctiMoggerDatabase`
  global; caches one entry per key string and clears on `OnUpdate`, so the
  per-lot hot path never decodes. Inert when the addon is absent.
- `adapters/outbound/history_db.lua`: the `market` map and its three write
  methods are removed; `trackerEvidence` reads the port. The valuation tiers
  and the 24-hour ceiling are unchanged.
- Delete `domain/rest_ingestor.lua`, `adapters/inbound/tracker_feed.lua`, the
  `AuctiMoggerTracker/` folder and their specs.
- `bootstrap.lua`: drop the ingestor, the feed adapter, `ingestAndLog`,
  `IngestTrackerFeed` and the re-ingest on list change.
- The sniper's own SavedVariables global `AuctiMoggerDB`: schema version 3;
  on load drop the stale `market`, `snapshots` and `counters.restKeysIngested`.
  Deals, resets, seen and observed marks are kept.
- `status().market` becomes `status().database = { loaded, market, ageSeconds }`.
- SPEC.md, AGENTS.md rule 2 (one write path: live observations), README.

**Batch 8 — remove the sniper's tooltip**

- Delete `ui/item_tooltip.lua` and its spec; drop the Session-tab "Item tips"
  button, the `/am tooltip` and `/am ingest` commands and the
  `AuctiMoggerUI.itemTooltip` key (pruned on load).
- Remove their manifest and allowlist entries; CI packages only
  `AuctiMogger`.

## 9. Quality gates

**Tracker (Rust)** — unchanged: nightly `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace`, `cargo deny check`. The golden-fixture test joins
them.

**Addon (Lua)** — every gate scoped to `addon/`, because the tree also holds
Lua these gates must not judge: foreign copies and probes in the gitignored
`target/`, and the differently styled `scripts/verify_feed.lua` until batch 2
deletes it:

```sh
stylua --check addon
luacheck addon
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
- CI: a new `addon` job on `ubuntu-latest` with the same pins; the `docker`
  job then needs `linux` **and** `addon`.

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
- Addon bundle: embedded table matches the folder.

**Lua**

- `pack`, `record`: every field, every absence, malformed and truncated
  records, wrong padding.
- Golden trio (§5.11).
- `market`: version gate, future rejection, stale flag, duplicate target,
  slot order, primary binding, default category, update callbacks.
- `settings`: defaults, strict coercion both ways, presets.
- `format`, `resolve`, `anchor`: table tests.
- `tooltip_lines`: one test per line for shown and omitted, Shift detail,
  collapsed owner, each toggle.
- `market_files`: a carrier loaded through a fake `...`; malformed header;
  unclosed builder.
- `tooltip`: registration on both paths, lifecycle (hidden when the owner is
  not shown, hidden on clear), Shift rebuild, busy guard, errors reported once.
- `public_api`: fresh tables per call, `GetAuctionBuyout` chaining, absent
  addon data.

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
7. Sniper: `/am status` shows the database; tracked items value on
   `tracker-d1`; a deal still arms; settings survive `/reload`.
8. Retail client, if available: steps 1–3.

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

### Batch 3 — addon core, specs, gates, CI

1. Folder, TOC, slot placeholders.
2. `Core/pack.lua`, `Core/record.lua`, `Core/market.lua`, `Core/settings.lua`,
   `Core/format.lua`, `Core/resolve.lua`.
3. `Adapters/market_files.lua`, `Adapters/compat.lua`,
   `Adapters/public_api.lua`, a first `bootstrap.lua` with the status line.
4. Specs: pack, record, golden trio, market, settings, format, resolve,
   public API, market files.
5. Lint and type configs; `addon/scripts/*`; CI `addon` job; docker `needs`.

After batch 2 the repository holds no other Lua, so the gates cover every
`.lua` file in the tree.

Done when every Lua gate passes locally and in CI.

### Batch 4 — addon tooltip

1. `Core/anchor.lua`, `Core/tooltip_lines.lua`.
2. `Adapters/extra_tip.lua`, `Adapters/tooltip.lua`.
3. `Adapters/options.lua` slash command; SavedVariables.
4. Specs. In-game validation (§10).

### Batch 5 — addon options panel

`Settings` panel rows of §7.8; spec for the row model.

### Batch 6 — tracker: addon bundle and sync

1. `addon_bundle.rs` with the drift test.
2. The two addon routes; OpenAPI.
3. `addon-sync` command (§6.7) with unit tests for placement and verification.
4. Release workflow attaches the addon zip.
5. README.

### Batch 7 — sniper reads the database (§8)

### Batch 8 — sniper tooltip removal and cleanup (§8)

## 12. Risks and mitigations

| Risk | Mitigation |
|---|---|
| No interface dump exists for these clients, so game APIs are unverified. | Nil-guard every surface; both tooltip hook paths; the compat manifest records each fallback; in-game checklist in §10. |
| Retail path cannot be tested here. | Code paths are flavor-agnostic; retail is documented as untested until the owner runs step 8. |
| Packing bugs would corrupt every number. | Integers only, a printable-only alphabet, a Rust reference decoder, and the golden trio pinned from both languages. |
| Data goes stale between syncs. | Age is always visible; stale markets are flagged; the sniper refuses stale spans. |
| A market file copied into a git checkout over a tracked slot shows as modified. | Same as today's `feed.lua`; `git checkout` restores the placeholder. Documented in the README. |
| The attached frame mis-anchors under UI scale. | Side decision is a pure function over effective-scale-corrected numbers; clamped to screen; checked in step 6 of §10. |
| Export reads a scan that lags the newest snapshot. | Live group is read from the snapshot directly; scheduler refreshes scans before publishing. |
| Tooltip font lacks special glyphs on Forever. | Signs are `+` / `-` text; the sparkline is textures. |

## 13. Known limitations and later work

- The sparkline is a gap-blind series of observed days: daily lows carry no
  day stamps. Day-stamped lows need a new bulk read.
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
- Automatic house detection is not attempted: OribosExchange's table and the
  tracker's README both disagree with the owner's API-verified house ids.
- Retail is compatible by construction but untested in game.

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
