-- Core/record.lua (addon/SPEC.md, sections 5.5 to 5.8): the record round trip,
-- every refusal the contract lists, and the values a record derives.
--
-- Records are written with the spec's own encoder (spec/helpers/encode.lua),
-- the twin of the tracker's writer, and read back with the decoder under test.
-- The refusals are the Rust reference decoder's (crates/infra/src/record.rs),
-- message for message.

local Loader = require("spec.helpers.loader")
local Encode = require("spec.helpers.encode")

local A = Loader.load(Loader.namespace(), "Core/pack.lua", "Core/record.lua")
local Pack = A.Core.Pack
local Record = A.Core.Record

local ALPHABET = Pack.ALPHABET
local MAX_INTEGER = Pack.MAX_INTEGER
local hasBit = Pack.hasBit

local OVERFLOW = "varint is longer than ten bytes or above 2^53 - 1"
local PADDING = "record padding is not at most four zero bits"
local NO_SPANS = "record has no span"
-- Single-symbol mutations of BASES that announce an empty group (see the
-- mutation sweep). The decoder refuses every one of them at the group's flag
-- word: two were accepted before the refusal existed, the rest were already
-- refused later in the record.
local EMPTY_REFUSALS = 26

local NAMES = { "d1", "d7", "d14", "d30" }

local GROUP_LIVE = 0x01
local GROUP_VALUE = 0x02
local GROUP_EVIDENCE = 0x04
local GROUP_POSITION = 0x08
local GROUP_VENDOR = 0x10
local GROUP_CRAFT = 0x20
local GROUP_DEAL = 0x40

-- Every field name the layout reads, in wire order.
local FIELDS = {
    "flags",
    "spans",
    "span.sflags",
    "span.n",
    "span.min",
    "span.p05",
    "span.p25",
    "span.median",
    "span.p75",
    "span.p95",
    "span.max",
    "span.mad",
    "span.vwap",
    "span.qtySum",
    "span.listingCount",
    "span.saleRate",
    "span.saleRate180d",
    "span.daysSinceLastSale",
    "span.closuresSinceLastSale",
    "span.qtySold",
    "live.lflags",
    "live.floor",
    "live.floorQty",
    "live.listed",
    "live.qtySum",
    "live.seenFloor",
    "live.seenQty",
    "value.vflags",
    "value.value7",
    "value.soldMedian14",
    "value.samples7",
    "value.trendBp",
    "evidence.eflags",
    "evidence.soldCount",
    "evidence.expiredCount",
    "evidence.supplyDays",
    "position.pflags",
    "position.low30",
    "position.lowRank30d",
    "position.lows",
    "vendor.vendorPrice",
    "craft.cost",
    "craft.profit",
    "craft.expectedProfit",
    "deal.rank",
    "deal.scoreBp",
    "deal.discountBp",
}

--- A present zero is written, never dropped, so a value is either nil or a
--- number: nothing here is ever `false`.
---@param value integer
---@param zero boolean whether the field holds a present zero instead
---@return integer
local function orZero(value, zero)
    if zero then
        return 0
    end
    return value
end

--- A span block whose optional fields are present exactly where `sflags` says,
--- holding zeros where present: a present zero must stay present.
---@param sflags integer
---@param salt integer separates the spans so a swapped one cannot round-trip
---@return AmdbSpanBlock
local function block(sflags, salt)
    local offset = salt * 1000
    local span = {
        n = 58 + offset,
        min = 575 + offset,
        p05 = 600 + offset,
        p25 = 1575 + offset,
        median = 15000 + offset,
        p75 = 19985 + offset,
        p95 = 50100 + offset,
        max = 50100 + offset,
        mad = 5100 + offset,
        vwap = 15912 + offset,
        qtySum = 1234 + offset,
        listingCount = 618 + offset,
    }
    if hasBit(sflags, 0x01) then
        span.saleRate = 8889 / 10000
    end
    if hasBit(sflags, 0x02) then
        span.saleRate180d = 5000 / 10000
    end
    if hasBit(sflags, 0x04) then
        span.daysSinceLastSale = 0
    end
    if hasBit(sflags, 0x08) then
        span.closuresSinceLastSale = 61
    end
    if hasBit(sflags, 0x10) then
        span.qtySold = 0
    end
    return span
end

--- The spans whose bits are set in `mask` (bit 0 = d1).
---@param mask integer
---@param sflags integer
---@return table<string, AmdbSpanBlock>
local function spans(mask, sflags)
    local out = {}
    for index = 1, 4 do
        if math.floor(mask / 2 ^ (index - 1)) % 2 == 1 then
            out[NAMES[index]] = block(sflags, index)
        end
    end
    return out
end

---@param mask integer
---@param sflags integer
---@return AmdbRecord
local function spansOnly(mask, sflags)
    return { spans = spans(mask, sflags) }
end

--- Every group and every flag, with distinct values so a swapped field cannot
--- round-trip unnoticed.
---@return AmdbRecord
local function kitchenSink()
    local d1 = block(31, 1)
    d1.n = 618
    d1.qtySold = 9
    local d7 = block(31, 2)
    d7.n = 1305
    d7.qtySold = 31
    local d14 = block(15, 3)
    d14.n = 2612
    d14.qtySold = 63
    local d30 = block(23, 4)
    d30.n = 5480
    d30.qtySold = 121
    return {
        spans = { d1 = d1, d7 = d7, d14 = d14, d30 = d30 },
        live = { floor = 3805000, floorQty = 214, listed = 618, qtySum = 1234, seenFloor = 3750000, seenQty = 5 },
        value7 = 5980000,
        samples7 = 1305,
        soldMedian14 = 6137000,
        trendBp = -300,
        soldCount = 34,
        expiredCount = 60,
        supplyDays = 274,
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
        craft = { cost = 5550000, profit = 280150, expectedProfit = 101358 },
        deal = { rank = 12, scoreBp = 7100, discountBp = 3800 },
    }
end

