-- Record codec of the database feed (addon/SPEC.md, section 5.5): one printable
-- word in, one `AmdbRecord` (section 7.3.1) out, with the values section 5.8
-- derives from it.
--
-- `Record.decode` mirrors the Rust reference decoder statement for statement
-- (crates/infra/src/record.rs, `decode`): it refuses exactly what section 5.4
-- calls malformed and nothing an encoder can write, and it reports the same
-- `PackError` words, so both decoders log the same line for the same bad
-- record. It never throws: a malformed record is `nil` plus the message,
-- because a carrier file is untrusted input that must not fail the login.
--
-- An absent field is `nil`, never `false` and never 0 (section 5.7); a present
-- zero stays a present zero. `saleRate` and `saleRate180d` are fractions,
-- `bp / 10000`.
--
-- Pure Lua: standard library only, no game global.

local _, A = ...
A.Core = A.Core or {}

local Pack = A.Core.Pack

-- Group presence, in the order the groups follow the spans.
local GROUP_LIVE = 0x01
local GROUP_VALUE = 0x02
local GROUP_EVIDENCE = 0x04
local GROUP_POSITION = 0x08
local GROUP_VENDOR = 0x10
local GROUP_CRAFT = 0x20
local GROUP_DEAL = 0x40
local GROUP_MASK = 0x7F

-- The spans in wire order, each with its presence bit.
local SPAN_NAMES = { "d1", "d7", "d14", "d30" }
local SPAN_BITS = { 0x01, 0x02, 0x04, 0x08 }
local SPAN_MASK = 0x0F

local SFLAG_SALE_RATE = 0x01
local SFLAG_SALE_RATE_180D = 0x02
local SFLAG_DAYS_SINCE_LAST_SALE = 0x04
local SFLAG_CLOSURES_SINCE_LAST_SALE = 0x08
local SFLAG_QTY_SOLD = 0x10
local SFLAG_MASK = 0x1F

local LFLAG_LISTED = 0x01
local LFLAG_QTY_SUM = 0x02
local LFLAG_SEEN = 0x04
local LFLAG_DAYS_LISTED = 0x08
local LFLAG_P05 = 0x10
local LFLAG_MASK = 0x1F

local VFLAG_SOLD_MEDIAN14 = 0x01
local VFLAG_TREND = 0x02
local VFLAG_MASK = 0x03

local EFLAG_SOLD_COUNT = 0x01
local EFLAG_EXPIRED_COUNT = 0x02
local EFLAG_SUPPLY_DAYS = 0x04
local EFLAG_MASK = 0x07

local PFLAG_LOW30 = 0x01
local PFLAG_LOW_RANK = 0x02
local PFLAG_LOWS = 0x04
local PFLAG_MASK = 0x07

-- The `lows` sparkline covers the previous 30 days, one byte each. A longer
-- count cannot be real data, so it is refused before anything is read.
local MAX_LOWS = 30
local MAX_U16 = 65535
local MAX_U32 = 4294967295

local NO_SPANS = "record has no span"

--- The message of a group announced in `flags` whose flag word is zero: the
--- Rust `PackError::EmptyGroup` text.
local EMPTY_GROUP = "`%s` is zero in an announced group"

local hasBit = Pack.hasBit

--- The span names `reference()` falls back through when it has neither a sold
--- median nor a listing value: d7 first, the one-day median last (section 5.8).
local REFERENCE_SPANS = { "d7", "d14", "d30", "d1" }

---@class AmdbSpanBlock
---@field n integer samples: unique auctions folded into the window
---@field min integer copper: the cheapest unit price in the window
---@field p05 integer copper: 5th percentile unit price
---@field p25 integer copper: 25th percentile unit price
---@field median integer copper: floor-even median unit price
---@field p75 integer copper: 75th percentile unit price
---@field p95 integer copper: 95th percentile unit price
---@field max integer copper: the most expensive unit price in the window
---@field mad integer copper: floor-even median absolute deviation
---@field vwap integer copper: volume-weighted average price
---@field qtySum integer units listed across the window's auctions
---@field listingCount integer unique auctions folded into the window
---@field saleRate number|nil fraction of eligible listings sold, `bp / 10000`
---@field saleRate180d number|nil the same estimator over 180 days, `bp / 10000`
---@field daysSinceLastSale integer|nil whole days since the last credited sale
---@field closuresSinceLastSale integer|nil net listings closed since that sale day
---@field qtySold integer|nil units sold in the span

