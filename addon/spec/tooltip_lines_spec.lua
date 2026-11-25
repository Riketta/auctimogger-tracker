-- Specs for Core/tooltip_lines.lua: every row of SPEC 7.6, shown and omitted,
-- the Shift detail level, a collapsed owner and each line toggle (section 10
-- "tooltip_lines").
--
-- A record is built with the spec-side encoder and read back by the real
-- decoder, the way the store hands one out, so a row is only ever built from
-- a record the wire can carry. Two whole-block tests pin the sample of
-- SPEC 7.6 line for line.

local Loader = require("spec.helpers.loader")
local Encode = require("spec.helpers.encode")

local A = Loader.load(
    Loader.namespace(),
    "Core/pack.lua",
    "Core/record.lua",
    "Core/settings.lua",
    "Core/format.lua",
    "Core/tooltip_lines.lua"
)
local Record = A.Core.Record
local Settings = A.Core.Settings
local TooltipLines = A.Core.TooltipLines

local HOUR = 3600
local DAY = 24 * HOUR

local TARGET = "forever-eu-pvp-alliance"
local HORDE = "forever-eu-pvp-horde"
local BLACKWATER = "forever-eu-pvp-blackwater"

-- The house colors of SPEC 7.5.4, written out so that a line pins its colors
-- by their bytes and not through the formatter that renders them.
local TITLE = "|cffff7d0aAuctiMogger Database|r"
local MUTED = "|cff99a1b3"
local DIM = "|cff666e80"
local GOOD = "|cff3dc773"
local WARN = "|cffffd100"
local HOT = "|cffff8040"
local BAD = "|cffff4040"
local RESET = "|r"

------------------------------------------------------------------- fixtures

--- A span block with its twelve fixed fields at `price`, plus `extra`.
---@param price integer copper
---@param extra table<string, any>|nil the optional ledger fields to add
---@return AmdbSpanBlock block
local function span(price, extra)
    ---@type AmdbSpanBlock
    local block = {
        n = 1,
        min = price,
        p05 = price,
        p25 = price,
        median = price,
        p75 = price,
        p95 = price,
        max = price,
        mad = 0,
        vwap = price,
        qtySum = 1,
        listingCount = 1,
    }
    if extra ~= nil then
        for name, value in pairs(extra) do
            block[name] = value
        end
    end
    return block
end

--- A record as the store hands one out: packed by the spec-side encoder and
--- read back by the real decoder.
---@param fields AmdbRecord the record's fields; rates as fractions, absent fields nil
---@return AmdbRecord record
local function packed(fields)
    local out, message = Record.decode(Encode.record(fields))
    assert.is_not_nil(out, message)
    return out
end

--- A record worth `price` and nothing else: one 30-day span, no group.
---@param price integer copper
---@return AmdbRecord record
local function worth(price)
    return packed({ spans = { d30 = span(price) } })
end

--- The example of SPEC 5.10 (its JSON twin), completed where it writes "...":
--- a commodity with every group. Its fortnight sold 443 units, 31.6 a day, so
--- the 1,234 units listed are 39 days of supply.
---@return AmdbRecord record
local function example()
    return packed({
        spans = {
            d1 = span(4010000, {
                min = 575000,
                p05 = 3800000,
                p25 = 4000000,
                p75 = 4100000,
                p95 = 4200000,
                max = 5200000,
                mad = 95000,
                vwap = 4032000,
                n = 58,
                qtySum = 58,
                listingCount = 58,
                saleRate = 3617 / 10000,
                qtySold = 9,
            }),
            d7 = span(4310000, {
                p05 = 3800000,
                p95 = 5120000,
                n = 214,
                qtySum = 214,
                listingCount = 214,
                saleRate = 3617 / 10000,
                qtySold = 210,
            }),
            d14 = span(4310000, {
                p05 = 3850000,
                p95 = 5120000,
                n = 300,
                qtySum = 600,
                listingCount = 300,
                saleRate = 3617 / 10000,
                saleRate180d = 3125 / 10000,
                daysSinceLastSale = 2,
                closuresSinceLastSale = 61,
                qtySold = 443,
            }),
        },
        live = { floor = 3805000, floorQty = 214, listed = 618, qtySum = 1234 },
        value7 = 4123500,
        samples7 = 214,
        soldMedian14 = 4021000,
        trendBp = -300,
        soldCount = 34,
        expiredCount = 60,
        supplyDays = 39,
        low30 = 3500000,
        lowRank30d = 8000,
        lows = {
            73,
            75,
            74,
            80,
            90,
            110,
            128,
            130,
            127,
            131,
            126,
            133,
            129,
            128,
            125,
            122,
            118,
            110,
            104,
            98,
            95,
            92,
            88,
            85,
            83,
            80,
            78,
            76,
            74,
            73,
        },
        vendorPrice = 125000,
        craft = { cost = 21000000, profit = 19000000, expectedProfit = 6840000 },
        deal = { rank = 12, scoreBp = 7100, discountBp = 3800 },
    })
