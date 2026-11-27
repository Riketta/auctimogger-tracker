-- Specs for Adapters/public_api.lua: the namespace published as the global of
-- SPEC 7.9 and section 10 "public API".
--
-- The golden carrier is loaded the way the client loads a slot file, through
-- the real Store and decoder, and every field of `AmdbMarketInfo` is held
-- against the oracle (fixtures/database_v4_decoded.lua).

local Loader = require("spec.helpers.loader")

local NOW = 1795560000
local DAY = 86400
local GOLDEN = Loader.FIXTURES .. "/database_v4.lua"
local TARGET = "forever-eu-pvp-alliance"
local OTHER = "forever-eu-pvp-horde"
local GENERATED_AT = 1795559034
local AUCTIONS_AT = 1795558000
local COMMODITIES_AT = 1795556000

local ITEM_LINK = "|cff1eff00|Hitem:194784:::::::|h[Arcanist's Sunstone]|h|r"
local LINK_25 = "|cff1eff00|Hitem:25:0:0:0:0:0:0:0:0:0|h[Soot]|h|r"
local PET_LINK = "|cff71d5ff|Hbattlepet:2546:1589:13:329:19:12:0000000000000000|h[Anubisath Idol]|h|r"

--- Everything the TOC loads before bootstrap.lua, slot files excepted.
local FILES = {
    "Core/pack.lua",
    "Core/record.lua",
    "Core/market.lua",
    "Core/settings.lua",
    "Core/format.lua",
    "Core/resolve.lua",
    "Adapters/market_files.lua",
    "Adapters/compat.lua",
    "Adapters/public_api.lua",
}

--- The field set of `AmdbMarketInfo` (SPEC 7.9), sorted.
local FIELDS = {
    "itemID",
    "category",
    "market",
    "generatedAt",
    "ageSeconds",
    "snapshotAgeSeconds",
    "stale",
    "floorNow",
    "floorQty",
    "listedNow",
    "p05Now",
    "seenFloor",
    "daysSinceListed",
    "value",
    "valueSource",
    "trend",
    "soldMedian14",
    "saleRate",
    "saleRate180d",
    "soldCount",
    "expiredCount",
    "soldPerDay",
    "supplyDays",
    "daysSinceLastSale",
    "closuresSinceLastSale",
    "low30",
    "lowRank30d",
    "vendorPrice",
    "craft",
    "deal",
    "spans",
}
table.sort(FIELDS)

--- The fields of one v3 span block.
local SPAN_FIELDS = {
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
    "qtySold",
    "saleRate",
    "saleRate180d",
    "daysSinceLastSale",
    "closuresSinceLastSale",
}

local oracle = dofile(Loader.FIXTURES .. "/database_v4_decoded.lua")