---@class AmdbLive
---@field floor integer|nil copper: the cheapest unit price listed now
---@field floorQty integer|nil units listed at exactly the floor
---@field listed integer|nil listings in the snapshot
---@field qtySum integer|nil units listed in total
---@field p05 integer|nil copper: 5th percentile of the unit prices in the newest snapshot, nearest rank
---@field seenFloor integer|nil copper: today's cheapest listing that is gone
---@field seenQty integer|nil units of that listing
---@field daysSinceListed integer|nil whole days since the item was last seen listed; 0 = today

---@class AmdbCraft
---@field cost integer copper: the reagent cost per make
---@field profit integer copper, signed: output value minus house cut and cost
---@field expectedProfit integer copper, signed: profit times the sale rate

---@class AmdbDeal
---@field rank integer 1-based rank by score in the category
---@field scoreBp integer the ranking score, in bp
---@field discountBp integer `1 - floor / reference`, in bp

---@class AmdbRecord
---@field spans table<string, AmdbSpanBlock> d1/d7/d14/d30 -> the v3 span block
---@field live AmdbLive|nil              -- floor, floorQty, listed, qtySum, p05, seenFloor, seenQty, daysSinceListed
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
local AmdbRecord = {}
AmdbRecord.__index = AmdbRecord

---@class AmdbRecordCodec
local Record = {}
A.Core.Record = Record

------------------------------------------------------------------- reading

---@param bp integer a rate in basis points, as read off the wire
---@return number rate the fraction the span block carries
local function rateFromBp(bp)
    -- The field saturates at `u32::MAX` on the wire, so a larger code reads
    -- as that ceiling rather than wrapping.
    if bp > MAX_U32 then
        bp = MAX_U32
    end
    return bp / 10000
end

---@param reader AmdbReader
---@return AmdbSpanBlock block
local function readSpan(reader)
    local sflags = reader:flag("span.sflags", SFLAG_MASK)
    local block = {
        n = reader:varint("span.n"),
        min = reader:varint("span.min"),
        p05 = reader:varint("span.p05"),
        p25 = reader:varint("span.p25"),
        median = reader:varint("span.median"),
        p75 = reader:varint("span.p75"),
        p95 = reader:varint("span.p95"),
        max = reader:varint("span.max"),
        mad = reader:varint("span.mad"),
        vwap = reader:varint("span.vwap"),
        qtySum = reader:varint("span.qtySum"),
        listingCount = reader:varint("span.listingCount"),
    }
    if hasBit(sflags, SFLAG_SALE_RATE) then
        block.saleRate = rateFromBp(reader:varint("span.saleRate"))
    end
    if hasBit(sflags, SFLAG_SALE_RATE_180D) then
        block.saleRate180d = rateFromBp(reader:varint("span.saleRate180d"))
    end
    if hasBit(sflags, SFLAG_DAYS_SINCE_LAST_SALE) then
        block.daysSinceLastSale = reader:varint("span.daysSinceLastSale")
    end
    if hasBit(sflags, SFLAG_CLOSURES_SINCE_LAST_SALE) then
        block.closuresSinceLastSale = reader:varint("span.closuresSinceLastSale")
    end
    if hasBit(sflags, SFLAG_QTY_SOLD) then
        block.qtySold = reader:varint("span.qtySold")
    end
    return block
end

---@param reader AmdbReader
---@return AmdbLive live
local function readLive(reader)
    local lflags = reader:flag("live.lflags", LFLAG_MASK)
    -- `qtySum` and `p05` describe the listing summary: without `listed` they
    -- read nothing.
    if hasBit(lflags, LFLAG_QTY_SUM) and not hasBit(lflags, LFLAG_LISTED) then
        error(Pack.unknownFlagMessage("live.lflags", LFLAG_QTY_SUM), 0)
    end
    if hasBit(lflags, LFLAG_P05) and not hasBit(lflags, LFLAG_LISTED) then
        error(Pack.unknownFlagMessage("live.lflags", LFLAG_P05), 0)
    end
    local live = {}
    if hasBit(lflags, LFLAG_LISTED) then
        local floor = reader:varint("live.floor")
        local floorQty = reader:varint("live.floorQty")
        local listed = reader:varint("live.listed")
        local qtySum
        if hasBit(lflags, LFLAG_QTY_SUM) then
            qtySum = reader:varint("live.qtySum")
        end
        local p05
        if hasBit(lflags, LFLAG_P05) then
            p05 = reader:varint("live.p05")
        end
        live.floor = floor
        live.floorQty = floorQty
        live.listed = listed
        live.qtySum = qtySum
        live.p05 = p05
    end
    if hasBit(lflags, LFLAG_SEEN) then
        live.seenFloor = reader:varint("live.seenFloor")
        live.seenQty = reader:varint("live.seenQty")
    end
    if hasBit(lflags, LFLAG_DAYS_LISTED) then
        live.daysSinceListed = reader:uint("live.daysSinceListed", MAX_U16)
    end
    return live
