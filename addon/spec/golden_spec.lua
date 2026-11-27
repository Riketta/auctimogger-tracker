-- The golden trio (addon/SPEC.md, section 5.11). `fixtures/database_v4.lua` is
-- the carrier the tracker renders; `fixtures/database_v4_decoded.lua` is the
-- oracle: the header and the nine records exactly as a correct decoder must
-- read them. The carrier is loaded the way the game client loads it, into a
-- namespace whose BeginMarket captures what the builder is handed.

local Loader = require("spec.helpers.loader")
local Encode = require("spec.helpers.encode")

local A = Loader.load(Loader.namespace(), "Core/pack.lua", "Core/record.lua")
local Pack = A.Core.Pack
local Record = A.Core.Record

local ALPHABET = Pack.ALPHABET

local TABLES = { "items", "commodities", "pets" }

---@class AmdbGoldenHeader
---@field version integer
---@field trackerVersion string
---@field target string
---@field flavor string
---@field region string
---@field rulesetId integer
---@field houseId integer
---@field generatedAt integer
---@field auctionsAt integer
---@field commoditiesAt integer

---@class AmdbGoldenMarket
---@field meta AmdbGoldenHeader
---@field items table<integer, AmdbRecord>
---@field commodities table<integer, AmdbRecord>
---@field pets table<integer, AmdbRecord>

---@type AmdbGoldenMarket
local oracle = dofile(Loader.FIXTURES .. "/database_v4_decoded.lua")

--- What the carrier handed over: one BeginMarket, one builder, one EndMarket.
---@class AmdbCapturedCarrier
---@field header AmdbGoldenHeader|nil
---@field items table<integer, string>
---@field commodities table<integer, string>
---@field pets table<integer, string>
local captured = { items = {}, commodities = {}, pets = {} }
local begun = 0
local ended = 0

local namespace = Loader.namespace()
---@param header AmdbGoldenHeader
---@return AmdbCapturedCarrier builder
function namespace.BeginMarket(header)
    begun = begun + 1
    captured.header = header
    return captured
end
function namespace.EndMarket()
    ended = ended + 1
end

Loader.carrier(namespace, Loader.FIXTURES .. "/database_v4.lua")