--- A record from its flag words. A group parameter is `false` when the group is
--- absent, otherwise its own flag word.
---@param live integer|false
---@param value integer|false
---@param evidence integer|false
---@param position integer|false
---@param vendor boolean
---@param craft boolean
---@param deal boolean
---@param zero boolean whether every present field holds a present zero
---@return AmdbRecord
local function fromFlags(live, value, evidence, position, vendor, craft, deal, zero)
    local record = { spans = spans(9, 17) }
    if live then
        local group = {}
        if hasBit(live, 0x01) then
            group.floor = orZero(3805000, zero)
            group.floorQty = orZero(214, zero)
            group.listed = orZero(618, zero)
        end
        if hasBit(live, 0x02) then
            group.qtySum = orZero(1234, zero)
        end
        if hasBit(live, 0x04) then
            group.seenFloor = orZero(3750000, zero)
            group.seenQty = orZero(5, zero)
        end
        record.live = group
    end
    if value then
        record.value7 = orZero(5980000, zero)
        record.samples7 = orZero(1305, zero)
        if hasBit(value, 0x01) then
            record.soldMedian14 = orZero(6137000, zero)
        end
        if hasBit(value, 0x02) then
            record.trendBp = orZero(-300, zero)
        end
    end
    if evidence then
        if hasBit(evidence, 0x01) then
            record.soldCount = orZero(34, zero)
        end
        if hasBit(evidence, 0x02) then
            record.expiredCount = orZero(60, zero)
        end
        if hasBit(evidence, 0x04) then
            record.supplyDays = orZero(274, zero)
        end
    end
    if position then
        if hasBit(position, 0x01) then
            record.low30 = orZero(3500000, zero)
        end
        if hasBit(position, 0x02) then
            record.lowRank30d = orZero(8000, zero)
        end
        if hasBit(position, 0x04) then
            if zero then
                record.lows = { 0, 0 }
            else
                record.lows = { 73, 129 }
            end
        end
    end
    if vendor then
        record.vendorPrice = orZero(125000, zero)
    end
    if craft then
        record.craft = {
            cost = orZero(5550000, zero),
            profit = orZero(280150, zero),
            expectedProfit = orZero(101358, zero),
        }
    end
    if deal then
        record.deal = { rank = orZero(12, zero), scoreBp = orZero(7100, zero), discountBp = orZero(3800, zero) }
    end
    return record
end

--- Encodes, decodes and compares: the word must come back unchanged and every
--- field must be equal, with no key missing on either side.
---@param record AmdbRecord
---@return AmdbRecord decoded
local function roundTrip(record)
    -- Plain asserts: this runs tens of thousands of times.
    local word = Encode.record(record)
    local out, message = Record.decode(word)
    assert(out ~= nil, message)
    assert(Encode.record(out) == word, "the word does not survive decoding: " .. word)
    assert(Encode.equal(record, out), "a field changed: " .. word)
    return out
end

---@param record AmdbRecord
---@return AmdbRecord decoded
local function decoded(record)
    local out, message = Record.decode(Encode.record(record))
    assert.is_not_nil(out, message)
    return out
end

---@param word string
---@param expected string the refusal message
local function refused(word, expected)
    local record, message = Record.decode(word)
    assert.is_nil(record)
    assert.equals(expected, message)
end