end

---@param reader AmdbReader
---@return integer value7
---@return integer|nil soldMedian14
---@return integer samples7
---@return integer|nil trendBp
local function readValue(reader)
    local vflags = reader:flag("value.vflags", VFLAG_MASK)
    local value7 = reader:varint("value.value7")
    local soldMedian14
    if hasBit(vflags, VFLAG_SOLD_MEDIAN14) then
        soldMedian14 = reader:varint("value.soldMedian14")
    end
    local samples7 = reader:varint("value.samples7")
    local trendBp
    if hasBit(vflags, VFLAG_TREND) then
        trendBp = reader:zigzag("value.trendBp")
    end
    return value7, soldMedian14, samples7, trendBp
end

---@param reader AmdbReader
---@return integer|nil soldCount
---@return integer|nil expiredCount
---@return integer|nil supplyDays
local function readEvidence(reader)
    local eflags = reader:flag("evidence.eflags", EFLAG_MASK)
    if eflags == 0 then
        -- The encoder leaves the whole group out when nothing is set, so a
        -- zero flag word cannot be its output.
        error(EMPTY_GROUP:format("evidence.eflags"), 0)
    end
    local soldCount
    if hasBit(eflags, EFLAG_SOLD_COUNT) then
        soldCount = reader:varint("evidence.soldCount")
    end
    local expiredCount
    if hasBit(eflags, EFLAG_EXPIRED_COUNT) then
        expiredCount = reader:varint("evidence.expiredCount")
    end
    local supplyDays
    if hasBit(eflags, EFLAG_SUPPLY_DAYS) then
        supplyDays = reader:uint("evidence.supplyDays", MAX_U16)
    end
    return soldCount, expiredCount, supplyDays
end

---@param reader AmdbReader
---@return integer|nil low30
---@return integer|nil lowRank30d
---@return integer[]|nil lows
local function readPosition(reader)
    local pflags = reader:flag("position.pflags", PFLAG_MASK)
    if pflags == 0 then
        -- As the evidence group: the encoder writes no empty position group.
        error(EMPTY_GROUP:format("position.pflags"), 0)
    end
    local low30
    if hasBit(pflags, PFLAG_LOW30) then
        low30 = reader:varint("position.low30")
    end
    local lowRank30d
    if hasBit(pflags, PFLAG_LOW_RANK) then
        lowRank30d = reader:uint("position.lowRank30d", MAX_U16)
    end
    local lows
    if hasBit(pflags, PFLAG_LOWS) then
        -- The count is untrusted: it is checked against its ceiling before a
        -- single byte is read.
        local count = reader:uint("position.lows", MAX_LOWS)
        lows = {}
        for index = 1, count do
            lows[index] = reader:byte("position.lows")
        end
    end
    return low30, lowRank30d, lows
end

---@param text string one record's text
---@return AmdbRecord record
local function read(text)
    local reader = Pack.new(text)
    local flags = reader:flag("flags", GROUP_MASK)
    local spanMask = reader:flag("spans", SPAN_MASK)
    if spanMask == 0 then
        error(NO_SPANS, 0)
    end

    local spans = {}
    for index = 1, #SPAN_NAMES do
        if hasBit(spanMask, SPAN_BITS[index]) then
            spans[SPAN_NAMES[index]] = readSpan(reader)
        end
    end

    local live
    local value7, soldMedian14, samples7, trendBp
    local soldCount, expiredCount, supplyDays
    local low30, lowRank30d, lows
    local vendorPrice
    local craft
    local deal
    if hasBit(flags, GROUP_LIVE) then
        live = readLive(reader)
    end
    if hasBit(flags, GROUP_VALUE) then
        value7, soldMedian14, samples7, trendBp = readValue(reader)
    end
    if hasBit(flags, GROUP_EVIDENCE) then
        soldCount, expiredCount, supplyDays = readEvidence(reader)
    end
    if hasBit(flags, GROUP_POSITION) then
        low30, lowRank30d, lows = readPosition(reader)
    end
    if hasBit(flags, GROUP_VENDOR) then
        vendorPrice = reader:varint("vendor.vendorPrice")
    end
    if hasBit(flags, GROUP_CRAFT) then
        craft = {
            cost = reader:varint("craft.cost"),
            profit = reader:zigzag("craft.profit"),
            expectedProfit = reader:zigzag("craft.expectedProfit"),
        }
    end
    if hasBit(flags, GROUP_DEAL) then
        deal = {
            rank = reader:uint("deal.rank", MAX_U32),
            scoreBp = reader:uint("deal.scoreBp", MAX_U32),
            discountBp = reader:uint("deal.discountBp", MAX_U32),
        }
    end

    reader:finish()
    return setmetatable({
        spans = spans,
        live = live,
        value7 = value7,
        soldMedian14 = soldMedian14,
        samples7 = samples7,
        trendBp = trendBp,
        soldCount = soldCount,
        expiredCount = expiredCount,
        supplyDays = supplyDays,
        low30 = low30,
        lowRank30d = lowRank30d,
        lows = lows,
        vendorPrice = vendorPrice,
        craft = craft,
        deal = deal,
    }, AmdbRecord)