end

--- A context against the example's market: 25 minutes after its auctions
--- stamp, 58 after its commodities stamp. `extra` overrides a field and
--- `cleared` removes one.
---@param extra table<string, any>|nil
---@param cleared string[]|nil
---@return AmdbTipContext context
local function context(extra, cleared)
    ---@type AmdbTipContext
    local base = {
        itemID = 194784,
        speciesID = nil,
        category = "commodity",
        target = TARGET,
        marketAge = 466,
        auctionsAge = 1500,
        commoditiesAge = 3500,
        feedVersion = 4,
        others = {},
        collapsed = false,
        shift = false,
        settings = Settings.defaults(),
    }
    if extra ~= nil then
        for name, value in pairs(extra) do
            base[name] = value
        end
    end
    if cleared ~= nil then
        for index = 1, #cleared do
            base[cleared[index]] = nil
        end
    end
    return base
end

--- The two other markets of the sample: 10% dearer and 5% cheaper than the
--- example's sold median.
---@return AmdbTipOther[] others
local function others()
    return {
        { target = HORDE, record = worth(4423100) },
        { target = BLACKWATER, record = worth(3820000) },
    }
end

--- The settings with one optional line set, the others on.
---@param key string the line to set
---@param on boolean
---@return AmdbSettings settings
local function withLine(key, on)
    local settings = Settings.defaults()
    assert.is_true(Settings.setLine(settings, key, on))
    return settings
end

--------------------------------------------------------------------- helpers

--- One row as a comparable line: `left | right`; a sparkline by its bar count.
---@param row AmdbTipRow
---@return string text
local function text(row)
    if row.bars ~= nil then
        return row.left .. " | [" .. #row.bars .. " bars]"
    end
    local right = row.right
    if right ~= nil then
        return row.left .. " | " .. right
    end
    return row.left .. " |"
end

---@param rows AmdbTipRow[]
---@return string[] lines
local function texts(rows)
    local lines = {}
    for index = 1, #rows do
        lines[index] = text(rows[index])
    end
    return lines
end

--- Whether some row has `left` as its label.
---@param rows AmdbTipRow[]
---@param left string
---@return boolean present
local function hasLeft(rows, left)
    for index = 1, #rows do
        if rows[index].left == left then
            return true
        end
    end
    return false
end

--- The labels of `rows`, in order.
---@param rows AmdbTipRow[]
---@return string[] labels
local function labels(rows)
    local out = {}
    for index = 1, #rows do
        out[index] = rows[index].left
    end
    return out
end

--- The standard level of the example, with every optional line on.
---@param extra table<string, any>|nil context overrides
---@return AmdbTipRow[] rows
local function standard(extra)
    return TooltipLines.build(example(), context(extra or { others = others() }))
end

-- The sample of SPEC 7.6, as the builder renders it.
local HEADER = TITLE .. " | " .. TARGET .. " · " .. MUTED .. "58m" .. RESET
local BUYOUT = "Min buyout | 380g 50s" .. DIM .. "  ×214" .. RESET
local VALUE = "Market value | 402g 10s" .. DIM .. "  sold" .. RESET .. BAD .. "  -3%" .. RESET
local RATE = "Sale rate (14d) | " .. WARN .. "36%" .. RESET .. DIM .. "  (34 of 94 sold)" .. RESET
local BASICS = { HEADER, BUYOUT, VALUE, RATE }

