-- Spec-only record ENCODER: the Lua twin of the tracker's writer
-- (crates/infra/src/record.rs, `encode`) and of the primitives it uses
-- (crates/infra/src/pack.rs, `put_varint`, `zigzag`, `to_text`).
--
-- The addon ships a decoder only, so the specs need a writer to exercise it
-- with. It is written from the Rust encoder and NOT from `Core/pack.lua`, so
-- the decoder under test gets an independent second opinion. Its input is a
-- record in the decoder's own shape (`AmdbRecord`): rates as fractions, absent
-- fields nil. Never shipped: it lives under spec/, outside the addon folder,
-- and is loaded with a plain require.

local MAX_INTEGER = 9007199254740991
local MAX_U32 = 4294967295
local ALPHABET = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz+."

-- Group presence, in the order the groups follow the spans.
local GROUP_LIVE, GROUP_VALUE, GROUP_EVIDENCE, GROUP_POSITION, GROUP_VENDOR, GROUP_CRAFT, GROUP_DEAL =
    0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40

local SPAN_BITS = { 0x01, 0x02, 0x04, 0x08 }
local SPAN_NAMES = { "d1", "d7", "d14", "d30" }

local SFLAG_SALE_RATE, SFLAG_SALE_RATE_180D, SFLAG_DAYS_SINCE_LAST_SALE, SFLAG_CLOSURES, SFLAG_QTY_SOLD =
    0x01, 0x02, 0x04, 0x08, 0x10

local LFLAG_LISTED, LFLAG_QTY_SUM, LFLAG_SEEN, LFLAG_DAYS_LISTED = 0x01, 0x02, 0x04, 0x08
local VFLAG_SOLD_MEDIAN14, VFLAG_TREND = 0x01, 0x02
local EFLAG_SOLD_COUNT, EFLAG_EXPIRED_COUNT, EFLAG_SUPPLY_DAYS = 0x01, 0x02, 0x04
local PFLAG_LOW30, PFLAG_LOW_RANK, PFLAG_LOWS = 0x01, 0x02, 0x04

local Encode = {}

------------------------------------------------------------------- primitives