end

--- Unpacks one record from its printable word. Never throws.
---@param text string one record's text, as the carrier file holds it
---@return AmdbRecord|nil record nil when the text is malformed
---@return string|nil message why it was refused, the Rust decoder's `PackError` text
function Record.decode(text)
    local ok, value = pcall(read, text)
    if ok then
        return value, nil
    end
    return nil, tostring(value)
end

------------------------------------------------------------------- derived

--- The one value the record is worth, and where it comes from (section 5.8).
---@param record AmdbRecord
---@return number|nil value
---@return string|nil source "sold", "listed" or the span name; nil without a value
local function pickReference(record)
    local soldMedian14 = record.soldMedian14
    if soldMedian14 ~= nil and soldMedian14 > 0 then
        return soldMedian14, "sold"
    end
    local value7 = record.value7
    if value7 ~= nil and value7 > 0 then
        return value7, "listed"
    end
    local spans = record.spans
    for index = 1, #REFERENCE_SPANS do
        local name = REFERENCE_SPANS[index]
        local block = spans[name]
        local median = block and block.median
        if median ~= nil and median > 0 then
            return median, name
        end
    end
    return nil, nil
end

--- The first present value of a span field across the spans, in wire order.
---@param spans table<string, AmdbSpanBlock>
---@param field string the span block field
---@param count integer how many spans, in wire order, to consult
---@return number|nil value
local function firstPresent(spans, field, count)
    for index = 1, count do
        local block = spans[SPAN_NAMES[index]]
        local value = block and block[field]
        if value ~= nil then
            return value
        end
    end
    return nil
end

--- The market value: `soldMedian14` if above zero, else `value7` if above
--- zero, else the first of d7, d14, d30, d1 `median` that is above zero.
---@return number|nil value copper
function AmdbRecord:reference()
    local value = pickReference(self)
    return value
end

--- Where `reference()` comes from.
---@return string|nil source "sold", "listed" or the span name; nil without a value
function AmdbRecord:valueSource()
    local _, source = pickReference(self)
    return source
end

--- The 14-day sale rate: the first of d1, d7 `saleRate`.
---@return number|nil rate a fraction
function AmdbRecord:saleRate14()
    return firstPresent(self.spans, "saleRate", 2)
end

--- The 180-day sale rate: the first present across d1, d7, d14, d30.
---@return number|nil rate a fraction
function AmdbRecord:saleRate180d()
    return firstPresent(self.spans, "saleRate180d", #SPAN_NAMES)
end

--- Days since the last credited sale: the first present across d1..d30.
---@return integer|nil days
function AmdbRecord:daysSinceLastSale()
    return firstPresent(self.spans, "daysSinceLastSale", #SPAN_NAMES)
end

--- Listings closed since the last credited sale: first present across d1..d30.
---@return integer|nil count
function AmdbRecord:closuresSinceLastSale()
    return firstPresent(self.spans, "closuresSinceLastSale", #SPAN_NAMES)
end

--- Units sold per day: d14 `qtySold` / 14, else d7 / 7, else d1.
---@return number|nil units a day
function AmdbRecord:soldPerDay()
    local spans = self.spans
    local d14 = spans.d14
    if d14 ~= nil and d14.qtySold ~= nil then
        return d14.qtySold / 14
    end
    local d7 = spans.d7
    if d7 ~= nil and d7.qtySold ~= nil then
        return d7.qtySold / 7
    end
    local d1 = spans.d1
    if d1 ~= nil and d1.qtySold ~= nil then
        return d1.qtySold
    end
    return nil
end

--- Whether the live floor is at or under the vendor sell price.
---@return boolean at_floor
function AmdbRecord:atVendorFloor()
    local vendorPrice = self.vendorPrice
    local live = self.live
    local floor = live and live.floor
    return vendorPrice ~= nil and floor ~= nil and floor <= vendorPrice
end

--- Whether the stock never clears: no `supplyDays` although `soldCount` is a
--- present zero (section 5.7). A `supplyDays` of 65535 is a different case:
--- at least that many days of stock, which the caller shows the same way.
---@return boolean unbounded
function AmdbRecord:supplyUnbounded()
    local soldCount = self.soldCount
    return self.supplyDays == nil and soldCount ~= nil and soldCount == 0
end

return Record