local STANDARD = {
    HEADER,
    BUYOUT,
    VALUE,
    RATE,
    "Sold | 31.6/day · supply 39d",
    "Last sale | 2d ago",
    "30-day low | 350g" .. DIM .. "  · 80% of days higher" .. RESET,
    "30 days | [30 bars]",
    "Crafting | 2100g → +1900g" .. DIM .. "  (exp. " .. RESET .. "+684g" .. DIM .. ")" .. RESET,
    "Deal | " .. GOOD .. "#12 · 38% under value · score 0.71" .. RESET,
    MUTED .. HORDE .. RESET .. " | 442g 31s  +10%",
    MUTED .. BLACKWATER .. RESET .. " | 382g  -5%",
}

local DETAIL = {
    HEADER,
    BUYOUT,
    VALUE,
    RATE,
    "Sold median (14d) | 402g 10s",
    "Value basis | 7-day listing value" .. DIM .. "  · 214 listings" .. RESET,
    "Listed | 618 listings" .. DIM .. "  · 1,234 units" .. RESET,
    "Vendor sell | 12g 50s",
    "Supply | 39 days" .. DIM .. "  (1,234 listed ÷ 31.6/day)" .. RESET,
    "Sale rate (180d) | " .. WARN .. "31%" .. RESET,
    "Closures since last sale | 61",
    "Snapshots | auctions 25m · commodities 58m ago",
    "d1   median 401g | p05 380g · p95 420g · 58 seen · 9 sold",
    "d7   median 431g | p05 380g · p95 512g · 214 seen · 210 sold",
    "d14  median 431g | p05 385g · p95 512g · 300 seen · 443 sold",
    DIM .. "194784 · commodity · feed v4" .. RESET .. " |",
}

-- Every optional line: its key in the settings module, the label its row
-- carries for the example, and how many rows the example has under it.
---@type { key: string, label: string, count: integer }[]
local TOGGLES = {
    { key = "sold", label = "Sold", count = 1 },
    { key = "lastSale", label = "Last sale", count = 1 },
    { key = "low", label = "30-day low", count = 1 },
    { key = "sparkline", label = "30 days", count = 1 },
    { key = "craft", label = "Crafting", count = 1 },
    { key = "deal", label = "Deal", count = 1 },
    { key = "markets", label = MUTED .. HORDE .. RESET, count = 2 },
}

-------------------------------------------------------------------- the module

describe("tooltip_lines", function()
    it("registers into the namespace and returns its table", function()
        local namespace = Loader.namespace()
        local chunk = assert(loadfile(Loader.ROOT .. "/Core/tooltip_lines.lua"))
        local module = chunk(Loader.ADDON, namespace)
        assert.equals(namespace.Core.TooltipLines, module)
    end)

    it("renders the standard sample of SPEC 7.6 line for line", function()
        assert.same(STANDARD, texts(standard()))
    end)

    it("renders the Shift sample of SPEC 7.6 line for line", function()
        local rows = TooltipLines.build(example(), context({ others = others(), shift = true }))
        assert.same(DETAIL, texts(rows))
    end)

    it("never answers an empty tooltip: a record with one span still has the basics", function()
        local rows = TooltipLines.build(worth(8000000), context())
        assert.same({
            HEADER,
            "Min buyout | " .. DIM .. "not listed" .. RESET,
            "Market value | 800g" .. DIM .. "  d30" .. RESET,
            "Sale rate (14d) | " .. DIM .. "no evidence" .. RESET,
        }, texts(rows))
    end)
end)

------------------------------------------------------------------- the header