--- The keys of `t`, sorted.
---@param t table
---@return any[] keys
local function keysOf(t)
    local keys = {}
    for key in pairs(t) do
        keys[#keys + 1] = key
    end
    table.sort(keys)
    return keys
end

--- Asserts that `actual` is the list `expected`, element for element.
---@param expected any[]
---@param actual any
local function sameList(expected, actual)
    assert.is_table(actual)
    assert.equals(#expected, #actual)
    for i = 1, #expected do
        assert.equals(expected[i], actual[i])
    end
end

--- True when `list` holds `value`.
---@param list any[]
---@param value any
local function holds(list, value)
    for i = 1, #list do
        if list[i] == value then
            return true
        end
    end
    return false
end

--- A fresh namespace in TOC order up to public_api.lua, the clock pinned to
--- NOW. `previous` is the GetAuctionBuyout another addon defined first.
---@param previous any
---@return table A namespace
local function api(previous)
    rawset(_G, "GetAuctionBuyout", previous)
    local A = Loader.load(Loader.namespace(), unpack(FILES))
    A.clock = function()
        return NOW
    end
    return A
end

--- `api` with the golden carrier registered.
---@param previous any
---@return table A namespace
local function loaded(previous)
    local A = api(previous)
    Loader.carrier(A, GOLDEN)
    return A
end

--- Registers a second market: a header, and the record lines `records` adds.
---@param A table namespace
---@param target string
---@param generatedAt number epoch seconds
---@param records string|nil Lua lines run between BeginMarket and EndMarket
local function secondMarket(A, target, generatedAt, records)
    local source = "local _, A = ...\n"
        .. "local m = A.BeginMarket({ version = 4, trackerVersion = \"0.1.0\", target = "
        .. string.format("%q", target)
        .. ", generatedAt = "
        .. tostring(generatedAt)
        .. " })\n"
        .. (records or "")
        .. "A.EndMarket()\n"
    local chunk = assert(loadstring(source, "=carrier"))
    chunk(Loader.ADDON, A)
end

--- The golden carrier's record words.
local words = { items = {}, commodities = {}, pets = {} }
do
    local ns = {
        BeginMarket = function()
            return words
        end,
        EndMarket = function() end,
    }
    Loader.carrier(ns, GOLDEN)
end

--- The record lines of a second market: item 25, copied from the golden carrier.
local ITEM_25 = "m.items[25] = \"" .. words.items[25] .. "\"\n"

describe("public_api", function()
    local savedNamespace, savedBuyout

    before_each(function()
        savedNamespace = rawget(_G, "AuctiMoggerDatabase")
        savedBuyout = rawget(_G, "GetAuctionBuyout")
    end)

    after_each(function()
        rawset(_G, "AuctiMoggerDatabase", savedNamespace)
        rawset(_G, "GetAuctionBuyout", savedBuyout)
    end)

    it("publishes the namespace as the global and carries the versions", function()
        local A = loaded()
        assert.equals(A, rawget(_G, "AuctiMoggerDatabase"))
        assert.equals("0.1.1", A.VERSION)
        assert.equals(4, A.FEED_VERSION)
        assert.equals(A.Core.Market.FEED_VERSION, A.FEED_VERSION)
        local published = rawget(_G, "AuctiMoggerDatabase")
        assert.equals(A.GetMarketInfo, published.GetMarketInfo)
        assert.equals(A.GetSpans, published.GetSpans)
        assert.equals(A.GetMarkets, published.GetMarkets)
        assert.equals(A.GetPrimaryMarket, published.GetPrimaryMarket)
        assert.equals(A.OnUpdate, published.OnUpdate)
        assert.equals(A.GetAuctionBuyout, published.GetAuctionBuyout)
        assert.equals(A.store, published.store)
        assert.equals(A.Compat, published.Compat)
    end)

    it("returns the namespace, and refuses to load before A.clock exists", function()
        local namespace = Loader.load(Loader.namespace(), unpack(FILES, 1, 6))
        local chunk = assert(loadfile(Loader.ROOT .. "/Adapters/public_api.lua"))
        local ok, err = pcall(chunk, Loader.ADDON, namespace)
        assert.is_false(ok)
        -- Lua 5.1's assert puts the source position in front of the message.
        local expected = "AuctiMoggerDatabase: A.clock must be set before Adapters/public_api.lua loads"
        assert.equals(expected, tostring(err):sub(-#expected))
        assert.is_nil(rawget(_G, "AuctiMoggerDatabase"))
        namespace.clock = function()
            return NOW
        end
        local module = chunk(Loader.ADDON, namespace)
        assert.equals(namespace, module)
        assert.equals(namespace, rawget(_G, "AuctiMoggerDatabase"))
    end)

    describe("GetMarketInfo", function()
        it("holds exactly the documented fields: none extra, and none missing where the record has it", function()
            local A = loaded()
            -- 7.9 spells 31 fields.
            assert.equals(31, #FIELDS)
            -- The record that carries every group has all of them present.
            sameList(FIELDS, keysOf(A.GetMarketInfo(194784, { category = "commodity" })))
            -- Elsewhere an absent value is an absent key, never a zero or a false.
            local inputs = { 25, 2589, 117, ITEM_LINK, PET_LINK }
            for i = 1, #inputs do
                local info = A.GetMarketInfo(inputs[i])
                assert.is_not_nil(info)
                for _, key in ipairs(keysOf(info)) do
                    assert.is_true(holds(FIELDS, key), key .. " is not a documented field")
                end
            end
            -- A record of spans only, dated: still the identity, market and
            -- snapshot fields, and the one thing its live group holds.
            sameList({
                "ageSeconds",
                "category",
                "daysSinceListed",
                "generatedAt",
                "itemID",
                "market",
                "saleRate",
                "snapshotAgeSeconds",
                "spans",
                "stale",
                "value",
                "valueSource",
            }, keysOf(A.GetMarketInfo(194784, { category = "item" })))
            assert.equals(NOW - AUCTIONS_AT, A.GetMarketInfo(194784).snapshotAgeSeconds)
            -- Listed three days ago and not since.
            assert.equals(3, A.GetMarketInfo(194784).daysSinceListed)
            -- The one rate the record does carry is a present zero (section 5.7).
            assert.equals(0, A.GetMarketInfo(194784).saleRate)
            assert.is_nil(A.GetMarketInfo(194784).trend)
        end)

        it("describes a listing from a snapshot summary older than the p05 column", function()
            local info = loaded().GetMarketInfo(PET_LINK)
            assert.is_not_nil(info)
            -- Everything of the listing but the percentile, which that summary
            -- never held: an absent field, never a zero.
            assert.equals(2500000, info.floorNow)
            assert.equals(2, info.listedNow)
            assert.is_nil(info.p05Now)
        end)

        it("describes a steady seller field for field", function()
            local A = loaded()
            local info = A.GetMarketInfo(25)
            assert.is_not_nil(info)
            assert.equals(25, info.itemID)
            assert.equals("item", info.category)
            assert.equals(TARGET, info.market)
            assert.equals(GENERATED_AT, info.generatedAt)
            assert.equals(NOW - GENERATED_AT, info.ageSeconds)
            assert.equals(NOW - AUCTIONS_AT, info.snapshotAgeSeconds)
            assert.is_false(info.stale)
            assert.equals(15000, info.floorNow)
            assert.equals(1, info.floorQty)
            assert.equals(58, info.listedNow)
            -- The 5th percentile of the snapshot's listing prices.
            assert.equals(15900, info.p05Now)
            assert.is_nil(info.seenFloor)
            -- Listed now: last listed today.
            assert.equals(0, info.daysSinceListed)
            assert.equals(7030, info.value)
            assert.equals("listed", info.valueSource)
            assert.equals(55934 / 10000, info.trend)
            assert.is_nil(info.soldMedian14)
            assert.equals(8889 / 10000, info.saleRate)
            assert.equals(8889 / 10000, info.saleRate180d)
            assert.equals(32, info.soldCount)
            assert.equals(4, info.expiredCount)
            assert.equals(32 / 14, info.soldPerDay)
            assert.equals(25, info.supplyDays)
            -- A present zero is a value, not an absence.
            assert.equals(0, info.daysSinceLastSale)
            assert.equals(0, info.closuresSinceLastSale)
            assert.equals(100, info.low30)
            assert.equals(8000 / 10000, info.lowRank30d)
            assert.equals(1250, info.vendorPrice)
            assert.is_nil(info.craft)
            assert.is_nil(info.deal)
            assert.same(oracle.items[25].spans, info.spans)
            assert.equals(58, info.spans.d1.n)
        end)

        it("describes an item that was seen today and is gone", function()
            local A = loaded()
            local info = A.GetMarketInfo(2589)
            assert.is_not_nil(info)
            assert.equals(2589, info.itemID)
            assert.equals("item", info.category)
            -- Seen, not listed: there is no floor to report.
            assert.is_nil(info.floorNow)
            assert.is_nil(info.floorQty)
            assert.is_nil(info.listedNow)
            assert.equals(2500, info.seenFloor)
            -- Seen today: last listed today, though nothing of it is listed.
            assert.equals(0, info.daysSinceListed)
            assert.equals(3200, info.value)
            assert.equals("sold", info.valueSource)
            assert.equals(3200, info.soldMedian14)
            assert.is_nil(info.trend)
            assert.equals(8000 / 10000, info.saleRate)
            assert.equals(7500 / 10000, info.saleRate180d)
            assert.equals(120, info.soldCount)
            assert.equals(30, info.expiredCount)
            assert.equals(1530 / 14, info.soldPerDay)
            -- No supply figure: the stock of something that sells is unknown here.
            assert.is_nil(info.supplyDays)
            assert.equals(1, info.daysSinceLastSale)
            assert.equals(2, info.closuresSinceLastSale)
            assert.is_nil(info.low30)
            assert.is_nil(info.lowRank30d)
            assert.is_nil(info.vendorPrice)
            assert.is_nil(info.craft)
            assert.is_nil(info.deal)
            assert.same(oracle.items[2589].spans, info.spans)
            sameList({ "d1", "d14", "d30", "d7" }, keysOf(info.spans))
        end)

        it("describes the record that carries every group", function()
            local A = loaded()
            local info = A.GetMarketInfo(194784, { category = "commodity" })
            assert.is_not_nil(info)
            assert.equals(194784, info.itemID)
            assert.equals("commodity", info.category)
            assert.equals(NOW - COMMODITIES_AT, info.snapshotAgeSeconds)
            assert.equals(3805000, info.floorNow)
            assert.equals(214, info.floorQty)
            assert.equals(618, info.listedNow)
            assert.equals(3750000, info.seenFloor)
            assert.equals(6137000, info.value)
            assert.equals("sold", info.valueSource)
            assert.equals(-300 / 10000, info.trend)
            assert.equals(6137000, info.soldMedian14)
            assert.equals(3617 / 10000, info.saleRate)
            assert.equals(3125 / 10000, info.saleRate180d)
            assert.equals(34, info.soldCount)
            assert.equals(60, info.expiredCount)
            assert.equals(63 / 14, info.soldPerDay)
            assert.equals(274, info.supplyDays)
            assert.equals(2, info.daysSinceLastSale)
            assert.equals(61, info.closuresSinceLastSale)
            assert.equals(3500000, info.low30)
            assert.equals(8000 / 10000, info.lowRank30d)
            assert.equals(125000, info.vendorPrice)
            assert.same({ cost = 5550000, profit = 280150, expectedProfit = 101358 }, info.craft)
            assert.same({ rank = 12, scoreBp = 7100, discountBp = 3800 }, info.deal)
            assert.same(oracle.commodities[194784].spans, info.spans)
        end)

        it("keeps a present zero apart from an absent field", function()
            local A = loaded()
            local info = A.GetMarketInfo(117)
            assert.equals(9500000000, info.value)
            assert.equals("listed", info.valueSource)
            assert.equals(0, info.trend)
            assert.equals(0, info.saleRate)
            assert.is_nil(info.saleRate180d)
            assert.equals(0, info.soldPerDay)
            assert.equals(0, info.soldCount)
            assert.equals(65535, info.supplyDays)
            assert.equals(0, info.lowRank30d)
            assert.is_nil(info.daysSinceLastSale)
            assert.is_nil(info.closuresSinceLastSale)
            assert.same({ rank = 1, scoreBp = 9250, discountBp = 5000 }, info.deal)
            assert.is_nil(info.craft)
        end)

        it("accepts an item id, an item link or an item string alike", function()
            local A = loaded()
            assert.equals(7030, A.GetMarketInfo(25).value)
            assert.equals(7030, A.GetMarketInfo("25").value)
            assert.equals(7030, A.GetMarketInfo("item:25").value)
            assert.equals(7030, A.GetMarketInfo(LINK_25).value)
            assert.equals(4400000, A.GetMarketInfo(ITEM_LINK).value)
            assert.equals(194784, A.GetMarketInfo(ITEM_LINK).itemID)
        end)

        it("reads the pool opts.category names for an id that is in both", function()
            local A = loaded()
            local item = A.GetMarketInfo(194784, { category = "item" })
            local commodity = A.GetMarketInfo(194784, { category = "commodity" })
            assert.equals("item", item.category)
            assert.equals(4400000, item.value)
            assert.equals("d7", item.valueSource)
            assert.equals("commodity", commodity.category)
            assert.equals(6137000, commodity.value)
            assert.equals(NOW - AUCTIONS_AT, item.snapshotAgeSeconds)
            assert.equals(NOW - COMMODITIES_AT, commodity.snapshotAgeSeconds)
            -- Without a pool the item comes first, which is the Store's own order.
            assert.equals("item", A.GetMarketInfo(194784).category)
            assert.equals(4400000, A.GetMarketInfo(194784).value)
            -- An id that is only a commodity needs no pool.
            assert.equals("commodity", A.GetMarketInfo(191460).category)
            assert.equals(80000, A.GetMarketInfo(191460).value)
            -- A pool the item is not in, or no pool at all, names nothing.
            assert.is_nil(A.GetMarketInfo(25, { category = "commodity" }))
            assert.is_nil(A.GetMarketInfo(191460, { category = "item" }))
            assert.is_nil(A.GetMarketInfo(194784, { category = "pet" }))
            assert.is_nil(A.GetMarketInfo(194784, { category = "nonsense" }))
        end)

        it("reads the market opts.market names", function()
            local A = loaded()
            secondMarket(A, OTHER, NOW - 600, ITEM_25)
            local here = A.GetMarketInfo(25)
            local there = A.GetMarketInfo(25, { market = OTHER })
            assert.equals(TARGET, here.market)
            assert.equals(GENERATED_AT, here.generatedAt)
            assert.equals(NOW - GENERATED_AT, here.ageSeconds)
            assert.equals(OTHER, there.market)
            assert.equals(NOW - 600, there.generatedAt)
            assert.equals(600, there.ageSeconds)
            -- A header without snapshot stamps: that age is absent, not zero.
            assert.is_nil(there.snapshotAgeSeconds)
            -- A market that is not loaded, or not named by a string, selects nothing.
            assert.is_nil(A.GetMarketInfo(25, { market = "not-loaded" }))
            assert.is_nil(A.GetMarketInfo(25, { market = 194784 }))
            -- A false option is no option.
            assert.equals(TARGET, A.GetMarketInfo(25, { market = false }).market)
        end)

        it("describes a pet through a battlepet link", function()
            local A = loaded()
            local info = A.GetMarketInfo(PET_LINK)
            assert.is_not_nil(info)
            assert.equals(2546, info.itemID)
            assert.equals("pet", info.category)
            assert.equals(2450000, info.value)
            assert.equals("sold", info.valueSource)
            assert.equals(2450000, info.soldMedian14)
            assert.equals(1, info.saleRate)
            assert.is_nil(info.saleRate180d)
            assert.is_nil(info.trend)
            assert.equals(NOW - AUCTIONS_AT, info.snapshotAgeSeconds)
            assert.equals(2500000, info.floorNow)
            assert.equals(1, info.floorQty)
            assert.equals(2, info.listedNow)
            assert.is_nil(info.seenFloor)
            assert.equals(6, info.soldCount)
            assert.equals(0, info.expiredCount)
            assert.equals(6 / 7, info.soldPerDay)
            assert.equals(4, info.supplyDays)
            assert.equals(0, info.daysSinceLastSale)
            assert.equals(0, info.closuresSinceLastSale)
            assert.equals(2400000, info.low30)
            assert.equals(0, info.lowRank30d)
            assert.is_nil(info.vendorPrice)
            assert.is_nil(info.craft)
            assert.same({ rank = 3, scoreBp = 5500, discountBp = 200 }, info.deal)
            assert.same(oracle.pets[2546].spans, info.spans)
            -- The bare string works like the link.
            assert.equals(2450000, A.GetMarketInfo("battlepet:2546").value)
        end)

        it("names a species directly through opts.species", function()
            local A = loaded()
            local info = A.GetMarketInfo(nil, { species = 293 })
            assert.is_not_nil(info)
            assert.equals(293, info.itemID)
            assert.equals("pet", info.category)
            assert.equals(8000000, info.value)
            assert.equals("d30", info.valueSource)
            assert.same(oracle.pets[293].spans, info.spans)
            sameList({ "d30" }, keysOf(info.spans))
            -- The species option wins over the item and over the category.
            assert.equals(293, A.GetMarketInfo(25, { species = 293 }).itemID)
            assert.equals(293, A.GetMarketInfo(25, { species = 293, category = "item" }).itemID)
            -- A battlepet string is accepted there; a digit string is not, it would be an item id.
            assert.equals(2546, A.GetMarketInfo(nil, { species = PET_LINK }).itemID)
            assert.is_nil(A.GetMarketInfo(nil, { species = "2546" }))
            assert.is_nil(A.GetMarketInfo(nil, { species = "battlepet:" }))
            assert.is_nil(A.GetMarketInfo(25, { species = 0 }))
            assert.is_nil(A.GetMarketInfo(25, { species = 999 }))
        end)

        it("takes junk options as no options, and never throws", function()
            local A = loaded()
            local junk = { "item", 25, true, false }
            for i = 1, #junk do
                local info = A.GetMarketInfo(25, junk[i])
                assert.is_not_nil(info)
                assert.equals(25, info.itemID)
                assert.equals(7030, info.value)
            end
        end)

        it("answers a fresh table on every call, all the way down", function()
            local A = loaded()
            local first = A.GetMarketInfo(194784, { category = "commodity" })
            local second = A.GetMarketInfo(194784, { category = "commodity" })
            assert.not_equals(first, second)
            assert.not_equals(first.craft, second.craft)
            assert.not_equals(first.deal, second.deal)
            assert.not_equals(first.spans, second.spans)
            assert.not_equals(first.spans.d1, second.spans.d1)
            first.value = 1
            first.market = "gone"
            first.craft.cost = 0
            first.deal.rank = 0
            first.spans.d1.n = 0
            first.spans.d1.median = 0
            first.spans.d99 = {}
            local third = A.GetMarketInfo(194784, { category = "commodity" })
            assert.equals(6137000, second.value)
            assert.equals(TARGET, second.market)
            assert.equals(5550000, second.craft.cost)
            assert.equals(12, second.deal.rank)
            assert.equals(618, second.spans.d1.n)
            assert.equals(4010000, third.spans.d1.median)
            assert.is_nil(third.spans.d99)
            -- The store's own answer never changed either.
            assert.equals(618, A.store:record(194784, "commodity").spans.d1.n)
        end)

        it("derives value and valueSource as section 5.8 does, for every golden record", function()
            local DERIVED = {
                { "items[25]", 25, nil, 7030, "listed" },
                { "items[117]", 117, nil, 9500000000, "listed" },
                { "items[2589]", 2589, nil, 3200, "sold" },
                { "items[194784]", 194784, "item", 4400000, "d7" },
                { "commodities[191460]", 191460, nil, 80000, "listed" },
                { "commodities[191461]", 191461, nil, 19, "sold" },
                { "commodities[194784]", 194784, "commodity", 6137000, "sold" },
            }
            for i = 1, #DERIVED do
                local case = DERIVED[i]
                local A = loaded()
                local info = A.GetMarketInfo(case[2], case[3] and { category = case[3] } or nil)
                assert.is_not_nil(info)
                assert.equals(case[4], info.value)
                assert.equals(case[5], info.valueSource)
                -- The same number Tekkub's global answers, which reads items first.
                if case[3] ~= "commodity" then
                    assert.equals(case[4], A.GetAuctionBuyout(case[2]))
                end
            end
            -- The two pets, through the two ways a caller has to name one.
            local A = loaded()
            assert.equals(8000000, A.GetMarketInfo(nil, { species = 293 }).value)
            assert.equals("d30", A.GetMarketInfo(nil, { species = 293 }).valueSource)
            assert.equals(2450000, A.GetMarketInfo(PET_LINK).value)
            assert.equals("sold", A.GetMarketInfo(PET_LINK).valueSource)
            assert.equals(8000000, A.GetAuctionBuyout("battlepet:293"))
            assert.equals(2450000, A.GetAuctionBuyout(PET_LINK))
        end)
    end)

    describe("GetSpans", function()
        it("hands out the v3 span table of the record", function()
            local A = loaded()
            assert.same(oracle.items[25].spans, A.GetSpans(25))
            assert.same(oracle.items[25].spans, A.GetSpans(LINK_25))
            -- An id in both pools: items first, or the pool named.
            assert.same(oracle.items[194784].spans, A.GetSpans(194784))
            assert.same(oracle.items[194784].spans, A.GetSpans(194784, "item"))
            assert.same(oracle.commodities[194784].spans, A.GetSpans(194784, "commodity"))
            assert.same(oracle.commodities[191460].spans, A.GetSpans(191460))
            -- A pet through its link.
            assert.same(oracle.pets[2546].spans, A.GetSpans(PET_LINK))
            assert.same(oracle.pets[293].spans, A.GetSpans("battlepet:293"))
        end)

        it("carries, of each block, only the fields that are present", function()
            local A = loaded()
            local spans = A.GetSpans(194784, "commodity")
            for _, block in pairs(spans) do
                for _, field in ipairs(keysOf(block)) do
                    assert.is_true(holds(SPAN_FIELDS, field), field .. " is not a v3 span field")
                end
            end
            local d1 = spans.d1
            assert.equals(618, d1.n)
            assert.equals(4010000, d1.median)
            assert.equals(3617 / 10000, d1.saleRate)
            assert.equals(2, d1.daysSinceLastSale)
            assert.equals(61, d1.closuresSinceLastSale)
            assert.equals(9, d1.qtySold)
            assert.is_nil(d1.lows)
            -- A block with the listing stats and a present zero rate: no ledger fields.
            -- Nothing of it was listed today, so there is no d1 block at all.
            local bare = A.GetSpans(194784)
            assert.is_nil(bare.d1)
            assert.equals(5, bare.d7.n)
            assert.equals(0, bare.d7.saleRate)
            assert.is_nil(bare.d7.qtySold)
            assert.is_nil(bare.d7.saleRate180d)
            -- The 12 v3 listing fields, plus the one rate that is present.
            assert.equals(13, #keysOf(bare.d7))
        end)

        it("answers a fresh table on every call", function()
            local A = loaded()
            local first = A.GetSpans(25)
            local second = A.GetSpans(25)
            assert.not_equals(first, second)
            assert.not_equals(first.d1, second.d1)
            first.d1.n = 0
            first.d30 = nil
            assert.equals(58, second.d1.n)
            assert.equals(2275, A.GetSpans(25).d30.median)
            assert.equals(58, A.store:record(25).spans.d1.n)
        end)
    end)

    describe("stale data", function()
        it("is flagged, hidden from GetSpans and still described", function()
            local A = loaded()
            assert.is_not_nil(A.GetSpans(25))
            A.clock = function()
                return GENERATED_AT + 2 * DAY
            end
            local info = A.GetMarketInfo(194784, { category = "commodity" })
            assert.is_true(info.stale)
            assert.equals(2 * DAY, info.ageSeconds)
            assert.equals(6137000, info.value)
            assert.is_not_nil(info.spans.d1)
            -- The sniper's evidence is withheld; the tooltip's data is not (SPEC 5.9).
            assert.is_nil(A.GetSpans(194784, "commodity"))
            assert.is_nil(A.GetSpans(194784))
            assert.is_nil(A.GetSpans(25))
            assert.is_nil(A.GetSpans(PET_LINK))
            assert.is_nil(A.GetSpans("battlepet:293"))
            local markets = A.GetMarkets()
            assert.is_true(markets[1].stale)
            assert.equals(2 * DAY, markets[1].ageSeconds)
            assert.equals(TARGET, A.GetPrimaryMarket())
            -- The reference value is not live evidence: Tekkub's global keeps answering.
            assert.equals(7030, A.GetAuctionBuyout(25))
        end)

        it("hides the spans of the stale market only", function()
            local A = loaded()
            local late = GENERATED_AT + 2 * DAY
            A.clock = function()
                return late
            end
            secondMarket(A, OTHER, late - 600, ITEM_25)
            -- The golden market is the primary: stale, so nil.
            assert.is_nil(A.GetSpans(25))
            assert.is_true(A.GetMarkets()[1].stale)
            -- The other market is fresh: once it is the primary, its spans answer.
            assert.is_true(A.store:setPrimary(OTHER))
            assert.same(oracle.items[25].spans, A.GetSpans(25))
            assert.equals(OTHER, A.GetMarketInfo(25).market)
            assert.equals(OTHER, A.GetPrimaryMarket())
        end)

        it("is not stale at exactly one day", function()
            local A = loaded()
            A.clock = function()
                return GENERATED_AT + DAY
            end
            assert.is_false(A.GetMarketInfo(25).stale)
            assert.is_not_nil(A.GetSpans(25))
            assert.is_false(A.GetMarkets()[1].stale)
        end)
    end)

    describe("unknown items", function()
        it("answer nil from every getter", function()
            local A = loaded()
            local unknown = { 999999, 0, -5, 1.5, "junk", "", "item:", "item:abc", "battlepet:", true, {} }
            for i = 1, #unknown + 1 do
                local item = unknown[i]
                assert.is_nil(A.GetMarketInfo(item), "GetMarketInfo " .. tostring(item))
                assert.is_nil(A.GetMarketInfo(item, { category = "item" }), "GetMarketInfo item " .. tostring(item))
                assert.is_nil(A.GetMarketInfo(item, { category = "commodity" }))
                assert.is_nil(A.GetSpans(item), "GetSpans " .. tostring(item))
                assert.is_nil(A.GetSpans(item, "commodity"))
                assert.is_nil(A.GetAuctionBuyout(item), "GetAuctionBuyout " .. tostring(item))
            end
            -- Known ids that are not in the feed.
            assert.is_nil(A.GetMarketInfo(1))
            assert.is_nil(A.GetSpans(1))
            assert.is_nil(A.GetAuctionBuyout(1))
            -- A pet id is never an item id.
            assert.is_nil(A.GetMarketInfo(2546))
            assert.is_nil(A.GetMarketInfo(293))
        end)
    end)

    describe("GetMarkets", function()
        it("lists the markets in slot order with exactly the documented fields", function()
            local A = loaded()
            secondMarket(A, OTHER, NOW - 600)
            local markets = A.GetMarkets()
            assert.equals(2, #markets)
            assert.same({
                target = TARGET,
                slot = 1,
                records = 9,
                generatedAt = GENERATED_AT,
                ageSeconds = NOW - GENERATED_AT,
                stale = false,
            }, markets[1])
            assert.same({
                target = OTHER,
                slot = 2,
                records = 0,
                generatedAt = NOW - 600,
                ageSeconds = 600,
                stale = false,
            }, markets[2])
            sameList({ "ageSeconds", "generatedAt", "records", "slot", "stale", "target" }, keysOf(markets[1]))
            -- Fresh tables on every call, in the order the store keeps.
            local again = A.GetMarkets()
            assert.not_equals(markets, again)
            assert.not_equals(markets[1], again[1])
            markets[1].target = "changed"
            assert.equals(TARGET, again[1].target)
        end)

        it("counts the records of a market and flags the stale ones", function()
            local A = loaded()
            secondMarket(
                A,
                OTHER,
                NOW - 3 * DAY,
                "m.items[25] = \"" .. words.items[25] .. "\"\nm.pets[293] = \"" .. words.pets[293] .. "\"\n"
            )
            local markets = A.GetMarkets()
            assert.equals(2, markets[2].records)
            assert.is_false(markets[1].stale)
            assert.is_true(markets[2].stale)
            assert.equals(3 * DAY, markets[2].ageSeconds)
        end)

        it("answers the primary target, the chosen one when it is loaded", function()
            local A = loaded()
            assert.equals(TARGET, A.GetPrimaryMarket())
            secondMarket(A, OTHER, NOW - 600, ITEM_25)
            assert.equals(TARGET, A.GetPrimaryMarket())
            assert.is_true(A.store:setPrimary(OTHER))
            assert.equals(OTHER, A.GetPrimaryMarket())
            assert.equals(OTHER, A.GetMarketInfo(25).market)
            -- A choice that names no loaded market falls back to the first slot.
            A.store:setExplicitPrimary("not-loaded")
            assert.equals(TARGET, A.GetPrimaryMarket())
        end)
    end)

    describe("OnUpdate", function()
        it("runs the callback when the store fires, until it is unsubscribed", function()
            local A = loaded()
            local calls = 0
            local unsubscribe = A.OnUpdate(function()
                calls = calls + 1
            end)
            assert.equals("function", type(unsubscribe))
            -- The carrier registered before the subscription: nothing to catch up.
            assert.equals(0, calls)
            A.store:fireUpdate()
            assert.equals(1, calls)
            -- A market that registers fires too.
            secondMarket(A, OTHER, NOW - 600)
            assert.equals(2, calls)
            unsubscribe()
            unsubscribe()
            A.store:fireUpdate()
            secondMarket(A, OTHER, NOW - 300)
            assert.equals(2, calls)
            -- The callbacks are the store's: one list for both doors.
            local other = 0
            A.store:onUpdate(function()
                other = other + 1
            end)
            local off = A.OnUpdate(function()
                other = other + 10
            end)
            A.store:fireUpdate()
            assert.equals(11, other)
            off()
            A.store:fireUpdate()
            assert.equals(12, other)
        end)

        it("takes no arguments", function()
            local A = loaded()
            local seen = "unset"
            local unsubscribe = A.OnUpdate(function(...)
                seen = select("#", ...)
            end)
            A.store:fireUpdate()
            unsubscribe()
            assert.equals(0, seen)
        end)
    end)

    describe("GetAuctionBuyout", function()
        it("is published, and answers the reference value in copper", function()
            local A = loaded()
            assert.equals(A.GetAuctionBuyout, rawget(_G, "GetAuctionBuyout"))
            assert.equals(7030, A.GetAuctionBuyout(25))
            assert.equals(7030, A.GetAuctionBuyout("25"))
            assert.equals(7030, A.GetAuctionBuyout("item:25"))
            assert.equals(7030, A.GetAuctionBuyout(LINK_25))
            -- The tooltip's default pool: items first.
            assert.equals(4400000, A.GetAuctionBuyout(194784))
            assert.equals(4400000, A.GetAuctionBuyout(ITEM_LINK))
            -- A pet, through its link.
            assert.equals(2450000, A.GetAuctionBuyout(PET_LINK))
            -- Through the global, which is the same function.
            assert.equals(19, rawget(_G, "GetAuctionBuyout")(191461))
        end)

        it("chains to the GetAuctionBuyout an earlier addon defined, for what it cannot answer", function()
            local asked = {}
            local function previous(item)
                asked[#asked + 1] = item
                return 4242
            end
            local A = loaded(previous)
            assert.equals(7030, A.GetAuctionBuyout(25))
            assert.same({}, asked)
            assert.equals(4242, A.GetAuctionBuyout(999999))
            assert.same({ 999999 }, asked)
            -- The earlier addon gets the item exactly as it was handed over.
            assert.equals(4242, rawget(_G, "GetAuctionBuyout")("junk"))
            assert.same({ 999999, "junk" }, asked)
            assert.equals(4242, A.GetAuctionBuyout("battlepet:999"))
            assert.equals(3, #asked)
        end)

        it("answers nil when nobody can: no earlier function, or one that is not a function", function()
            local A = loaded()
            assert.is_nil(A.GetAuctionBuyout(999999))
            local B = loaded(7)
            assert.is_nil(B.GetAuctionBuyout(999999))
            assert.equals(B.GetAuctionBuyout, rawget(_G, "GetAuctionBuyout"))
        end)
    end)

    describe("absent addon data", function()
        it("answers nil and empty arrays from every getter, without throwing", function()
            local A = api()
            assert.is_nil(A.GetMarketInfo(25))
            assert.is_nil(A.GetMarketInfo(25, { category = "item" }))
            assert.is_nil(A.GetMarketInfo(PET_LINK))
            assert.is_nil(A.GetMarketInfo(nil, { species = 293 }))
            assert.is_nil(A.GetMarketInfo(25, { market = TARGET }))
            assert.is_nil(A.GetSpans(25))
            assert.is_nil(A.GetSpans(25, "item"))
            assert.same({}, A.GetMarkets())
            assert.equals(0, #A.GetMarkets())
            assert.is_nil(A.GetPrimaryMarket())
            assert.is_nil(A.GetAuctionBuyout(25))
            assert.equals(A, rawget(_G, "AuctiMoggerDatabase"))
            assert.equals("0.1.1", A.VERSION)
            -- The subscription works, and fires when a market finally registers.
            local calls = 0
            A.OnUpdate(function()
                calls = calls + 1
            end)
            A.store:fireUpdate()
            assert.equals(1, calls)
            Loader.carrier(A, GOLDEN)
            assert.equals(2, calls)
            assert.equals(7030, A.GetMarketInfo(25).value)
            assert.equals(1, #A.GetMarkets())
        end)

        it("still chains GetAuctionBuyout to an earlier addon", function()
            local A = api(function()
                return 4242
            end)
            -- Nothing is loaded, so everything is handed on.
            assert.equals(4242, A.GetAuctionBuyout(25))
            assert.equals(4242, A.GetAuctionBuyout(999999))
        end)
    end)
end)
