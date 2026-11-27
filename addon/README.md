# AuctiMogger Database

An in-game World of Warcraft addon that shows the auction house data of
[auctimogger-tracker](https://github.com/Riketta/auctimogger-tracker) on every
item tooltip: the cheapest buyout right now, what the item sells for, how
often it sells, its 30-day low, the profit in crafting it and where it ranks
among the tracker's deals — and the same item on your other realms.

The game's own tooltip is never modified. The addon adds a panel of its own
beside it.

- One packed market file per realm, up to six loaded side by side.
- No auction house queries and nothing saved: market data lives in the files
  alone and never reaches SavedVariables.
- Ships with the tracker: the server embeds the addon, so what it serves
  always matches the feed it produces.
- Targets Forever (interface 16001) and retail (120100). Retail is untested
  in game.

[SPEC.md](SPEC.md) is the technical contract. This file is the guide.

## Contents

1. [Install](#install)
2. [Keep the data fresh](#keep-the-data-fresh)
3. [The tooltip](#the-tooltip)
4. [Commands](#commands)
5. [Options panel](#options-panel)
6. [Troubleshooting](#troubleshooting)
7. [For addon authors](#for-addon-authors)
8. [For developers](#for-developers)

## Install

You need the addon folder and at least one market file, both from a tracker
of this version.

**With the tracker binary**, on the machine with the game client:

```sh
auctimogger-tracker addon-sync --url https://tracker.example \
    --addons-dir "<wow>/Interface/AddOns"
```

This writes the addon into `AddOns/AuctiMoggerDatabase/` and one market per
free slot: every target the tracker reports, in its order.
`--market <target>=<slot>` pins a market to a slot, `--dry-run` reports and
writes nothing, and every download is verified before it replaces a file.

**By hand:**

1. Copy [`AuctiMoggerDatabase/`](AuctiMoggerDatabase/) from this repository —
   or the folder inside a release's `AuctiMoggerDatabase.zip` — into
   `<wow>/Interface/AddOns/`. `<wow>` is the client root, the folder that
   holds `Interface/`.
2. Download one market per slot, `Market1.lua` to `Market6.lua`:

   ```sh
   cd "<wow>/Interface/AddOns/AuctiMoggerDatabase"
   curl -sf -o Market1.lua 'https://tracker.example/feed.lua?target=forever-eu-pvp-alliance'
   curl -sf -o Market2.lua 'https://tracker.example/feed.lua?target=forever-eu-pvp-horde'
   ```

   In Windows PowerShell write `curl.exe`: `curl` is an alias for a different
   command there.

3. Enable **AuctiMogger Database** in the AddOns list at character select and
   log in. After replacing a market file, `/reload`.

The first login prints one status line in chat:

```
AuctiMogger Database: 2 markets · primary forever-eu-pvp-alliance · data 25m old
```

With several markets loaded and none chosen yet, the line asks you to pick
this character's primary market:

```
/amdb market forever-eu-pvp-alliance
```

The primary market is the one the tooltip reads and the one the AuctiMogger
sniper values on. It is saved per character.

> A market file downloaded into a git checkout of this repository overwrites a
> tracked placeholder and shows as modified. `git checkout` restores it.

## Keep the data fresh

A market file is a snapshot and the addon never refreshes it. To update,
download the files again (or run `addon-sync` again) and `/reload`.

The age never hides. The tooltip header shows it and `/amdb status` lists it
for every market:

| Age | Color |
|---|---|
| under 2 hours | muted |
| 2 to 12 hours | yellow |
| 12 to 24 hours | orange |
| over 24 hours | red, marked `stale` |

A stale market still shows its tooltips, flagged. `GetSpans` answers nothing
for it, so an addon that values on spans gets no stale evidence.

Right after the tracker was updated, its feed routes answer 404 until its first
cycle has published the new format. That is deliberate: a file the previous
version published cannot be read by this addon.

## The tooltip

Hover an item — in your bags, bank, mail, a chat link or an auction house row.
With the standard preset and every optional line on, the panel reads:

```
 AuctiMogger Database                    forever-eu-pvp-alliance · 58m
 Min buyout                 380g 50s  ×214
 Live p05                   382g
 Market value               402g 10s  sold  -3%
 Sale rate (14d)            36%  (34 of 94 sold)
 Sold                       31.6/day · supply 39d
 Last sale                  2d ago
 Last listed                today
 30-day low                 350g  · 80% of days higher
 30 days                    [ sparkline: 30 bars, lowest daily ask ]
 Crafting                   2100g → +1900g  (exp. +684g)
 Deal                       #12 · 38% under value · score 0.71
 forever-eu-pvp-horde       442g 31s  +10%
 forever-eu-pvp-blackwater  382g  -5%
```

The panel sits to the right of the game's tooltip and flips to its left when
the screen's edge is near. A line without data is left out, and a tooltip the
client reports as collapsed shows the three bold lines only.

| Line | Meaning |
|---|---|
| Header | The market the record comes from, and its age. |
| **Min buyout** | The cheapest unit price listed now, and how many units sit at exactly that price. Not listed but seen today: `none listed · seen today …`. Otherwise `not listed`. |
| Live p05 | The price under which the cheapest 5% of the listings in the newest snapshot sit — the sniping level, read off the same dump as the min buyout. With twenty listings or fewer it equals the min buyout. |
| **Market value** | What the tracker values the item at, and where that comes from: the 14-day sold median (`sold`) once at least three priced units sold, else the 7-day listing median (`listed`), else the median of a longer window (named). The signed percent is today's listing median against the previous six days. |
| **Sale rate (14d)** | The share of the fortnight's listings that sold, with the counts behind it. Without a 14-day rate the line reads `Sale rate (180d)`. |
| Sold | Units sold per day, and how long the listed supply lasts at that pace (`supply ∞` when the figure is unbounded). |
| Last sale | `today`, `1d ago`, `Nd ago`. |
| Last listed | How many days ago the item was last seen on the auction house — another question than when it last sold. It shows beside `not listed` too, which is where it says most. |
| 30-day low | The lowest ask of the previous 30 days, and on how many of those days the lowest ask was higher than today's floor. |
| 30 days | The lowest daily ask of the previous 30 days, oldest first. |
| Crafting | What one make costs in reagents, the profit at the market value after the auction house cut, and that profit times the sale rate. Losses in red. |
| Deal | For a live deal: its rank among the category's deals, how far under the market value it is, and its score. |
| Other markets | One line per other loaded market that has the item: its market value and the difference against the primary's. |

Hold **Shift** to replace the optional lines with the detail block:

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

Items are tracked by item id, so variants of a gear piece (item level, suffix)
share one record. Battle pets are looked up by species.

**Presets.** `standard` shows everything above; `basic` keeps the three bold
lines. Every other line can be switched on or off on its own.

**Coin icons.** `/amdb coins on` (or the panel's box) draws every price as the
client's own gold, silver and copper icons instead of `380g 50s` text, in the
tooltip and in chat. It needs a client with `GetMoneyString`; without it the
command says so and prices stay as text.

## Commands

`/amdb`, alias `/auctimoggerdb`.

| Command | Effect |
|---|---|
| `on` / `off` | The master switch. |
| `status` | Every market (slot, target, records, age), the primary market, the switch and the preset; then anything that failed to load and any client surface the addon lacks. |
| `market` | List the loaded markets. |
| `market <target>` | Choose this character's primary market. |
| `lines` | List the optional lines and their state. |
| `basic` / `standard` | Presets: the optional lines all off / all on. |
| `<line> on` / `<line> off` | One optional line: `p05`, `sold`, `lastSale`, `listed`, `low`, `sparkline`, `craft`, `deal`, `markets`. |
| `shift on` / `shift off` | Whether Shift shows the detail block. |
| `coins on` / `coins off` | Prices as coin icons instead of text. |
| `decode <item>` | Debug: every decoded field of the item, from an id, a link or an `item:` string. An absent field prints `-`. |
| `help` | The list above. |

`/amdb` alone and `help` print the list. Settings are saved as you change them
and shared across the account; the primary market is per character.

## Options panel

Interface → Options → AddOns → AuctiMogger Database: the master switch, the
presets, one checkbox per optional line, the Shift detail, the coin icons and
the primary-market picker. The panel and the command change the same settings
through the same path, so a change made in one shows in the other without a
reload.

A client without the settings API gets no panel and says so once at login;
the command covers everything.

## Troubleshooting

| Chat says | What it means | What to do |
|---|---|---|
| `no market data — copy the tracker's feed.lua over Market1.lua` | No slot file holds a market. | Install a market file. |
| `a version 3 tracker feed is in a market slot and was not read — …` | A slot file holds the old feed format, usually one downloaded before an updated tracker had published again. | Update the tracker, wait for its first cycle, then download the market again over that slot — the file that opens with `AuctiMoggerTrackerFeed`. |
| `several markets loaded, pick one: /amdb market <id>` | More than one market is loaded and this character has no primary market. | `/amdb market <target>`. |
| `<market> is 2d old — resync` | A market other than the primary is over 24 hours old. | Download it again and `/reload`. |
| `rejected: …` | A slot file is not a feed this version reads: another version, a malformed header, a clock more than five minutes ahead, or a truncated download. | Update the tracker, then download again. |
| `replaces slot N` | The same market arrived from two slots; the later slot won. | Delete one of the two files. |
| `dropped: N` | N records of a market failed to decode and were skipped; the rest loaded. | Update the tracker, then download again. |
| `no options panel on this client — the /amdb command covers everything` | The client has no settings API. | Use `/amdb`. |
| `this client has no GetMoneyString: prices stay in text` | The client cannot draw coin icons. | Leave prices as text. |

`/amdb status` repeats all of it.

## For addon authors

The addon publishes one global, `AuctiMoggerDatabase`, and one global
function, `GetAuctionBuyout`. Load it first with
`## OptionalDeps: AuctiMoggerDatabase` in your TOC, and guard the global: it
is nil when the addon is absent or disabled.

| Function | Result |
|---|---|
| `VERSION`, `FEED_VERSION` | The addon's version, and the feed version it reads. |
| `GetMarketInfo(item, opts)` | One item of the primary market, or `nil`. |
| `GetSpans(item, category)` | The per-window statistics, or `nil` when the item is unknown or the market is stale. |
| `GetMarkets()` | Every loaded market: `target`, `slot`, `records`, `generatedAt`, `ageSeconds`, `stale`. |
| `GetPrimaryMarket()` | The primary market's target id, or `nil`. |
| `OnUpdate(callback)` | Runs `callback` at login and on every market change. Returns a function that unsubscribes. |
| `GetAuctionBuyout(item)` | The market value in copper — the convention other price addons read. When it has no value, it chains to a `GetAuctionBuyout` defined before it. |

`item` is an item id, an item link or an `item:` string. A pet takes a
`battlepet:` link, or a species id with `opts.species`. `opts.category` and
`opts.market` are optional and select another pool or another market.

`GetMarketInfo` returns a fresh, flat table — `floorNow`, `floorQty`,
`listedNow`, `p05Now`, `daysSinceListed`, `value`, `valueSource`, `trend`, `saleRate`,
`saleRate180d`, `low30`, `craft`, `deal`, `stale`, `ageSeconds` and the rest —
with `nil` for every field the tracker has no evidence for. Keep what you
need: the next call builds a new one.

```lua
local Database = AuctiMoggerDatabase
local info = Database and Database.GetMarketInfo(194784)
if info and not info.stale then
    print(info.value, info.floorNow, info.saleRate)
end
```

## For developers

```
AuctiMoggerDatabase/   the deployable addon: the TOC, bootstrap.lua, Core/,
                        Adapters/ and the six slot placeholders
spec/                  the busted specs
scripts/               ci_local.sh, bootstrap-tools.sh and the guards they run
tools/                 gitignored: StyLua and LuaLS, fetched by bootstrap-tools.sh
SPEC.md                the contract: the feed format, every module, the test plan
```

Every gate green before a commit:

```sh
bash addon/scripts/bootstrap-tools.sh   # once, on Windows: StyLua and LuaLS into tools/
bash addon/scripts/ci_local.sh          # from anywhere
```

`ci_local.sh` runs StyLua, luacheck, busted, the TOC coverage guard, the
compat manifest guard and LuaLS with the versions CI pins: Lua 5.1,
luacheck 1.2.0, busted 2.2.0, StyLua 2.6.0 and lua-language-server 3.19.1. On
Windows the Unix gates run in WSL, with the rocks installed under
`~/.luarocks`. Without rocks, `lua scripts/run_tests.lua` from this folder
runs the specs with a plain Lua interpreter.

- Every game API the addon touches is listed in a tier of
  `AuctiMoggerDatabase/Adapters/compat.lua`; the guard fails the build on an
  unlisted one.
- A new file in the deployable folder goes into the TOC and into the server's
  embedded bundle (`crates/server/src/addon_bundle.rs`). A test fails until
  folder, TOC and bundle agree.
- When `SPEC.md` and the code disagree, the code wins: fix the spec in the
  same change.