describe("the header", function()
    it("carries the title, the market, and the age of the record's own snapshot", function()
        local rows = standard()
        assert.equals(TITLE, rows[1].left)
        assert.equals(TARGET .. " · " .. MUTED .. "58m" .. RESET, rows[1].right)
    end)

    it("reads the commodities stamp for a commodity and the auctions stamp for an item", function()
        local record = example()
        local item = context({ category = "item" })
        assert.equals(
            TITLE .. " | " .. TARGET .. " · " .. MUTED .. "25m" .. RESET,
            text(TooltipLines.build(record, item)[1])
        )
    end)

    it("reads the auctions stamp for a battle pet: a pet sells in the auctions", function()
        local pet = context({ category = "pet", speciesID = 2546 }, { "itemID" })
        assert.equals(
            TITLE .. " | " .. TARGET .. " · " .. MUTED .. "25m" .. RESET,
            text(TooltipLines.build(worth(2450000), pet)[1])
        )
    end)

    it("falls back to the market's own age when the header lacks the stamp", function()
        local record = example()
        local commodity = context(nil, { "commoditiesAge" })
        assert.equals(
            TITLE .. " | " .. TARGET .. " · " .. MUTED .. "7m" .. RESET,
            text(TooltipLines.build(record, commodity)[1])
        )
        local item = context({ category = "item" }, { "auctionsAge" })
        assert.equals(
            TITLE .. " | " .. TARGET .. " · " .. MUTED .. "7m" .. RESET,
            text(TooltipLines.build(record, item)[1])
        )
    end)

    it("colors the age by its tier: muted under 2 h, then warn, hot, and bad from 24 h", function()
        local record = example()
        local function badge(age)
            local rows = TooltipLines.build(record, context({ commoditiesAge = age }))
            return rows[1].right
        end
        assert.equals(TARGET .. " · " .. MUTED .. "58m" .. RESET, badge(3500))
        assert.equals(TARGET .. " · " .. MUTED .. "1h 59m" .. RESET, badge(2 * HOUR - 60))
        assert.equals(TARGET .. " · " .. WARN .. "2h 00m" .. RESET, badge(2 * HOUR))
        assert.equals(TARGET .. " · " .. HOT .. "12h 00m" .. RESET, badge(12 * HOUR))
        assert.equals(TARGET .. " · " .. BAD .. "1d" .. RESET, badge(DAY))
    end)

    it("reads a clock a little ahead of the stamp as no age at all", function()
        local rows = TooltipLines.build(example(), context({ commoditiesAge = -120 }))
        assert.equals(TARGET .. " · " .. MUTED .. "0m" .. RESET, rows[1].right)
    end)
end)

----------------------------------------------------------------- the basics

describe("Min buyout", function()
    it("shows the floor with the quantity at it", function()
        local rows = standard()
        assert.equals("Min buyout", rows[2].left)
        assert.equals("380g 50s" .. DIM .. "  ×214" .. RESET, rows[2].right)
    end)

    it("marks a floor at or under the vendor sell price in good", function()
        local record = packed({
            spans = { d1 = span(100) },
            live = { floor = 100, floorQty = 5, listed = 5 },
            vendorPrice = 100,
        })
        assert.equals(
            "1s" .. DIM .. "  ×5" .. RESET .. GOOD .. "  ≤ vendor" .. RESET,
            TooltipLines.build(record, context())[2].right
        )
    end)

    it("leaves the vendor mark off a floor above the vendor sell price", function()
        local record = packed({
            spans = { d1 = span(100) },
            live = { floor = 101, floorQty = 5, listed = 5 },
            vendorPrice = 100,
        })
        assert.equals("1s 1c" .. DIM .. "  ×5" .. RESET, TooltipLines.build(record, context())[2].right)
    end)

    it("says a listing was seen today and is gone", function()
        local record = packed({ spans = { d1 = span(2500) }, live = { seenFloor = 2500, seenQty = 20 } })
        assert.equals(
            DIM .. "none listed · seen today 25s ×20" .. RESET,
            TooltipLines.build(record, context())[2].right
        )
    end)

    it("says the item is not listed when nothing was seen either", function()
        assert.equals(DIM .. "not listed" .. RESET, TooltipLines.build(worth(8000000), context())[2].right)
    end)
end)

describe("Market value", function()
    it("names the sold median as the source, with the trend in its color", function()
        local rows = standard()
        assert.equals("Market value", rows[3].left)
        assert.equals("402g 10s" .. DIM .. "  sold" .. RESET .. BAD .. "  -3%" .. RESET, rows[3].right)
    end)

    it("names the listing value when nothing sold", function()
        local record = packed({ spans = { d1 = span(100) }, value7 = 80000, samples7 = 21, trendBp = 2500 })
        assert.equals(
            "8g" .. DIM .. "  listed" .. RESET .. GOOD .. "  +25%" .. RESET,
            TooltipLines.build(record, context())[3].right
        )
    end)

    it("names the span a fallback value comes from", function()
        assert.equals("800g" .. DIM .. "  d30" .. RESET, TooltipLines.build(worth(8000000), context())[3].right)
    end)

    it("colors a rising trend good and a flat one muted", function()
        local rising = packed({ spans = { d1 = span(100) }, value7 = 100, samples7 = 1, trendBp = 55934 })
        assert.equals(
            "1s" .. DIM .. "  listed" .. RESET .. GOOD .. "  +559%" .. RESET,
            TooltipLines.build(rising, context())[3].right
        )
        local flat = packed({ spans = { d1 = span(100) }, value7 = 100, samples7 = 1, trendBp = 0 })
        assert.equals(
            "1s" .. DIM .. "  listed" .. RESET .. MUTED .. "  0%" .. RESET,
            TooltipLines.build(flat, context())[3].right
        )
    end)

    it("shows the value alone when there is no trend", function()
        local record = packed({ spans = { d1 = span(100) }, value7 = 100, samples7 = 1 })
        assert.equals("1s" .. DIM .. "  listed" .. RESET, TooltipLines.build(record, context())[3].right)
    end)

    it("says the value is unknown when every median is zero", function()
        assert.equals(DIM .. "unknown" .. RESET, TooltipLines.build(worth(0), context())[3].right)
    end)
end)