---@param actual integer[]
---@param expected integer[]
local function sameBytes(actual, expected)
    assert.equals(#expected, #actual)
    for index = 1, #expected do
        assert.equals(expected[index], actual[index])
    end
end

--- A record's opening: the given groups, then one all-zero d1 span.
---@param groups integer
---@return integer[]
local function opening(groups)
    local bytes = Encode.cat(groups, 0x01, 0x00)
    for _ = 1, 12 do
        bytes[#bytes + 1] = 0
    end
    return bytes
end

---@param count integer how many zero bytes
---@return integer[]
local function zeros(count)
    local bytes = {}
    for _ = 1, count do
        bytes[#bytes + 1] = 0
    end
    return bytes
end

--- The names a record's spans carry, in wire order.
---@param record AmdbRecord
---@return string[]
local function spanNames(record)
    local names = {}
    for index = 1, 4 do
        if record.spans[NAMES[index]] ~= nil then
            names[#names + 1] = NAMES[index]
        end
    end
    return names
end

--- How many zero bits a word pads its byte stream with.
---@param word string
---@return integer padded 0, 2 or 4
local function paddingOf(word)
    return #word * 6 - #Pack.fromText(word) * 8
end

--- The first `count` bytes of a record's stream, as a word.
---@param record AmdbRecord
---@param count integer
---@return string word
local function cut(record, count)
    local bytes = Pack.fromText(Encode.record(record))
    local part = {}
    for index = 1, count do
        part[index] = bytes[index]
    end
    return Encode.toText(part)
end

local KITCHEN = kitchenSink()
local KITCHEN_TEXT = Encode.record(KITCHEN)

describe("Record.decode: round trips", function()
    it("reads back a full record field for field", function()
        local record = roundTrip(KITCHEN)
        assert.equals(15000 + 1000, record.spans.d1.median)
        assert.equals(50100 + 2000, record.spans.d7.max)
        assert.equals(8889 / 10000, record.spans.d14.saleRate)
        assert.equals(5000 / 10000, record.spans.d14.saleRate180d)
        assert.equals(0, record.spans.d14.daysSinceLastSale)
        assert.equals(6137000, record.soldMedian14)
        assert.equals(-300, record.trendBp)
        assert.equals(3805000, record.live.floor)
        assert.equals(1234, record.live.qtySum)
        assert.equals(5, record.live.seenQty)
        assert.equals(274, record.supplyDays)
        assert.equals(8000, record.lowRank30d)
        assert.equals(280150, record.craft.profit)
        assert.equals(101358, record.craft.expectedProfit)
        assert.equals(3800, record.deal.discountBp)
        assert.equals(30, #record.lows)
        assert.equals(133, record.lows[12])
        -- The record carries the methods of section 5.8.
        assert.equals("table", type(getmetatable(record)))
        assert.equals("function", type(record.reference))
        assert.equals("function", type(record.supplyUnbounded))
    end)

    it("round-trips every group present and absent, with every inner flag", function()
        -- live without `listed` cannot carry `qtySum`: six flag words are valid.
        local LIVE = { 0, 1, 3, 4, 5, 7 }
        local count = 0
        for zero = 0, 1 do
            local zeroes = zero == 1
            for live = 0, #LIVE do
                for value = 0, 4 do
                    for evidence = 0, 8 do
                        for position = 0, 8 do
                            for vendor = 0, 1 do
                                for craft = 0, 1 do
                                    for deal = 0, 1 do
                                        roundTrip(
                                            fromFlags(
                                                live > 0 and LIVE[live] or false,
                                                value < 4 and value or false,
                                                evidence < 8 and evidence or false,
                                                position < 8 and position or false,
                                                vendor == 1,
                                                craft == 1,
                                                deal == 1,
                                                zeroes
                                            )
                                        )
                                        count = count + 1
                                    end
                                end
                            end
                        end
                    end
                end
            end
        end
        -- (6 + 1) x (3 + 1) x (7 + 1) x (7 + 1) x 2 x 2 x 2, once with values
        -- and once with present zeros.
        assert.equals(45360, count)
    end)

    it("round-trips every span set with every span flag combination", function()
        for mask = 1, 15 do
            local expected = {}
            for index = 1, 4 do
                if math.floor(mask / 2 ^ (index - 1)) % 2 == 1 then
                    expected[#expected + 1] = NAMES[index]
                end
            end
            for sflags = 0, 31 do
                local out = roundTrip(spansOnly(mask, sflags))
                assert.equals(table.concat(expected, ","), table.concat(spanNames(out), ","))
                for index = 1, #expected do
                    local span = out.spans[expected[index]]
                    -- Present exactly where flagged, a present zero where present.
                    assert.equals(hasBit(sflags, 0x01), span.saleRate ~= nil)
                    assert.equals(hasBit(sflags, 0x02), span.saleRate180d ~= nil)
                    assert.equals(hasBit(sflags, 0x04), span.daysSinceLastSale ~= nil)
                    assert.equals(hasBit(sflags, 0x08), span.closuresSinceLastSale ~= nil)
                    assert.equals(hasBit(sflags, 0x10), span.qtySold ~= nil)
                    if hasBit(sflags, 0x01) then
                        assert.equals(8889 / 10000, span.saleRate)
                    end
                    if hasBit(sflags, 0x04) then
                        assert.equals(0, span.daysSinceLastSale)
                    end
                    if hasBit(sflags, 0x08) then
                        assert.equals(61, span.closuresSinceLastSale)
                    end
                    if hasBit(sflags, 0x10) then
                        assert.equals(0, span.qtySold)
                    end
                end
            end
        end
    end)

    it("keeps both signs and a present zero in the signed fields", function()
        local trends = { -300, 0, 150, -9007199254740991, 9007199254740991 }
        local profits = { { -125000, -45000 }, { 0, 0 }, { 190000, 68400 }, { 280150, -101358 } }
        for _, trend in ipairs(trends) do
            for _, pair in ipairs(profits) do
                local out = roundTrip({
                    spans = spans(1, 0),
                    value7 = 4123500,
                    samples7 = 214,
                    trendBp = trend,
                    craft = { cost = 2100000, profit = pair[1], expectedProfit = pair[2] },
                })
                assert.equals(trend, out.trendBp)
                assert.equals(pair[1], out.craft.profit)
                assert.equals(pair[2], out.craft.expectedProfit)
            end
        end
    end)

    it("keeps every field at the contract's ceilings", function()
        local top = block(31, 0)
        for _, field in ipairs({
            "n",
            "min",
            "p05",
            "p25",
            "median",
            "p75",
            "p95",
            "max",
            "mad",
            "vwap",
            "qtySum",
            "listingCount",
            "daysSinceLastSale",
            "closuresSinceLastSale",
            "qtySold",
        }) do
            top[field] = MAX_INTEGER
        end
        local record = {
            spans = { d1 = top },
            live = {
                floor = MAX_INTEGER,
                floorQty = MAX_INTEGER,
                listed = MAX_INTEGER,
                qtySum = MAX_INTEGER,
                seenFloor = MAX_INTEGER,
                seenQty = MAX_INTEGER,
            },
            value7 = MAX_INTEGER,
            soldMedian14 = MAX_INTEGER,
            samples7 = MAX_INTEGER,
            trendBp = -MAX_INTEGER,
            soldCount = MAX_INTEGER,
            expiredCount = MAX_INTEGER,
            supplyDays = 65535,
            low30 = MAX_INTEGER,
            lowRank30d = 65535,
            lows = {},
            vendorPrice = MAX_INTEGER,
            craft = { cost = MAX_INTEGER, profit = MAX_INTEGER, expectedProfit = -MAX_INTEGER },
            deal = { rank = 4294967295, scoreBp = 4294967295, discountBp = 4294967295 },
        }
        for _ = 1, 30 do
            record.lows[#record.lows + 1] = 255
        end
        local out = roundTrip(record)
        assert.equals(MAX_INTEGER, out.spans.d1.vwap)
        assert.equals(65535, out.supplyDays)
        assert.equals(65535, out.lowRank30d)
        assert.equals(4294967295, out.deal.scoreBp)
        assert.equals(-MAX_INTEGER, out.trendBp)
        assert.equals(-MAX_INTEGER, out.craft.expectedProfit)
        assert.equals(30, #out.lows)
        assert.equals(255, out.lows[30])
    end)

    it("saturates a rate above u32, like the Rust decoder", function()
        -- The rate fields are read as u64 and saturate at u32::MAX.
        local cases = {
            { bp = 4294967294, expected = 4294967294 / 10000 },
            { bp = 4294967295, expected = 4294967295 / 10000 },
            { bp = 4294967296, expected = 4294967295 / 10000 },
            { bp = MAX_INTEGER, expected = 4294967295 / 10000 },
        }
        for _, case in ipairs(cases) do
            local bytes = opening(0)
            bytes[3] = 0x01
            local out, message = Record.decode(Encode.toText(Encode.cat(bytes, Encode.varint(case.bp))))
            assert.is_not_nil(out, message)
            assert.equals(case.expected, out.spans.d1.saleRate)
        end
    end)

    it("writes a present zero, never drops it", function()
        local zeroBlock = block(0, 1)
        zeroBlock.saleRate = 0
        zeroBlock.daysSinceLastSale = 0
        zeroBlock.closuresSinceLastSale = 0
        zeroBlock.qtySold = 0
        local record = {
            spans = { d1 = zeroBlock },
            live = { floor = 0, floorQty = 0, listed = 0, qtySum = 0, seenFloor = 0, seenQty = 0 },
            value7 = 1,
            soldMedian14 = 0,
            samples7 = 0,
            trendBp = 0,
            soldCount = 0,
            expiredCount = 0,
            supplyDays = 0,
            low30 = 0,
            lowRank30d = 0,
            lows = {},
            vendorPrice = 0,
            craft = { cost = 0, profit = 0, expectedProfit = 0 },
            deal = { rank = 0, scoreBp = 0, discountBp = 0 },
        }
        local out = roundTrip(record)
        local d1 = out.spans.d1
        assert.equals(0, d1.saleRate)
        assert.equals(0, d1.daysSinceLastSale)
        assert.equals(0, d1.closuresSinceLastSale)
        assert.equals(0, d1.qtySold)
        assert.is_nil(d1.saleRate180d)
        assert.equals(0, out.live.qtySum)
        assert.equals(0, out.live.seenQty)
        assert.equals(0, out.soldMedian14)
        assert.equals(0, out.trendBp)
        assert.equals(0, out.samples7)
        assert.equals(0, out.soldCount)
        assert.equals(0, out.expiredCount)
        assert.equals(0, out.supplyDays)
        assert.equals(0, out.low30)
        assert.equals(0, out.lowRank30d)
        assert.equals(0, #out.lows)
        assert.equals(0, out.vendorPrice)
        assert.equals(0, out.craft.profit)
        assert.equals(0, out.deal.rank)
        -- The group flags say so too: a present zero is not an absent group.
        local flags = Pack.fromText(Encode.record(record))[1]
        assert.is_true(hasBit(flags, GROUP_LIVE))
        assert.is_true(hasBit(flags, GROUP_VALUE))
        assert.is_true(hasBit(flags, GROUP_EVIDENCE))
        assert.is_true(hasBit(flags, GROUP_POSITION))
        assert.is_true(hasBit(flags, GROUP_VENDOR))
        assert.is_true(hasBit(flags, GROUP_CRAFT))
        assert.is_true(hasBit(flags, GROUP_DEAL))
    end)

    it("writes an empty live group as an empty group", function()
        local out = roundTrip({ spans = spans(1, 0), live = {} })
        assert.is_not_nil(out.live)
        assert.is_nil(out.live.floor)
        assert.is_nil(out.live.listed)
        assert.is_nil(out.live.seenFloor)
        -- The group costs exactly its one flag word.
        local bare = #Encode.recordBytes(spansOnly(1, 0))
        assert.equals(bare + 1, #Encode.recordBytes(out))
        local flags = Pack.fromText(Encode.record(out))[1]
        assert.is_true(hasBit(flags, GROUP_LIVE))
    end)
end)

describe("Record.decode: encoding", function()
    it("carries rates as basis points", function()
        for _, bp in ipairs({ 0, 1, 2500, 3333, 3617, 8889, 10000, 3125, 9411, 9565 }) do
            local set = spans(15, 0)
            for _, span in pairs(set) do
                span.saleRate = bp / 10000
                span.saleRate180d = bp / 10000
            end
            roundTrip({ spans = set })
            assert.equals(bp, Encode.basisPoints(bp / 10000))
        end
        -- The ranking computes 34 / 94; the wire carries it to four decimals.
        local set = spans(1, 0)
        set.d1.saleRate = 34 / 94
        local out = decoded({ spans = set })
        assert.equals(3617 / 10000, out.spans.d1.saleRate)
        assert.equals(3617, Encode.basisPoints(34 / 94))
        assert.equals(0, Encode.basisPoints(-0.5))
        assert.equals(1, Encode.basisPoints(0.000050001))
        assert.equals(0, Encode.basisPoints(0.000049999))
        assert.equals(25000, Encode.basisPoints(2.5))
        -- The wire field is a u32: a finite rate saturates at both ends...
        assert.equals(4294967295, Encode.basisPoints(1000000000))
        assert.equals(4294967295, Encode.basisPoints(429496.7295))
        assert.equals(4294967294, Encode.basisPoints(429496.7294))
        -- ...and a rate that is not a number is no evidence at all.
        assert.is_nil(Encode.basisPoints(math.huge))
        assert.is_nil(Encode.basisPoints(0 / 0))
        assert.is_nil(Encode.basisPoints(-math.huge))
        -- A rate that is not a number is no evidence: it stays absent.
        local nan = spans(1, 0)
        nan.d1.saleRate = 0 / 0
        nan.d1.saleRate180d = math.huge * 0
        assert.equals(Encode.record({ spans = spans(1, 0) }), Encode.record({ spans = nan }))
        assert.is_nil(decoded({ spans = nan }).spans.d1.saleRate)
    end)

    it("saturates like the tracker, never wrapping", function()
        local zero = block(0, 1)
        zero.min = -5
        zero.daysSinceLastSale = -2
        zero.closuresSinceLastSale = -9
        zero.qtySold = -1
        -- Nothing of this record round-trips to itself: that is the point.
        local out = decoded({
            spans = { d1 = zero },
            live = { floor = -1, floorQty = -2, listed = -3, qtySum = -4, seenFloor = -6, seenQty = -7 },
            value7 = -8,
            samples7 = -9,
            soldMedian14 = -10,
            soldCount = -11,
            expiredCount = -12,
            low30 = -13,
            vendorPrice = -14,
            craft = { cost = -15, profit = 7, expectedProfit = 7 },
        })
        local d1 = out.spans.d1
        assert.equals(0, d1.min)
        assert.equals(0, d1.daysSinceLastSale)
        assert.equals(0, d1.closuresSinceLastSale)
        assert.equals(0, d1.qtySold)
        assert.equals(0, out.live.floor)
        assert.equals(0, out.live.seenQty)
        assert.equals(0, out.value7)
        assert.equals(0, out.soldMedian14)
        assert.equals(0, out.vendorPrice)
        -- Signed fields keep their sign.
        assert.equals(0, out.craft.cost)
        assert.equals(7, out.craft.profit)
        -- Above the ceiling an unsigned field saturates there, a signed one at
        -- the same magnitude with its sign.
        local high = block(0, 1)
        high.n = 9223372036854775807
        high.max = MAX_INTEGER + 1
        high.qtySold = 9223372036854775807
        local top = decoded({
            spans = { d1 = high },
            value7 = 9223372036854775807,
            samples7 = MAX_INTEGER + 1,
            trendBp = 9223372036854775807,
        })
        assert.equals(MAX_INTEGER, top.spans.d1.n)
        assert.equals(MAX_INTEGER, top.spans.d1.qtySold)
        assert.equals(MAX_INTEGER, top.value7)
        assert.equals(MAX_INTEGER, top.samples7)
        assert.equals(MAX_INTEGER, top.trendBp)
        sameBytes(Encode.uint(-5), { 0 })
        sameBytes(Encode.uint(MAX_INTEGER + 1), Encode.varint(MAX_INTEGER))
        -- A signed field is written through the clamp, at either sign.
        local stream = Encode.recordBytes({
            spans = spans(1, 0),
            craft = { cost = 0, profit = MAX_INTEGER + 1, expectedProfit = -MAX_INTEGER - 2 },
        })
        -- flags, span mask, the span block and craft.cost come first: the two
        -- signed fields are the last sixteen bytes.
        local tail = {}
        for index = #stream - 15, #stream do
            tail[#tail + 1] = stream[index]
        end
        sameBytes(Encode.cat(Encode.zigzag(MAX_INTEGER), Encode.zigzag(-MAX_INTEGER)), tail)
    end)

    it("writes the wire layout of the contract", function()
        local record = {
            spans = {
                d7 = {
                    n = 1,
                    min = 2,
                    p05 = 3,
                    p25 = 4,
                    median = 5,
                    p75 = 6,
                    p95 = 7,
                    max = 8,
                    mad = 9,
                    vwap = 10,
                    qtySum = 11,
                    listingCount = 12,
                    saleRate = 0.25,
                    daysSinceLastSale = 0,
                    qtySold = 40,
                },
            },
            live = { floor = 100, floorQty = 5, listed = 7 },
            value7 = 900,
            soldMedian14 = 950,
            samples7 = 12,
            trendBp = -3,
            soldCount = 3,
            supplyDays = 4,
            low30 = 800,
            lows = { 1, 2 },
            vendorPrice = 50,
            craft = { cost = 600, profit = -60, expectedProfit = -20 },
            deal = { rank = 2, scoreBp = 7000, discountBp = 300 },
        }
        -- flags, span mask; d7: sflags (saleRate, daysSinceLastSale, qtySold),
        -- the twelve fixed fields, then the three optional ones in flag order.
        local expected = Encode.cat(
            { 0x7F, 0x02, 0x15 },
            { 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12 },
            Encode.varint(2500),
            { 0, 40 },
            -- live: lflags, floor, floorQty, listed
            { 0x01, 100, 5, 7 },
            -- value: vflags, value7, soldMedian14, samples7, trendBp
            { 0x03 },
            Encode.varint(900),
            Encode.varint(950),
            { 12 },
            Encode.zigzag(-3),
            -- evidence: eflags, soldCount, supplyDays
            { 0x05, 3, 4 },
            -- position: pflags, low30, lows count, lows
            { 0x05 },
            Encode.varint(800),
            { 2, 1, 2 },
            -- vendorPrice
            { 50 },
            -- craft: cost, profit, expectedProfit
            Encode.varint(600),
            Encode.zigzag(-60),
            Encode.zigzag(-20),
            -- deal: rank, scoreBp, discountBp
            { 2 },
            Encode.varint(7000),
            Encode.varint(300)
        )
        local word = Encode.record(record)
        sameBytes(Encode.recordBytes(record), expected)
        sameBytes(Pack.fromText(word), expected)
        assert.is_true(Encode.equal(record, decoded(record)))
    end)

    it("renders single alphabet words", function()
        local _, foreign = KITCHEN_TEXT:gsub("[^" .. ALPHABET .. "]", "")
        assert.equals(0, foreign)
        refused("", "record ended inside `flags`")
    end)

    it("tells every field apart", function()
        -- The comparison the round trips rely on must not be blind to any field.
        local variants = {
            function(record)
                record.value7 = record.value7 + 1
            end,
            function(record)
                record.soldCount = nil
            end,
            function(record)
                record.live.floor = record.live.floor + 1
            end,
            function(record)
                record.live.seenQty = nil
            end,
            function(record)
                record.spans.d7.median = 0
            end,
            function(record)
                record.spans.d30 = nil
            end,
            function(record)
                record.spans.d1.saleRate180d = 0.25
            end,
            function(record)
                record.lows[3] = 255
            end,
            function(record)
                record.lows[#record.lows] = nil
            end,
            function(record)
                record.craft.profit = -record.craft.profit
            end,
            function(record)
                record.deal.rank = 1
            end,
            function(record)
                record.trendBp = false
            end,
            function(record)
                record.extra = 1
            end,
        }
        assert.equals(13, #variants)
        for _, variant in ipairs(variants) do
            local copy = decoded(KITCHEN)
            assert.is_true(Encode.equal(KITCHEN, copy))
            variant(copy)
            assert.is_false(Encode.equal(KITCHEN, copy))
        end
        assert.is_true(Encode.equal(KITCHEN, KITCHEN))
    end)
end)

describe("Record.decode: malformed input", function()
    -- The kitchen sink; one bare span; every span with every span flag; every
    -- group with values; and one where everything present is a present zero,
    -- with an empty live group and no optional value field.
    local BASES = {
        KITCHEN_TEXT,
        Encode.record(spansOnly(1, 0)),
        Encode.record(spansOnly(15, 31)),
        Encode.record(fromFlags(7, 3, 7, 7, true, true, true, false)),
        Encode.record(fromFlags(0, 0, 0, 0, true, true, true, true)),
    }
    for _, base in ipairs(BASES) do
        local record, message = Record.decode(base)
        assert.is_not_nil(record, message)
    end

    it("refuses every proper prefix of a record", function()
        for _, base in ipairs(BASES) do
            for length = 0, #base - 1 do
                assert.is_nil((Record.decode(base:sub(1, length))))
            end
            local bytes = Pack.fromText(base)
            for length = 1, #bytes - 1 do
                local part = {}
                for index = 1, length do
                    part[index] = bytes[index]
                end
                assert.is_nil((Record.decode(Encode.toText(part))))
            end
        end
    end)

    it("handles every single-symbol mutation of a full record", function()
        local symbols = {}
        for index = 1, 64 do
            symbols[index] = ALPHABET:sub(index, index)
        end
        local rejected, changed, accepted, empty = 0, 0, 0, 0
        for number, base in ipairs(BASES) do
            local original = Record.decode(base)
            for position = 1, #base do
                local head = base:sub(1, position - 1)
                local tail = base:sub(position + 1)
                local replaced = base:sub(position, position)
                for index = 1, 64 do
                    local symbol = symbols[index]
                    local mutated = head .. symbol .. tail
                    local where = ("base %d, symbol %d at position %d"):format(number, index, position)
                    local record, message = Record.decode(mutated)
                    if record ~= nil then
                        accepted = accepted + 1
                        -- Every accepted word is its record's encoding.
                        assert.equals(mutated, Encode.record(record), where)
                        local same = Encode.equal(original, record)
                        if symbol == replaced then
                            assert(same, where)
                        else
                            -- A changed word is a changed record.
                            changed = changed + 1
                            assert(not same, where)
                        end
                    else
                        rejected = rejected + 1
                        -- The original symbol is accepted.
                        assert(symbol ~= replaced, where)
                        if message ~= nil and message:find("is zero in an announced group", 1, true) then
                            empty = empty + 1
                        end
                    end
                end
            end
        end
        -- The sweep must exercise both outcomes, and it must still meet the
        -- words that announce a group over a zero byte: the present-zero base
        -- allows one symbol to do it, and they are refused.
        assert.is_true(rejected > 0 and changed > 0 and accepted > 0)
        assert.equals(EMPTY_REFUSALS, empty)
    end)

    it("refuses an announced evidence or position group that holds nothing", function()
        -- `flags` announces a group whose flag word is zero. No encoder writes
        -- one (it leaves the group out instead), so the word is refused: every
        -- text a decoder accepts is an encoder's output and re-encodes to itself.
        local cases = {
            { groups = GROUP_EVIDENCE, tail = { 0x00 }, field = "evidence.eflags" },
            { groups = GROUP_EVIDENCE + GROUP_VENDOR, tail = { 0x00, 0x00 }, field = "evidence.eflags" },
            { groups = GROUP_POSITION, tail = { 0x00 }, field = "position.pflags" },
            { groups = GROUP_POSITION + GROUP_VENDOR, tail = { 0x00, 0x00 }, field = "position.pflags" },
        }
        for _, case in ipairs(cases) do
            local word = Encode.toText(Encode.cat(opening(case.groups), case.tail))
            local out, message = Record.decode(word)
            assert.is_nil(out)
            assert.equals("`" .. case.field .. "` is zero in an announced group", message)
        end
        -- One set flag is a group: a present zero sold count is still evidence.
        local word = Encode.toText(Encode.cat(opening(GROUP_EVIDENCE), { 0x01, 0x00 }))
        local present = assert(Record.decode(word))
        assert.equals(0, present.soldCount)
    end)

    it("refuses every text of up to three symbols, without throwing", function()
        local count = 0
        local function check(word)
            assert(Record.decode(word) == nil, "decodes: " .. word)
            count = count + 1
        end
        check("")
        for a = 1, 64 do
            local one = ALPHABET:sub(a, a)
            check(one)
            for b = 1, 64 do
                local two = one .. ALPHABET:sub(b, b)
                check(two)
                for c = 1, 64 do
                    check(two .. ALPHABET:sub(c, c))
                end
            end
        end
        -- The empty text, then 64 + 64^2 + 64^3: no text of three symbols
        -- holds a span block.
        assert.equals(266305, count)
    end)

    it("names the field a record ends inside, for every field of the layout", function()
        local named = {}
        local length = #Pack.fromText(KITCHEN_TEXT)
        for cutAt = 0, length - 1 do
            local _, message = Record.decode(cut(KITCHEN, cutAt))
            assert.equals("record ended inside", message:sub(1, 19), "cut at " .. cutAt)
            named[message:match("^record ended inside `([^`]+)`$")] = true
        end
        for _, field in ipairs(FIELDS) do
            assert.is_true(named[field] ~= nil, field)
        end
        local total = 0
        for field in pairs(named) do
            total = total + 1
            assert.is_not_nil(field)
        end
        assert.equals(#FIELDS, total)
    end)

    it("refuses a flag bit with no meaning, and qtySum without listed", function()
        local cases = {
            { bytes = { 0x80, 0x01 }, message = "`flags` sets undefined bits 0x80" },
            { bytes = { 0x00, 0x10 }, message = "`spans` sets undefined bits 0x10" },
            { bytes = { 0x00, 0x01, 0x20 }, message = "`span.sflags` sets undefined bits 0x20" },
            { bytes = Encode.cat(opening(GROUP_LIVE), 0x08), message = "`live.lflags` sets undefined bits 0x8" },
            { bytes = Encode.cat(opening(GROUP_VALUE), 0x04), message = "`value.vflags` sets undefined bits 0x4" },
            {
                bytes = Encode.cat(opening(GROUP_EVIDENCE), 0x08),
                message = "`evidence.eflags` sets undefined bits 0x8",
            },
            {
                bytes = Encode.cat(opening(GROUP_POSITION), 0x08),
                message = "`position.pflags` sets undefined bits 0x8",
            },
            -- `qtySum` qualifies the listing summary; on its own it is a flag
            -- without meaning, alone or next to `seen`.
            { bytes = Encode.cat(opening(GROUP_LIVE), 0x02), message = "`live.lflags` sets undefined bits 0x2" },
            { bytes = Encode.cat(opening(GROUP_LIVE), 0x06), message = "`live.lflags` sets undefined bits 0x2" },
        }
        for _, case in ipairs(cases) do
            refused(Encode.toText(case.bytes), case.message)
        end
        -- qtySum rides with a listing.
        local listed = Encode.toText(Encode.cat(opening(GROUP_LIVE), 0x03, 100, 5, 7, 9))
        local out, message = Record.decode(listed)
        assert.is_not_nil(out, message)
        assert.equals(9, out.live.qtySum)
    end)

    it("refuses a record without any span", function()
        -- The encoder writes what it is given; the decoder calls it malformed.
        refused(Encode.record({ spans = {} }), NO_SPANS)
        refused(Encode.toText({ 0x00, 0x00 }), NO_SPANS)
        refused(Encode.toText({ 0x7F, 0x00 }), NO_SPANS)
    end)

    it("refuses bytes after the last field", function()
        refused(Encode.toText(Encode.cat(opening(0), 0x00)), "1 byte(s) follow the last field")
        refused(Encode.toText(Encode.cat(opening(0), 0x00, 0x00)), "2 byte(s) follow the last field")
        refused(Encode.toText(Encode.cat(opening(GROUP_VENDOR), 0x50, 0x00)), "1 byte(s) follow the last field")
        -- A stray zero symbol after a complete word is six more bits: with the
        -- word's own padding that is a whole extra byte. Only after a word that
        -- had no padding do the six bits stand alone.
        local padded = {}
        for _, base in ipairs(BASES) do
            local bits = paddingOf(base)
            padded[bits] = (padded[bits] or 0) + 1
            if bits >= 2 then
                refused(base .. "0", "1 byte(s) follow the last field")
            else
                refused(base .. "0", PADDING)
            end
        end
        -- Both cases are exercised.
        assert.is_true((padded[0] or 0) > 0)
        assert.is_true((padded[2] or 0) + (padded[4] or 0) > 0)
        refused("0", PADDING)
    end)

    it("refuses wrong padding", function()
        -- 16 bytes leave four padding bits: the lowest bits of the last symbol.
        local word = Encode.toText(Encode.cat(opening(GROUP_VENDOR), 0x50))
        assert.equals(4, paddingOf(word))
        assert.is_not_nil((Record.decode(word)))
        refused(word:sub(1, -2) .. "1", PADDING)
        for _, base in ipairs(BASES) do
            local last = ALPHABET:find(base:sub(-1), 1, true) - 1
            if paddingOf(base) > 0 and last < 63 then
                -- Set the lowest bit: a padding bit on every padded word.
                refused(base:sub(1, -2) .. ALPHABET:sub(last + 2, last + 2), PADDING)
            end
        end
        -- A zero symbol appended to a word without padding is six stray bits.
        for _, base in ipairs(BASES) do
            if paddingOf(base) == 0 then
                refused(base .. "0", PADDING)
            end
        end
    end)

    it("refuses a character outside the alphabet", function()
        refused(
            KITCHEN_TEXT:sub(1, 3) .. "!" .. KITCHEN_TEXT:sub(5),
            "character 0x21 at offset 3 is not a record symbol"
        )
        local tail = "character 0x20 at offset " .. #KITCHEN_TEXT .. " is not a record symbol"
        refused(KITCHEN_TEXT .. " ", tail)
    end)

    it("refuses values above their field's range, and accepts the ceiling", function()
        local cases = {
            {
                bytes = Encode.cat(opening(GROUP_EVIDENCE), 0x04, Encode.varint(65536)),
                message = "`evidence.supplyDays` holds 65536, out of range",
            },
            {
                bytes = Encode.cat(opening(GROUP_POSITION), 0x02, Encode.varint(65536)),
                message = "`position.lowRank30d` holds 65536, out of range",
            },
            {
                bytes = Encode.cat(opening(GROUP_POSITION), 0x04, 31, zeros(31)),
                message = "`position.lows` holds 31, out of range",
            },
            {
                bytes = Encode.cat(opening(GROUP_POSITION), 0x04, Encode.varint(1099511627776)),
                message = "`position.lows` holds 1099511627776, out of range",
            },
            {
                bytes = Encode.cat(opening(GROUP_POSITION), 0x04, Encode.varint(MAX_INTEGER)),
                message = "`position.lows` holds 9007199254740991, out of range",
            },
            {
                bytes = Encode.cat(opening(GROUP_DEAL), Encode.varint(4294967296)),
                message = "`deal.rank` holds 4294967296, out of range",
            },
            {
                bytes = Encode.cat(opening(GROUP_DEAL), 5, Encode.varint(4294967296)),
                message = "`deal.scoreBp` holds 4294967296, out of range",
            },
            {
                bytes = Encode.cat(opening(GROUP_DEAL), 5, 6, Encode.varint(4294967296)),
                message = "`deal.discountBp` holds 4294967296, out of range",
            },
        }
        for _, case in ipairs(cases) do
            refused(Encode.toText(case.bytes), case.message)
        end
        -- The ceilings themselves are valid: a count is checked before its bytes.
        local accepted = {
            {
                bytes = Encode.cat(opening(GROUP_EVIDENCE), 0x07, Encode.varint(65535), 5, Encode.varint(65535)),
                read = function(record)
                    return record.supplyDays
                end,
            },
            {
                bytes = Encode.cat(opening(GROUP_POSITION), 0x02, Encode.varint(65535)),
                read = function(record)
                    return record.lowRank30d
                end,
            },
            {
                bytes = Encode.cat(
                    opening(GROUP_DEAL),
                    Encode.varint(4294967295),
                    Encode.varint(4294967295),
                    Encode.varint(4294967295)
                ),
                read = function(record)
                    return record.deal.rank .. "," .. record.deal.scoreBp .. "," .. record.deal.discountBp
                end,
                value = "4294967295,4294967295,4294967295",
            },
        }
        for _, case in ipairs(accepted) do
            local out, message = Record.decode(Encode.toText(case.bytes))
            assert.is_not_nil(out, message)
            assert.equals(case.value or 65535, case.read(out))
        end
        local out, message = Record.decode(Encode.toText(Encode.cat(opening(GROUP_POSITION), 0x04, 30, zeros(30))))
        assert.is_not_nil(out, message)
        assert.equals(30, #out.lows)
    end)

    it("refuses integers above 2^53 - 1 in magnitude", function()
        local cases = {
            Encode.cat(opening(GROUP_VENDOR), Encode.varint(MAX_INTEGER + 1)),
            Encode.cat(opening(GROUP_VENDOR), Encode.varint(18014398509481984)),
            Encode.cat(opening(GROUP_CRAFT), 100, Encode.zigzag(MAX_INTEGER + 1)),
            Encode.cat(opening(GROUP_CRAFT), 100, Encode.zigzag(MAX_INTEGER), Encode.zigzag(-9007199254740992)),
            Encode.cat(opening(GROUP_VALUE), 0x02, 1, 1, Encode.zigzag(-9007199254740992)),
        }
        for _, bytes in ipairs(cases) do
            refused(Encode.toText(bytes), OVERFLOW)
        end
        -- The magnitudes themselves are valid, both signs.
        local edges = {
            Encode.cat(opening(GROUP_VENDOR), Encode.varint(MAX_INTEGER)),
            Encode.cat(opening(GROUP_CRAFT), 100, Encode.zigzag(MAX_INTEGER), Encode.zigzag(-MAX_INTEGER)),
            Encode.cat(opening(GROUP_VALUE), 0x02, 1, 1, Encode.zigzag(-MAX_INTEGER)),
        }
        for _, bytes in ipairs(edges) do
            local out, message = Record.decode(Encode.toText(bytes))
            assert.is_not_nil(out, message)
        end
    end)

    it("refuses a non-minimal varint, and one longer than ten bytes", function()
        local cases = {
            { bytes = { 0x80, 0x00 }, message = "`flags` is a non-minimal varint" },
            { bytes = { 0x00, 0x80, 0x00 }, message = "`spans` is a non-minimal varint" },
            { bytes = { 0x00, 0x01, 0x80, 0x00 }, message = "`span.sflags` is a non-minimal varint" },
            {
                bytes = Encode.cat(opening(GROUP_CRAFT), 100, { 0x80, 0x00 }),
                message = "`craft.profit` is a non-minimal varint",
            },
            {
                bytes = Encode.cat(opening(GROUP_LIVE), 0x01, { 0x80, 0x00 }),
                message = "`live.floor` is a non-minimal varint",
            },
            {
                bytes = Encode.cat(opening(GROUP_LIVE), 0x01, 100, { 0xFF, 0x00 }),
                message = "`live.floorQty` is a non-minimal varint",
            },
        }
        for _, case in ipairs(cases) do
            refused(Encode.toText(case.bytes), case.message)
        end
        local long = { 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00 }
        refused(Encode.toText(Encode.cat(opening(GROUP_VENDOR), long)), OVERFLOW)
        refused(Encode.toText(Encode.cat(opening(GROUP_CRAFT), 100, long)), OVERFLOW)
        -- A varint cut short names its field instead.
        refused(Encode.toText(Encode.cat(opening(GROUP_VENDOR), 0x80)), "record ended inside `vendor.vendorPrice`")
    end)

    it("refuses a truncated lows series", function()
        -- A count of three with one byte left.
        local message = "record ended inside `position.lows`"
        refused(Encode.toText(Encode.cat(opening(GROUP_POSITION), 0x04, 0x03, 0x80)), message)
        refused(Encode.toText(Encode.cat(opening(GROUP_POSITION), 0x04, 0x02, 0x7F)), message)
        -- A count of zero is a present, empty sparkline.
        local out, why = Record.decode(Encode.toText(Encode.cat(opening(GROUP_POSITION), 0x04, 0x00)))
        assert.is_not_nil(out, why)
        assert.equals(0, #out.lows)
    end)

    it("refuses input that is not a record string, without throwing", function()
        local function refusal(value)
            local record, message = Record.decode(value)
            assert.is_nil(record)
            assert.equals("record text is " .. type(value) .. ", not a string", message)
        end
        refusal(nil)
        refusal(12345)
        refusal(true)
        refusal({})
        -- No arguments at all.
        local record, message = Record.decode()
        assert.is_nil(record)
        assert.equals("record text is nil, not a string", message)
    end)
end)

describe("AmdbRecord: derived values (section 5.8)", function()
    -- Four spans with distinct, rising medians (the fixture salts each span).
    local D1, D7, D14, D30 = block(0, 1).median, block(0, 2).median, block(0, 3).median, block(0, 4).median
    assert.equals(16000, D1)
    assert.equals(17000, D7)
    assert.equals(18000, D14)
    assert.equals(19000, D30)

    --- The same spans with no usable value group: the reference falls back.
    ---@param mask integer
    ---@return AmdbRecord
    local function noValue(mask)
        return decoded({ spans = spans(mask, 0), value7 = 0, samples7 = 1 })
    end

    it("reference() prefers sold, then listed, then d7, d14, d30, d1", function()
        local cases = {
            {
                name = "the sold median first",
                record = { soldMedian14 = 6137000, value7 = 5980000, samples7 = 1305 },
                value = 6137000,
                source = "sold",
            },
            {
                name = "a present-zero sold median is skipped",
                record = { soldMedian14 = 0, value7 = 7030, samples7 = 188 },
                value = 7030,
                source = "listed",
            },
            {
                name = "a present-zero listing value falls back to the spans",
                record = { value7 = 0, samples7 = 188 },
                value = D7,
                source = "d7",
            },
            {
                name = "no value group at all",
                record = {},
                value = D7,
                source = "d7",
            },
        }
        for _, case in ipairs(cases) do
            local record = decoded({
                spans = spans(15, 0),
                value7 = case.record.value7,
                samples7 = case.record.samples7,
                soldMedian14 = case.record.soldMedian14,
            })
            assert.equals(case.value, record:reference(), case.name)
            assert.equals(case.source, record:valueSource(), case.name)
        end
        -- The span order: d7, d14, d30, d1.
        assert.equals(D14, noValue(12):reference())
        assert.equals("d14", noValue(12):valueSource())
        assert.equals(D30, noValue(8):reference())
        assert.equals("d30", noValue(8):valueSource())
        assert.equals(D1, noValue(1):reference())
        assert.equals("d1", noValue(1):valueSource())
        -- A zero median is skipped for the next span; all zero is no value.
        local set = spans(15, 0)
        set.d7.median = 0
        local record = decoded({ spans = set, value7 = 0, samples7 = 1 })
        assert.equals(D14, record:reference())
        assert.equals("d14", record:valueSource())
        set.d14.median = 0
        record = decoded({ spans = set, value7 = 0, samples7 = 1 })
        assert.equals(D30, record:reference())
        local flat = spans(15, 0)
        for _, span in pairs(flat) do
            span.median = 0
        end
        record = decoded({ spans = flat })
        assert.is_nil(record:reference())
        assert.is_nil(record:valueSource())
    end)

    it("saleRate14() reads d1, then d7", function()
        assert.equals(8889 / 10000, decoded({ spans = spans(15, 3) }):saleRate14())
        local set = spans(15, 0)
        set.d7.saleRate = 2500 / 10000
        set.d14.saleRate = 9900 / 10000
        assert.equals(2500 / 10000, decoded({ spans = set }):saleRate14())
        -- d14 and d30 are not consulted for a 14-day rate.
        assert.is_nil(decoded({ spans = spans(12, 1) }):saleRate14())
        -- A present zero is a rate of zero, not an absent one.
        local zero = spans(1, 1)
        zero.d1.saleRate = 0
        assert.equals(0, decoded({ spans = zero }):saleRate14())
        assert.is_nil(decoded({ spans = spans(15, 0) }):saleRate14())
    end)

    it("saleRate180d(), daysSinceLastSale() and closuresSinceLastSale() read the first present span", function()
        local record = decoded({ spans = spans(15, 30) })
        assert.equals(5000 / 10000, record:saleRate180d())
        assert.equals(0, record:daysSinceLastSale())
        assert.equals(61, record:closuresSinceLastSale())

        local set = spans(8, 0)
        set.d30.saleRate180d = 9411 / 10000
        set.d30.daysSinceLastSale = 9
        set.d30.closuresSinceLastSale = 3
        record = decoded({ spans = set })
        assert.equals(9411 / 10000, record:saleRate180d())
        assert.equals(9, record:daysSinceLastSale())
        assert.equals(3, record:closuresSinceLastSale())

        -- A later span fills in what an earlier one lacks.
        set = spans(15, 0)
        set.d14.saleRate180d = 1 / 10000
        set.d14.daysSinceLastSale = 0
        record = decoded({ spans = set })
        assert.equals(1 / 10000, record:saleRate180d())
        assert.equals(0, record:daysSinceLastSale())
        assert.is_nil(record:closuresSinceLastSale())

        record = decoded({ spans = spans(15, 0) })
        assert.is_nil(record:saleRate180d())
        assert.is_nil(record:daysSinceLastSale())
        assert.is_nil(record:closuresSinceLastSale())
    end)

    it("soldPerDay() divides d14 by 14, else d7 by 7, else reads d1", function()
        local set = spans(15, 0)
        set.d1.qtySold = 9
        set.d7.qtySold = 31
        set.d14.qtySold = 63
        set.d30.qtySold = 121
        assert.equals(63 / 14, decoded({ spans = set }):soldPerDay())
        set.d14.qtySold = nil
        assert.equals(31 / 7, decoded({ spans = set }):soldPerDay())
        set.d7.qtySold = nil
        assert.equals(9, decoded({ spans = set }):soldPerDay())
        -- d30 alone is not a rate; a present zero is a rate of zero.
        set.d1.qtySold = nil
        assert.is_nil(decoded({ spans = set }):soldPerDay())
        set.d1.qtySold = 0
        assert.equals(0, decoded({ spans = set }):soldPerDay())
        assert.equals(0 / 14, decoded({ spans = spans(15, 16) }):soldPerDay())
        assert.is_nil(decoded({ spans = spans(15, 0) }):soldPerDay())
    end)

    it("atVendorFloor() is true when the live floor is at or under the vendor price", function()
        local function atFloor(record)
            record.spans = spans(1, 0)
            return decoded(record):atVendorFloor()
        end
        --- A listing summary: `listed` carries all three fields.
        ---@param floor integer
        ---@return AmdbLive
        local function listing(floor)
            return { floor = floor, floorQty = 1, listed = 1 }
        end
        assert.is_true(atFloor({ vendorPrice = 125000, live = listing(15000) }))
        assert.is_true(atFloor({ vendorPrice = 125000, live = listing(125000) }))
        assert.is_true(atFloor({ vendorPrice = 0, live = listing(0) }))
        assert.is_false(atFloor({ vendorPrice = 125000, live = listing(125001) }))
        assert.is_false(atFloor({ live = listing(15000) }))
        -- Seen today and gone, or not listed: no floor to compare.
        assert.is_false(atFloor({ vendorPrice = 125000, live = { seenFloor = 15000, seenQty = 1 } }))
        assert.is_false(atFloor({ vendorPrice = 125000, live = {} }))
        assert.is_false(atFloor({ vendorPrice = 125000 }))
    end)

    it("supplyUnbounded() is true when supplyDays is absent while soldCount is a present zero", function()
        assert.is_true(decoded({ spans = spans(1, 0), soldCount = 0 }):supplyUnbounded())
        assert.is_false(decoded({ spans = spans(1, 0), soldCount = 0, supplyDays = 5 }):supplyUnbounded())
        assert.is_false(decoded({ spans = spans(1, 0), soldCount = 3 }):supplyUnbounded())
        assert.is_false(decoded({ spans = spans(1, 0) }):supplyUnbounded())
        -- 65535 is "at least that many days", which the caller shows the same
        -- way; this method answers the absent case of section 5.7 only.
        assert.is_false(decoded({ spans = spans(1, 0), soldCount = 0, supplyDays = 65535 }):supplyUnbounded())
    end)
end)