--- The ids of one carrier table, in ascending order.
---@param name string "items", "commodities" or "pets"
---@return integer[] ids
local function ids(name)
    local out = {}
    for id in pairs(oracle[name]) do
        out[#out + 1] = id
    end
    table.sort(out)
    return out
end

--- The record word of one oracle entry.
---@param name string
---@param id integer
---@return string word
local function word(name, id)
    return captured[name][id]
end

--- Decodes one oracle entry's word; the record must exist.
---@param name string
---@param id integer
---@return AmdbRecord record
local function read(name, id)
    local out, message = Record.decode(word(name, id))
    assert.is_not_nil(out, message)
    return out
end

--- Asserts that `actual` holds exactly the keys of `expected`.
---@param actual table<any, any>
---@param expected table<any, any>
---@param what string
local function sameKeySet(actual, expected, what)
    for key in pairs(expected) do
        assert.is_not_nil(actual[key], what .. ": no " .. tostring(key))
    end
    local unexpected = {}
    for key in pairs(actual) do
        if expected[key] == nil then
            unexpected[#unexpected + 1] = tostring(key)
        end
    end
    assert.equals("", table.concat(unexpected, ", "), what .. ": unexpected keys")
end

--- Every field against the oracle, over its key set: both nil or both equal.
---@param record AmdbRecord
---@param expected AmdbRecord
local function sameRecord(record, expected)
    -- Leaves are equal (an absent field is nil on both sides), and no table
    -- holds a key the other one lacks.
    assert.is_true(Encode.equal(record, expected))
    sameKeySet(record, expected, "record")
    sameKeySet(record.spans, expected.spans, "spans")
    for name, block in pairs(expected.spans) do
        sameKeySet(record.spans[name], block, "span " .. name)
    end
    sameKeySet(record.live or {}, expected.live or {}, "live")
    sameKeySet(record.craft or {}, expected.craft or {}, "craft")
    sameKeySet(record.deal or {}, expected.deal or {}, "deal")
    if expected.lows ~= nil then
        assert.equals(#expected.lows, #record.lows)
    end
end

describe("the golden carrier's header", function()
    it("is handed to BeginMarket intact, key by key", function()
        assert.equals(1, begun)
        assert.equals(1, ended)
        local header = captured.header
        assert(header ~= nil, "the carrier never called BeginMarket")
        sameKeySet(header, oracle.meta, "header")
        sameKeySet(oracle.meta, header, "header")
        local keys = 0
        for key, value in pairs(oracle.meta) do
            keys = keys + 1
            assert.equals(value, header[key], key)
        end
        assert.equals(10, keys)
        assert.equals(4, header.version)
    end)
end)

describe("the golden carrier's records", function()
    it("are exactly the oracle's, table by table", function()
        for _, name in ipairs(TABLES) do
            local count = 0
            for id in pairs(captured[name]) do
                count = count + 1
                assert.is_not_nil(oracle[name][id], name .. "[" .. id .. "] is not in the oracle")
            end
            assert.equals(#ids(name), count, name)
            for _, id in ipairs(ids(name)) do
                assert.equals("string", type(word(name, id)), name .. "[" .. id .. "]")
            end
        end
        -- The nine records of section 5.11.
        assert.equals(4, #ids("items"))
        assert.equals(3, #ids("commodities"))
        assert.equals(2, #ids("pets"))
    end)

    it("are single words of alphabet characters", function()
        for _, name in ipairs(TABLES) do
            for _, id in ipairs(ids(name)) do
                local text = word(name, id)
                local plain, foreign = text:gsub("[^" .. ALPHABET .. "]", "")
                assert.equals(0, foreign, name .. "[" .. id .. "]")
                assert.equals(text, plain)
            end
        end
    end)
end)

-- What section 5.8 derives from each golden record.
---@class AmdbDerivedCase
---@field tableName string
---@field id integer
---@field value integer
---@field source string
local DERIVED = {
    { tableName = "items", id = 25, value = 7030, source = "listed" },
    { tableName = "items", id = 117, value = 9500000000, source = "listed" },
    { tableName = "items", id = 2589, value = 3200, source = "sold" },
    { tableName = "items", id = 194784, value = 4400000, source = "d7" },
    { tableName = "commodities", id = 191460, value = 80000, source = "listed" },
    { tableName = "commodities", id = 191461, value = 19, source = "sold" },
    { tableName = "commodities", id = 194784, value = 6137000, source = "sold" },
    { tableName = "pets", id = 293, value = 8000000, source = "d30" },
    { tableName = "pets", id = 2546, value = 2450000, source = "sold" },
}

for _, case in ipairs(DERIVED) do
    local name, id = case.tableName, case.id
    describe(name .. "[" .. id .. "]", function()
        it("decodes to the oracle's record, field for field", function()
            sameRecord(read(name, id), oracle[name][id])
        end)

        it("re-encodes to the carrier's own word", function()
            assert.equals(word(name, id), Encode.record(read(name, id)))
        end)

        it("derives its reference the way section 5.8 says", function()
            local record = read(name, id)
            local expected = oracle[name][id]
            assert.equals(case.value, record:reference())
            assert.equals(case.source, record:valueSource())
            -- That is one field of the oracle's own record.
            local fromField
            if case.source == "sold" then
                fromField = expected.soldMedian14
            elseif case.source == "listed" then
                fromField = expected.value7
            else
                fromField = expected.spans[case.source].median
            end
            assert.equals(fromField, record:reference())
        end)
    end)
end

describe("the golden records' other derived values", function()
    it("read the sale rates of the steady seller", function()
        local steady = read("items", 25)
        assert.equals(8889 / 10000, steady:saleRate14())
        assert.equals(8889 / 10000, steady:saleRate180d())
        assert.equals(0, steady:daysSinceLastSale())
        assert.equals(0, steady:closuresSinceLastSale())
        assert.equals(32 / 14, steady:soldPerDay())
        assert.is_false(steady:atVendorFloor())
        assert.is_false(steady:supplyUnbounded())
    end)

    it("keep a present zero apart from an absent value", function()
        -- A listing that never sells: a present zero rate, a present zero sold.
        local never = read("items", 117)
        assert.equals(0, never:saleRate14())
        assert.equals(0, never:soldPerDay())
        assert.equals(65535, never.supplyDays)
        assert.is_false(never:supplyUnbounded())
        -- Nothing sold and no supply figure: the stock never clears.
        local unsold = read("commodities", 191460)
        assert.equals(0, unsold.soldCount)
        assert.is_nil(unsold.supplyDays)
        assert.is_true(unsold:supplyUnbounded())
        -- The days come from the first span that has them, d1 here.
        local gone = read("items", 2589)
        assert.equals(1, gone:daysSinceLastSale())
        assert.equals(2, gone:closuresSinceLastSale())
        assert.is_nil(read("items", 194784):daysSinceLastSale())
    end)

    it("answer atVendorFloor from the live floor and the vendor price", function()
        -- A vendor price of one copper under a floor of twelve.
        local bulk = read("commodities", 191461)
        assert.is_false(bulk:atVendorFloor())
        -- Seen today and gone, or never listed: there is no floor.
        assert.is_false(read("items", 2589):atVendorFloor())
        assert.is_false(read("items", 194784):atVendorFloor())
        assert.equals(77000000 / 14, bulk:soldPerDay())
        assert.equals(9411 / 10000, bulk:saleRate180d())
        assert.equals(0, bulk:closuresSinceLastSale())
    end)

    it("date the last listing off the newest snapshot, then the hour fold", function()
        -- Listed now, or seen today and gone: last listed today.
        assert.equals(0, read("items", 25).live.daysSinceListed)
        assert.equals(0, read("items", 2589).live.daysSinceListed)
        assert.equals(0, read("commodities", 194784).live.daysSinceListed)
        -- Listed three days ago and not since: the one thing its live group holds.
        local quiet = read("items", 194784)
        assert.equals(3, quiet.live.daysSinceListed)
        assert.is_nil(quiet.live.floor)
        assert.is_nil(quiet.live.seenFloor)
        -- A pooled species carries spans only: no live group, so no date.
        assert.is_nil(read("pets", 293).live)
    end)
end)