describe("Sale rate", function()
    it("shows the 14-day rate in its tier color, with the closures behind it", function()
        local rows = standard()
        assert.equals("Sale rate (14d)", rows[4].left)
        assert.equals(WARN .. "36%" .. RESET .. DIM .. "  (34 of 94 sold)" .. RESET, rows[4].right)
    end)

    it("colors a high rate good", function()
        local record = packed({ spans = { d1 = span(100, { saleRate = 0.8889 }) }, soldCount = 8, expiredCount = 1 })
        assert.equals(
            GOOD .. "89%" .. RESET .. DIM .. "  (8 of 9 sold)" .. RESET,
            TooltipLines.build(record, context())[4].right
        )
    end)

    it("leaves the closures out when either count is absent", function()
        local record = packed({ spans = { d1 = span(100, { saleRate = 0.5 }) }, soldCount = 10 })
        assert.equals(GOOD .. "50%" .. RESET, TooltipLines.build(record, context())[4].right)
    end)

    it("colors a present zero rate bad: nothing sold as far as we know", function()
        local record = packed({ spans = { d1 = span(100, { saleRate = 0 }) }, soldCount = 0, expiredCount = 3 })
        assert.equals(
            BAD .. "0%" .. RESET .. DIM .. "  (0 of 3 sold)" .. RESET,
            TooltipLines.build(record, context())[4].right
        )
    end)

    it("shows the 180-day rate under its own name when there is no 14-day one", function()
        local record = packed({
            spans = { d1 = span(100, { saleRate180d = 0.75 }) },
            soldCount = 10,
            expiredCount = 90,
        })
        local rows = TooltipLines.build(record, context())
        assert.equals("Sale rate (180d)", rows[4].left)
        -- The counts describe the fortnight: they are not shown beside the
        -- longer window.
        assert.equals(GOOD .. "75%" .. RESET, rows[4].right)
    end)

    it("reads the rate off the first span that has one: d1 before d7", function()
        local record = packed({
            spans = { d1 = span(100, { saleRate = 0.9 }), d7 = span(100, { saleRate = 0.1 }) },
        })
        assert.equals(GOOD .. "90%" .. RESET, TooltipLines.build(record, context())[4].right)
    end)

    it("says there is no evidence when no span carries a rate", function()
        assert.equals(DIM .. "no evidence" .. RESET, TooltipLines.build(worth(8000000), context())[4].right)
    end)
end)

----------------------------------------------------------- the optional lines