---@param value integer an unsigned integer below 2^63
---@return integer[] bytes the varint: 7 bits per byte, low group first
function Encode.varint(value)
    local out = {}
    while value >= 128 do
        out[#out + 1] = value % 128 + 128
        value = math.floor(value / 128)
    end
    out[#out + 1] = value
    return out
end

--- The zigzag encoding of a signed value: the sign in the low bit, so that
--- 0, -1, 1, -2, 2 read as 0, 1, 2, 3, 4. Exact only while the code is below
--- 2^53, which leaves out the most negative integers: `Encode.zigzag` builds
--- those from its bytes instead.
---@param value integer a signed integer
---@return integer code
function Encode.zigzagCode(value)
    if value >= 0 then
        return 2 * value
    end
    return -2 * value - 1
end

---@param value integer a signed integer
---@return integer[] bytes the zigzag code as a varint
function Encode.zigzag(value)
    if value >= 0 then
        return Encode.varint(2 * value)
    end
    -- The code of a negative value is odd: 2 * |value| - 1 = 2 * (|value| - 1) + 1.
    -- A double cannot hold an odd number of 2^54, so the low bit is placed by
    -- hand and the magnitude supplies the groups above it.
    local magnitude = -value - 1
    local low = magnitude % 64 * 2 + 1
    local rest = math.floor(magnitude / 64)
    if rest == 0 then
        return { low }
    end
    return Encode.cat({ low + 128 }, Encode.varint(rest))
end

--- An unsigned field as the wire carries it: a negative value (a tracker bug,
--- never real data) saturates at zero, anything above 2^53 - 1 at the ceiling.
---@param value integer a signed integer
---@return integer[] bytes
function Encode.uint(value)
    if value < 0 then
        value = 0
    elseif value > MAX_INTEGER then
        value = MAX_INTEGER
    end
    return Encode.varint(value)
end

local function signed(value)
    if value > MAX_INTEGER then
        value = MAX_INTEGER
    elseif value < -MAX_INTEGER then
        value = -MAX_INTEGER
    end
    return Encode.zigzag(value)
end

--- A rate as the wire carries it: basis points, rounded half away from zero.
--- Not a number or an infinity is no evidence: nil, so the field stays absent
--- instead of reading as a zero rate.
---@param rate number|nil a fraction
---@return integer|nil bp
function Encode.basisPoints(rate)
    if rate == nil or rate ~= rate or rate == math.huge or rate == -math.huge then
        return nil
    end
    local scaled = rate * 10000
    local bp
    if scaled >= 0 then
        bp = math.floor(scaled + 0.5)
    else
        bp = math.ceil(scaled - 0.5)
    end
    -- The wire field is a u32 and the cast saturates at both ends.
    if bp < 0 then
        return 0
    end
    if bp > MAX_U32 then
        return MAX_U32
    end
    return bp
end

local function rateBytes(rate)
    local bp = Encode.basisPoints(rate)
    if bp == nil then
        return nil
    end
    return Encode.varint(bp)
end

--- Concatenates byte arrays and single bytes, in order.
---@param ... integer|integer[] parts
---@return integer[] bytes
function Encode.cat(...)
    local out = {}
    local count = 0
    for index = 1, select("#", ...) do
        local part = select(index, ...)
        if type(part) == "number" then
            count = count + 1
            out[count] = part
        else
            for position = 1, #part do
                count = count + 1
                out[count] = part[position]
            end
        end
    end
    return out
end

--- Renders a byte stream as record text: three bytes are exactly four 6-bit
--- symbols, a leftover byte leaves four zero padding bits, two leave two.
---@param bytes integer[]
---@return string text
function Encode.toText(bytes)
    local out = {}
    local index = 1
    local count = #bytes
    while index + 2 <= count do
        local word = bytes[index] * 65536 + bytes[index + 1] * 256 + bytes[index + 2]
        out[#out + 1] = string.char(
            ALPHABET:byte(math.floor(word / 262144) + 1),
            ALPHABET:byte(math.floor(word / 4096) % 64 + 1),
            ALPHABET:byte(math.floor(word / 64) % 64 + 1),
            ALPHABET:byte(word % 64 + 1)
        )
        index = index + 3
    end
    local left = count - index + 1
    if left == 1 then
        local byte = bytes[index]
        out[#out + 1] = string.char(ALPHABET:byte(math.floor(byte / 4) + 1), ALPHABET:byte(byte % 4 * 16 + 1))
    elseif left == 2 then
        local word = bytes[index] * 256 + bytes[index + 1]
        out[#out + 1] = string.char(
            ALPHABET:byte(math.floor(word / 1024) + 1),
            ALPHABET:byte(math.floor(word / 16) % 64 + 1),
            ALPHABET:byte(word % 16 * 4 + 1)
        )
    end
    return table.concat(out)
end

---------------------------------------------------------------------- records

local function put(out, bytes)
    local count = #out
    for index = 1, #bytes do
        count = count + 1
        out[count] = bytes[index]
    end
end

--- One span block: its flag word, the twelve fixed fields, then the optional
--- five in flag order.
local function putSpan(out, block)
    local saleRate = rateBytes(block.saleRate)
    local saleRate180d = rateBytes(block.saleRate180d)
    local sflags = 0
    if saleRate ~= nil then
        sflags = sflags + SFLAG_SALE_RATE
    end
    if saleRate180d ~= nil then
        sflags = sflags + SFLAG_SALE_RATE_180D
    end
    if block.daysSinceLastSale ~= nil then
        sflags = sflags + SFLAG_DAYS_SINCE_LAST_SALE
    end
    if block.closuresSinceLastSale ~= nil then
        sflags = sflags + SFLAG_CLOSURES
    end
    if block.qtySold ~= nil then
        sflags = sflags + SFLAG_QTY_SOLD
    end
    put(out, Encode.varint(sflags))

    put(out, Encode.uint(block.n))
    put(out, Encode.uint(block.min))
    put(out, Encode.uint(block.p05))
    put(out, Encode.uint(block.p25))
    put(out, Encode.uint(block.median))
    put(out, Encode.uint(block.p75))
    put(out, Encode.uint(block.p95))
    put(out, Encode.uint(block.max))
    put(out, Encode.uint(block.mad))
    put(out, Encode.uint(block.vwap))
    put(out, Encode.uint(block.qtySum))
    put(out, Encode.uint(block.listingCount))

    if saleRate ~= nil then
        put(out, saleRate)
    end
    if saleRate180d ~= nil then
        put(out, saleRate180d)
    end
    if block.daysSinceLastSale ~= nil then
        put(out, Encode.uint(block.daysSinceLastSale))
    end
    if block.closuresSinceLastSale ~= nil then
        put(out, Encode.uint(block.closuresSinceLastSale))
    end
    if block.qtySold ~= nil then
        put(out, Encode.uint(block.qtySold))
    end
end

--- The `live` group: the listing summary, then what was seen and is gone,
--- then how long ago the item was last listed. `listed` carries `floor`,
--- `floorQty` and `listed` together; `qtySum` only rides with it; `seenFloor`
--- carries `seenQty`.
local function putLive(out, live)
    local lflags = 0
    if live.listed ~= nil then
        lflags = lflags + LFLAG_LISTED
    end
    if live.qtySum ~= nil then
        lflags = lflags + LFLAG_QTY_SUM
    end
    if live.seenFloor ~= nil then
        lflags = lflags + LFLAG_SEEN
    end
    if live.daysSinceListed ~= nil then
        lflags = lflags + LFLAG_DAYS_LISTED
    end
    put(out, Encode.varint(lflags))

    if live.listed ~= nil then
        put(out, Encode.uint(live.floor))
        put(out, Encode.uint(live.floorQty))
        put(out, Encode.uint(live.listed))
        if live.qtySum ~= nil then
            put(out, Encode.uint(live.qtySum))
        end
    end
    if live.seenFloor ~= nil then
        put(out, Encode.uint(live.seenFloor))
        put(out, Encode.uint(live.seenQty))
    end
    if live.daysSinceListed ~= nil then
        put(out, Encode.uint(live.daysSinceListed))
    end
end

--- Packs a record exactly as the tracker does. Deterministic and infallible:
--- a record is written as given, including one a decoder then refuses (a
--- record without a span, `qtySum` without `listed`, a value beyond its
--- field). A group is written when its data is present: `value` when `value7`
--- is, `evidence` and `position` when any of their fields is.
---@param record AmdbRecord a record in the decoder's shape; rates are fractions
---@return integer[] bytes the record's byte stream
function Encode.recordBytes(record)
    local out = {}

    local flags = 0
    if record.live ~= nil then
        flags = flags + GROUP_LIVE
    end
    if record.value7 ~= nil then
        flags = flags + GROUP_VALUE
    end
    if record.soldCount ~= nil or record.expiredCount ~= nil or record.supplyDays ~= nil then
        flags = flags + GROUP_EVIDENCE
    end
    if record.low30 ~= nil or record.lowRank30d ~= nil or record.lows ~= nil then
        flags = flags + GROUP_POSITION
    end
    if record.vendorPrice ~= nil then
        flags = flags + GROUP_VENDOR
    end
    if record.craft ~= nil then
        flags = flags + GROUP_CRAFT
    end
    if record.deal ~= nil then
        flags = flags + GROUP_DEAL
    end
    put(out, Encode.varint(flags))

    local spans = record.spans or {}
    local spanMask = 0
    for index = 1, #SPAN_NAMES do
        if spans[SPAN_NAMES[index]] ~= nil then
            spanMask = spanMask + SPAN_BITS[index]
        end
    end
    put(out, Encode.varint(spanMask))
    for index = 1, #SPAN_NAMES do
        local block = spans[SPAN_NAMES[index]]
        if block ~= nil then
            putSpan(out, block)
        end
    end

    if record.live ~= nil then
        putLive(out, record.live)
    end

    if record.value7 ~= nil then
        local vflags = 0
        if record.soldMedian14 ~= nil then
            vflags = vflags + VFLAG_SOLD_MEDIAN14
        end
        if record.trendBp ~= nil then
            vflags = vflags + VFLAG_TREND
        end
        put(out, Encode.varint(vflags))
        put(out, Encode.uint(record.value7))
        if record.soldMedian14 ~= nil then
            put(out, Encode.uint(record.soldMedian14))
        end
        put(out, Encode.uint(record.samples7))
        if record.trendBp ~= nil then
            put(out, signed(record.trendBp))
        end
    end

    if record.soldCount ~= nil or record.expiredCount ~= nil or record.supplyDays ~= nil then
        local eflags = 0
        if record.soldCount ~= nil then
            eflags = eflags + EFLAG_SOLD_COUNT
        end
        if record.expiredCount ~= nil then
            eflags = eflags + EFLAG_EXPIRED_COUNT
        end
        if record.supplyDays ~= nil then
            eflags = eflags + EFLAG_SUPPLY_DAYS
        end
        put(out, Encode.varint(eflags))
        if record.soldCount ~= nil then
            put(out, Encode.uint(record.soldCount))
        end
        if record.expiredCount ~= nil then
            put(out, Encode.uint(record.expiredCount))
        end
        if record.supplyDays ~= nil then
            put(out, Encode.varint(record.supplyDays))
        end
    end

    if record.low30 ~= nil or record.lowRank30d ~= nil or record.lows ~= nil then
        local pflags = 0
        if record.low30 ~= nil then
            pflags = pflags + PFLAG_LOW30
        end
        if record.lowRank30d ~= nil then
            pflags = pflags + PFLAG_LOW_RANK
        end
        if record.lows ~= nil then
            pflags = pflags + PFLAG_LOWS
        end
        put(out, Encode.varint(pflags))
        if record.low30 ~= nil then
            put(out, Encode.uint(record.low30))
        end
        if record.lowRank30d ~= nil then
            put(out, Encode.varint(record.lowRank30d))
        end
        local lows = record.lows
        if lows ~= nil then
            put(out, Encode.varint(#lows))
            for index = 1, #lows do
                put(out, { lows[index] })
            end
        end
    end

    if record.vendorPrice ~= nil then
        put(out, Encode.uint(record.vendorPrice))
    end

    local craft = record.craft
    if craft ~= nil then
        put(out, Encode.uint(craft.cost))
        put(out, signed(craft.profit))
        put(out, signed(craft.expectedProfit))
    end

    local deal = record.deal
    if deal ~= nil then
        put(out, Encode.varint(deal.rank))
        put(out, Encode.varint(deal.scoreBp))
        put(out, Encode.varint(deal.discountBp))
    end

    return out
end

---@param record AmdbRecord a record in the decoder's shape; rates are fractions
---@return string text the record's printable word
function Encode.record(record)
    return Encode.toText(Encode.recordBytes(record))
end

-------------------------------------------------------------------- comparing

--- Deep equality of two records: every key present on one side is present on
--- the other with an equal value. An absent field (nil) differs from `false`
--- and from 0, and metatables are not looked at.
---@param left table|nil
---@param right table|nil
---@return boolean equal
function Encode.equal(left, right)
    if left == right then
        return true
    end
    if type(left) ~= "table" or type(right) ~= "table" then
        return false
    end
    for key, value in pairs(left) do
        if not Encode.equal(value, right[key]) then
            return false
        end
    end
    for key in pairs(right) do
        if left[key] == nil then
            return false
        end
    end
    return true
end

return Encode