describe("the optional lines", function()
    it("Sold: units a day and the days of supply", function()
        assert.equals("Sold | 31.6/day · supply 39d", text(standard()[5]))
    end)

    it("Sold: supply is unbounded when nothing sold and no figure exists", function()
        local record = packed({ spans = { d1 = span(100, { qtySold = 0 }) }, soldCount = 0 })
        assert.equals("Sold | 0/day · supply ∞", text(TooltipLines.build(record, context())[5]))
    end)

    it("Sold: a saturated supply figure reads the same way", function()
        local record = packed({ spans = { d14 = span(100, { qtySold = 140 }) }, supplyDays = 65535 })
        assert.equals("Sold | 10/day · supply ∞", text(TooltipLines.build(record, context())[5]))
    end)

    it("Sold: the per-day figure alone when there is no supply figure", function()
        local record = packed({ spans = { d7 = span(100, { qtySold = 35 }) } })
        assert.equals("Sold | 5/day", text(TooltipLines.build(record, context())[5]))
    end)

    it("Sold: left out without a sold figure, whatever else the record says", function()
        local record = packed({ spans = { d1 = span(100) }, supplyDays = 39 })
        local rows = TooltipLines.build(record, context())
        assert.is_false(hasLeft(rows, "Sold"))
        assert.same({ TITLE, "Min buyout", "Market value", "Sale rate (14d)" }, labels(rows))
    end)

    it("Last sale: today, a day ago, and further back", function()
        local function when(days)
            local record = packed({ spans = { d7 = span(100, { daysSinceLastSale = days }) } })
            local rows = TooltipLines.build(record, context())
            assert.equals("Last sale", rows[5].left)
            return rows[5].right
        end
        assert.equals("today", when(0))
        assert.equals("1d ago", when(1))
        assert.equals("2d ago", when(2))
        assert.equals("14d ago", when(14))
    end)

    it("Last sale: left out when the ledger never credited a sale", function()
        local rows = TooltipLines.build(worth(100), context())
        assert.is_false(hasLeft(rows, "Last sale"))
        assert.same({ TITLE, "Min buyout", "Market value", "Sale rate (14d)" }, labels(rows))
    end)

    it("30-day low: the low, and the share of days that sat higher", function()
        assert.equals("30-day low | 350g" .. DIM .. "  · 80% of days higher" .. RESET, text(standard()[7]))
    end)

    it("30-day low: the low alone without the rank", function()
        local record = packed({ spans = { d1 = span(100) }, value7 = 100, samples7 = 1, low30 = 600 })
        assert.equals("30-day low | 6s", text(TooltipLines.build(record, context())[5]))
    end)

    it("30-day low: left out without a low", function()
        assert.is_false(hasLeft(TooltipLines.build(worth(100), context()), "30-day low"))
    end)

    it("Sparkline: the caption, and the daily lows as bars, oldest first", function()
        local record = example()
        local rows = TooltipLines.build(record, context())
        assert.equals("30 days", rows[8].left)
        assert.is_nil(rows[8].right)
        -- The record's own series, not a copy: bars come from `lows` as decoded.
        assert.equals(true, rows[8].bars == record.lows)
    end)

    it("Sparkline: left out without lows, or with an empty series", function()
        assert.is_false(hasLeft(TooltipLines.build(worth(100), context()), "30 days"))
        local empty = packed({ spans = { d1 = span(100) }, value7 = 100, samples7 = 1, lows = {} })
        assert.is_false(hasLeft(TooltipLines.build(empty, context()), "30 days"))
    end)

    it("Crafting: the cost, the result and the expected result", function()
        assert.equals(
            "Crafting | 2100g → +1900g" .. DIM .. "  (exp. " .. RESET .. "+684g" .. DIM .. ")" .. RESET,
            text(standard()[9])
        )
    end)

    it("Crafting: a loss reads in bad, in both results", function()
        local record = packed({
            spans = { d1 = span(100) },
            craft = { cost = 400000, profit = -125000, expectedProfit = -25000 },
        })
        assert.equals(
            "Crafting | 40g → "
                .. BAD
                .. "-12g 50s"
                .. RESET
                .. DIM
                .. "  (exp. "
                .. RESET
                .. BAD
                .. "-2g 50s"
                .. RESET
                .. DIM
                .. ")"
                .. RESET,
            text(TooltipLines.build(record, context())[5])
        )
    end)

    it("Crafting: left out without a recipe", function()
        assert.is_false(hasLeft(TooltipLines.build(worth(100), context()), "Crafting"))
    end)

    it("Deal: the rank, the discount and the score, in good", function()
        assert.equals("Deal | " .. GOOD .. "#12 · 38% under value · score 0.71" .. RESET, text(standard()[10]))
    end)

    it("Deal: rounds the score to two digits", function()
        local low = packed({ spans = { d1 = span(100) }, deal = { rank = 87, scoreBp = 4200, discountBp = 500 } })
        assert.equals(
            "Deal | " .. GOOD .. "#87 · 5% under value · score 0.42" .. RESET,
            text(TooltipLines.build(low, context())[5])
        )
        local top = packed({ spans = { d1 = span(100) }, deal = { rank = 1, scoreBp = 9250, discountBp = 5000 } })
        assert.equals(
            "Deal | " .. GOOD .. "#1 · 50% under value · score 0.93" .. RESET,
            text(TooltipLines.build(top, context())[5])
        )
    end)

    it("Deal: left out for an item that is not a ranked deal", function()
        assert.is_false(hasLeft(TooltipLines.build(worth(100), context()), "Deal"))
    end)
end)

describe("the other markets", function()
    it("show one line per market that has the item, with the difference to the primary", function()
        local rows = standard()
        assert.same({ MUTED .. HORDE .. RESET, MUTED .. BLACKWATER .. RESET }, { rows[11].left, rows[12].left })
        assert.equals("442g 31s  +10%", rows[11].right)
        assert.equals("382g  -5%", rows[12].right)
    end)

    it("show the value alone when the primary market has none to compare with", function()
        local few = { others = { { target = HORDE, record = worth(455000) } } }
        assert.equals("45g 50s", TooltipLines.build(worth(0), context(few))[5].right)
    end)

    it("skip a market whose record has no value", function()
        local few =
            { others = { { target = HORDE, record = worth(0) }, { target = BLACKWATER, record = worth(3820000) } } }
        local rows = TooltipLines.build(example(), context(few))
        assert.equals(11, #rows)
        assert.equals(MUTED .. BLACKWATER .. RESET, rows[11].left)
    end)

    it("are absent when the primary is the only market", function()
        local rows = TooltipLines.build(example(), context())
        assert.same(STANDARD[1], text(rows[1]))
        assert.equals(10, #rows)
        assert.same({
            TITLE,
            "Min buyout",
            "Market value",
            "Sale rate (14d)",
            "Sold",
            "Last sale",
            "30-day low",
            "30 days",
            "Crafting",
            "Deal",
        }, labels(rows))
    end)
end)

describe("each toggle", function()
    it("covers every line key of the settings module, in its order", function()
        local keys = {}
        for index = 1, #TOGGLES do
            keys[index] = TOGGLES[index].key
        end
        assert.same(Settings.LINE_KEYS, keys)
    end)

    for _, toggle in ipairs(TOGGLES) do
        describe(toggle.key, function()
            it("shows the line while the line is on", function()
                local rows =
                    TooltipLines.build(example(), context({ others = others(), settings = withLine(toggle.key, true) }))
                assert.is_true(hasLeft(rows, toggle.label))
            end)

            it("leaves the line out once it is switched off, and nothing else", function()
                local rows = TooltipLines.build(
                    example(),
                    context({ others = others(), settings = withLine(toggle.key, false) })
                )
                assert.is_false(hasLeft(rows, toggle.label))
                -- Only that line is gone: the rest of the sample, the basics first.
                assert.equals(#STANDARD - toggle.count, #rows)
                assert.same(BASICS, { text(rows[1]), text(rows[2]), text(rows[3]), text(rows[4]) })
            end)
        end)
    end

    it("leaves every optional line out under the basic preset", function()
        local settings = Settings.defaults()
        assert.is_true(Settings.applyPreset(settings, "basic"))
        local rows = TooltipLines.build(example(), context({ others = others(), settings = settings }))
        assert.same(BASICS, texts(rows))
    end)
end)

--------------------------------------------------------- the Shift detail level

describe("the Shift detail level", function()
    it("keeps the basics and replaces every optional line", function()
        local rows = TooltipLines.build(example(), context({ others = others(), shift = true }))
        assert.same(DETAIL, texts(rows))
        for index = 1, #TOGGLES do
            assert.is_false(hasLeft(rows, TOGGLES[index].label))
        end
    end)

    it("does not read the line switches: every line off still shows the detail", function()
        local settings = Settings.defaults()
        assert.is_true(Settings.applyPreset(settings, "basic"))
        local rows = TooltipLines.build(example(), context({ others = others(), settings = settings, shift = true }))
        assert.same(DETAIL, texts(rows))
    end)

    it("leaves each detail line out when its data is absent", function()
        local rows = TooltipLines.build(worth(8000000), context({ shift = true }))
        assert.same({
            HEADER,
            "Min buyout | " .. DIM .. "not listed" .. RESET,
            "Market value | 800g" .. DIM .. "  d30" .. RESET,
            "Sale rate (14d) | " .. DIM .. "no evidence" .. RESET,
            "Snapshots | auctions 25m · commodities 58m ago",
            "d30  median 800g | p05 800g · p95 800g · 1 seen",
            DIM .. "194784 · commodity · feed v4" .. RESET .. " |",
        }, texts(rows))
    end)

    it("Listed: the listings alone when the snapshot counted no quantity", function()
        local record = packed({ spans = { d1 = span(100) }, live = { floor = 100, floorQty = 1, listed = 618 } })
        assert.equals("Listed | 618 listings", text(TooltipLines.build(record, context({ shift = true }))[5]))
    end)

    it("Supply: a saturated figure says so, and still shows the division", function()
        local record = packed({
            spans = { d14 = span(100, { qtySold = 14 }) },
            live = { floor = 100, floorQty = 1, listed = 3, qtySum = 65535 },
            supplyDays = 65535,
        })
        assert.equals(
            "Supply | at least 65,535 days" .. DIM .. "  (65,535 listed ÷ 1/day)" .. RESET,
            text(TooltipLines.build(record, context({ shift = true }))[6])
        )
    end)

    it("Supply: the figure alone when the division cannot be shown", function()
        local record = packed({ spans = { d1 = span(100) }, supplyDays = 5 })
        assert.equals("Supply | 5 days", text(TooltipLines.build(record, context({ shift = true }))[5]))
    end)

    it("Sale rate (180d): left out when the basic line already shows that rate", function()
        local record = packed({ spans = { d1 = span(100, { saleRate180d = 0.75 }) } })
        local rows = TooltipLines.build(record, context({ shift = true }))
        assert.equals("Sale rate (180d)", rows[4].left)
        -- Once: on the basic line, and not again among the detail lines.
        local shown = 0
        for index = 1, #rows do
            if rows[index].left == "Sale rate (180d)" then
                shown = shown + 1
            end
        end
        assert.equals(1, shown)
    end)

    it("Snapshots: each stamp that exists, and nothing without one", function()
        local record = worth(100)
        local both = context({ shift = true })
        assert.equals("Snapshots | auctions 25m · commodities 58m ago", text(TooltipLines.build(record, both)[5]))
        local auctionsOnly = context({ shift = true }, { "commoditiesAge" })
        assert.equals("Snapshots | auctions 25m ago", text(TooltipLines.build(record, auctionsOnly)[5]))
        local none = context({ shift = true }, { "auctionsAge", "commoditiesAge" })
        local rows = TooltipLines.build(record, none)
        assert.is_false(hasLeft(rows, "Snapshots"))
        -- Without the stamps the header falls back to the market's own age.
        assert.equals(TITLE .. " | " .. TARGET .. " · " .. MUTED .. "7m" .. RESET, text(rows[1]))
    end)

    it("shows the spans that exist, in wire order, and the sold units only where known", function()
        local record = packed({
            spans = {
                d30 = span(300),
                d1 = span(100, { qtySold = 9 }),
                d14 = span(200),
            },
        })
        -- Without the snapshot stamps, so the span rows follow the basics.
        local rows = TooltipLines.build(record, context({ shift = true }, { "auctionsAge", "commoditiesAge" }))
        assert.same({
            "d1   median 1s | p05 1s · p95 1s · 1 seen · 9 sold",
            "d14  median 2s | p05 2s · p95 2s · 1 seen",
            "d30  median 3s | p05 3s · p95 3s · 1 seen",
        }, { text(rows[5]), text(rows[6]), text(rows[7]) })
    end)

    it("ends with the identity: the id, its category, and the feed version", function()
        local rows = TooltipLines.build(example(), context({ shift = true }))
        assert.equals(DIM .. "194784 · commodity · feed v4" .. RESET, rows[#rows].left)
        assert.is_nil(rows[#rows].right)
    end)

    it("names the species id in the footer of a battle pet", function()
        local pet = context({ category = "pet", speciesID = 2546, shift = true }, { "itemID" })
        local rows = TooltipLines.build(worth(2450000), pet)
        assert.equals(DIM .. "2546 · pet · feed v4" .. RESET, rows[#rows].left)
    end)
end)

------------------------------------------------------------- a collapsed owner

describe("a collapsed owner tooltip", function()
    it("renders the three basics only, whatever else is on", function()
        assert.same(BASICS, texts(TooltipLines.build(example(), context({ others = others(), collapsed = true }))))
    end)

    it("stays at the basics with Shift held", function()
        local rows = TooltipLines.build(example(), context({ others = others(), collapsed = true, shift = true }))
        assert.same(BASICS, texts(rows))
    end)
end)
