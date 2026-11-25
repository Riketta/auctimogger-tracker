-- Specs for Core/market.lua: the rules of SPEC 5.9, the store of SPEC 7.3.2
-- and the "market" items of SPEC 10.
--
-- Core/record.lua has its own spec. A stub stands in for the decoder here and
-- is installed in the namespace before market.lua loads, because the module
-- binds A.Core.Record at load time. The stub answers a fresh marker table for
-- the real carrier words of the golden fixture and refuses everything else.

local Loader = require("spec.helpers.loader")

local NOW = 1795560000
local DAY = 86400
local MISSING = {}

local decodeCalls = 0
local KNOWN = {}
local BOOM = "stub: makes the decoder throw"

---@param text string
---@return table|nil record
---@return string|nil message
local function stubDecode(text)
    decodeCalls = decodeCalls + 1
    if text == BOOM then
        error("stub: decoder blew up")
    end
    if KNOWN[text] then
        return { word = text }
    end
    return nil, "stub: undecodable"
end

--- Runs the golden carrier the way the client runs a slot file, against a
--- namespace that only captures what the carrier fills.
---@return table captured `{ meta, items, commodities, pets }`
local function runFixture()
    local captured
    local A = {
        BeginMarket = function(meta)
            captured = { meta = meta, items = {}, commodities = {}, pets = {} }
            return captured
        end,
        EndMarket = function() end,
    }
    Loader.carrier(A, Loader.FIXTURES .. "/database_v4.lua")
    return captured
end

local fixture = runFixture()
for _, table_ in pairs({ fixture.items, fixture.commodities, fixture.pets }) do
    for _, word in pairs(table_) do
        KNOWN[word] = true
    end
end

local ITEM_WORD = fixture.items[194784]
local COMMODITY_WORD = fixture.commodities[194784]
local PET_WORD = fixture.pets[2546]

--- A fresh namespace with market.lua loaded into it, and a fresh store.
---@return AmdbMarketModule Market
---@return AmdbStore store
local function fresh()
    local A = Loader.namespace()
    A.Core = { Record = { decode = stubDecode } }
    Loader.load(A, "Core/market.lua")
    local Market = A.Core.Market
    return Market, Market.Store.new()
end

--- A valid header for market "alpha", 25 minutes old, with overrides. A value
--- of MISSING removes the field.
---@param patch table<string, any>|nil
---@return table meta
local function header(patch)
    local meta = {
        version = 4,
        trackerVersion = "0.1.0",
        target = "alpha",
        flavor = "forever",
        region = "eu",
        rulesetId = 1,
        houseId = 111,
        generatedAt = NOW - 1500,
        auctionsAt = NOW - 1500,
        commoditiesAt = NOW - 3900,
    }
    if patch then
        for key, value in pairs(patch) do
            if value == MISSING then
                meta[key] = nil
            else
                meta[key] = value
            end
        end
    end
    return meta
end

--- Commits the builder that is already open and insists that it registered.
---@param store AmdbStore
---@return AmdbMarket market
---@return string[] reports
local function commitOpen(store)
    local market, reports = store:commit(NOW)
    if market == nil then
        error("commitOpen: the market was rejected: " .. tostring(reports[1]))
    end
    return market, reports
end

--- Begins a market, copies the given records into its builder and commits.
---@param store AmdbStore
---@param meta any the header, well-formed or deliberately not
---@param records table|nil `{ items = {...}, commodities = {...}, pets = {...} }`
---@return AmdbMarket|nil market
---@return string[] reports
local function commit(store, meta, records)
    local builder = store:begin(meta)
    if records then
        for _, name in ipairs({ "items", "commodities", "pets" }) do
            local source = records[name]
            if source then
                for id, text in pairs(source) do
                    builder[name][id] = text
                end
            end
        end
    end
    return store:commit(NOW)
end

--- `commit` for a market that must register: the tests that read the market
--- then need no nil checks.
---@param store AmdbStore
---@param meta any
---@param records table|nil
---@return AmdbMarket market
---@return string[] reports
local function load(store, meta, records)
    local market, reports = commit(store, meta, records)
    if market == nil then
        error("load: the market was rejected: " .. tostring(reports[1]))
    end
    return market, reports
end

--- Calls a module function with a deliberately wrong-typed argument: the
--- module must answer, never throw. Typed `any` so the type checker does not
--- flag exactly the wrong type the test means to pass.
---@param fn function
---@param ... any
---@return any
local function withJunk(fn, ...)
    return fn(...)
end

---@param value table
---@return string[] keys sorted
local function keys(value)
    local list = {}
    for key in pairs(value) do
        list[#list + 1] = key
    end
    table.sort(list)
    return list
end

---@param store AmdbStore
---@return string[] targets in slot order
local function targets(store)
    local list = {}
    for i, market in ipairs(store:markets()) do
        list[i] = market.target
    end
    return list
end

describe("Market module", function()
    it("exposes the contract constants and classes", function()
        local Market, store = fresh()
        assert.equals(4, Market.FEED_VERSION)
        assert.equals(86400, Market.STALE_AFTER_SECONDS)
        assert.equals(300, Market.FUTURE_TOLERANCE_SECONDS)
        assert.same({ "item", "commodity", "pet" }, Market.CATEGORIES)
        assert.is_table(Market.Market)
        assert.is_table(Market.Store)
        assert.is_table(store)
    end)

    it("registers into the namespace and returns its table", function()
        local A = Loader.namespace()
        A.Core = { Record = { decode = stubDecode } }
        local chunk = assert(loadfile(Loader.ROOT .. "/Core/market.lua"))
        local Market = chunk(Loader.ADDON, A)
        assert.equals(A.Core.Market, Market)
        assert.equals(Market.Store, Market.Store)
        assert.equals("AuctiMoggerDatabase", Loader.ADDON)
    end)
end)

describe("Store: the version gate", function()
    it("accepts version 4", function()
        local _, store = fresh()
        local market, reports = load(store, header())
        assert.same({}, reports)
        assert.equals(4, market.meta.version)
        assert.equals(1, #store:markets())
    end)

    it("rejects 3, 5, the string \"4\" and a missing version", function()
        local _, store = fresh()
        local market, reports = commit(store, header({ version = 3 }))
        assert.is_nil(market)
        assert.same({ "market \"alpha\" rejected: feed version 3 is not supported (expected 4)" }, reports)
        market, reports = commit(store, header({ version = 5 }))
        assert.is_nil(market)
        assert.same({ "market \"alpha\" rejected: feed version 5 is not supported (expected 4)" }, reports)
        market, reports = commit(store, header({ version = "4" }))
        assert.is_nil(market)
        assert.same({ "market \"alpha\" rejected: feed version \"4\" is not supported (expected 4)" }, reports)
        market, reports = commit(store, header({ version = MISSING }))
        assert.is_nil(market)
        assert.same({ "market \"alpha\" rejected: feed version is missing (expected 4)" }, reports)
        assert.same({}, store:markets())
        assert.equals(4, #store:rejected())
    end)
end)

describe("Store: required header fields", function()
    local function rejects(patch, expected)
        local _, store = fresh()
        local market, reports = commit(store, header(patch))
        assert.is_nil(market)
        assert.same({ expected }, reports)
        assert.same({}, store:markets())
        assert.equals(1, #store:rejected())
    end

    it("rejects a missing or malformed trackerVersion", function()
        rejects(
            { trackerVersion = MISSING },
            "market \"alpha\" rejected: header field trackerVersion is missing or malformed"
        )
        rejects(
            { trackerVersion = "" },
            "market \"alpha\" rejected: header field trackerVersion is missing or malformed"
        )
        rejects(
            { trackerVersion = 7 },
            "market \"alpha\" rejected: header field trackerVersion is missing or malformed"
        )
    end)

    it("rejects a missing or malformed target", function()
        rejects({ target = MISSING }, "market rejected: header field target is missing or malformed")
        rejects({ target = "" }, "market \"\" rejected: header field target is missing or malformed")
        rejects({ target = 42 }, "market \"42\" rejected: header field target is missing or malformed")
        rejects({ target = {} }, "market rejected: header field target is missing or malformed")
    end)

    it("rejects a target outside the tracker's id grammar", function()
        -- The tracker's own rule for a target id (crates/core/src/config.rs). A
        -- target is printed in chat and in tooltips, so it may hold id
        -- characters only. The classes are ASCII, not the locale: an uppercase
        -- id, or one with a path separator, never comes from a tracker.
        local reason = "header field target is not a tracker target id ([a-z0-9][a-z0-9._-]*)"
        rejects({ target = "Forever-EU" }, "market \"Forever-EU\" rejected: " .. reason)
        rejects({ target = "forever eu" }, "market \"forever eu\" rejected: " .. reason)
        rejects({ target = "forever/eu" }, "market \"forever/eu\" rejected: " .. reason)
        rejects({ target = ".." }, "market \"..\" rejected: " .. reason)
        rejects({ target = "-eu" }, "market \"-eu\" rejected: " .. reason)

        -- A color escape or a control character is exactly what the rule keeps
        -- out of chat. The report names such a target only after cleaning it,
        -- so here the recorded reason is what is compared.
        local _, store = fresh()
        local escape = string.char(27) .. "[31m"
        for _, target in ipairs({ "eu|r", "eu\n", escape }) do
            local market = commit(store, header({ target = target }))
            assert.is_nil(market)
            local rejections = store:rejected()
            assert.equals(reason, rejections[#rejections].reason)
        end
        assert.equals(3, #store:rejected())

        -- What the grammar admits is what a tracker can be configured with.
        for _, target in ipairs({ "a", "0", "eu", "forever-eu-pvp-blackwater", "eu_1.x-y" }) do
            local _, permissive = fresh()
            local market = commit(permissive, header({ target = target }))
            assert.is_true(market ~= nil and market.target == target, target)
        end
    end)

    it("rejects a missing or malformed generatedAt", function()
        local bad = "market \"alpha\" rejected: header field generatedAt is missing or malformed"
        rejects({ generatedAt = MISSING }, bad)
        rejects({ generatedAt = "1795559034" }, bad)
        rejects({ generatedAt = 1795559034.5 }, bad)
        rejects({ generatedAt = 0 / 0 }, bad)
        rejects({ generatedAt = math.huge }, bad)
        rejects({ generatedAt = -math.huge }, bad)
        rejects({ generatedAt = true }, bad)
    end)

    it("names every malformed field at once", function()
        local _, store = fresh()
        local market, reports = commit(store, header({ trackerVersion = MISSING, generatedAt = "x" }))
        assert.is_nil(market)
        assert.same(
            { "market \"alpha\" rejected: header fields trackerVersion, generatedAt are missing or malformed" },
            reports
        )
        market, reports = commit(store, header({ trackerVersion = 1, target = {}, generatedAt = 1.5 }))
        assert.is_nil(market)
        assert.same(
            { "market rejected: header fields trackerVersion, target, generatedAt are missing or malformed" },
            reports
        )
    end)

    it("rejects a missing or non-table header", function()
        local _, store = fresh()
        local market, reports = commit(store, nil)
        assert.is_nil(market)
        assert.same({ "market rejected: header is missing" }, reports)
        market, reports = commit(store, "version = 4")
        assert.is_nil(market)
        assert.same({ "market rejected: header is missing" }, reports)
        assert.same({}, store:markets())
    end)

    it("keeps a valid market when only the required fields are present", function()
        local _, store = fresh()
        local market, reports =
            load(store, { version = 4, trackerVersion = "0.1.0", target = "bare", generatedAt = NOW - 60 })
        assert.same({}, reports)
        assert.same({ "generatedAt", "target", "trackerVersion", "version" }, keys(market.meta))
    end)
end)

describe("Store: optional header fields", function()
    it("drops a wrong-typed optional field without rejecting the market", function()
        local _, store = fresh()
        local market, reports = load(
            store,
            header({
                flavor = 1,
                region = true,
                rulesetId = "1",
                houseId = 1.5,
                connectedRealmId = {},
                auctionsAt = "x",
                commoditiesAt = 0 / 0,
            })
        )
        assert.same({}, reports)
        assert.is_nil(market.meta.flavor)
        assert.is_nil(market.meta.region)
        assert.is_nil(market.meta.rulesetId)
        assert.is_nil(market.meta.houseId)
        assert.is_nil(market.meta.connectedRealmId)
        assert.is_nil(market.meta.auctionsAt)
        assert.is_nil(market.meta.commoditiesAt)
        assert.equals("alpha", market.meta.target)
        assert.equals(NOW - 1500, market.meta.generatedAt)
    end)

    it("keeps well-typed optional fields and ignores unknown keys", function()
        local _, store = fresh()
        local market = load(store, header({ connectedRealmId = 5, junk = "ignored", nested = { 1 } }))
        assert.equals("forever", market.meta.flavor)
        assert.equals("eu", market.meta.region)
        assert.equals(1, market.meta.rulesetId)
        assert.equals(111, market.meta.houseId)
        assert.equals(5, market.meta.connectedRealmId)
        assert.equals(NOW - 1500, market.meta.auctionsAt)
        assert.equals(NOW - 3900, market.meta.commoditiesAt)
        assert.is_nil(rawget(market.meta, "junk"))
        assert.is_nil(rawget(market.meta, "nested"))
        assert.same({
            "auctionsAt",
            "commoditiesAt",
            "connectedRealmId",
            "flavor",
            "generatedAt",
            "houseId",
            "region",
            "rulesetId",
            "target",
            "trackerVersion",
            "version",
        }, keys(market.meta))
    end)

    it("hands out a copy: the header the carrier wrote is not kept alive", function()
        local _, store = fresh()
        local meta = header()
        local market = load(store, meta)
        meta.target = "changed"
        meta.generatedAt = 1
        assert.equals("alpha", market.meta.target)
        assert.equals(NOW - 1500, market.meta.generatedAt)
        assert.not_equals(meta, market.meta)
    end)
end)

describe("Store: the future gate", function()
    it("rejects a generatedAt more than 5 minutes ahead of now", function()
        local _, store = fresh()
        local market, reports = commit(store, header({ generatedAt = NOW + 301 }))
        assert.is_nil(market)
        assert.same({ "market \"alpha\" rejected: generatedAt is 301 s in the future (diverged generator)" }, reports)
        market, reports = commit(store, header({ generatedAt = NOW + 600 }))
        assert.is_nil(market)
        assert.same({ "market \"alpha\" rejected: generatedAt is 600 s in the future (diverged generator)" }, reports)
        assert.same({}, store:markets())
        assert.equals("generatedAt is 600 s in the future (diverged generator)", store:rejected()[2].reason)
    end)

    it("keeps a generatedAt exactly 5 minutes ahead", function()
        local _, store = fresh()
        local market, reports = load(store, header({ generatedAt = NOW + 300 }))
        assert.same({}, reports)
        assert.equals(-300, market:ageSeconds(NOW))
        assert.is_false(market:isStale(NOW))
    end)
end)

describe("Market: the stale flag", function()
    it("flips at the 24 h boundary, on both sides", function()
        local _, store = fresh()
        local freshMarket = load(store, header({ target = "edge", generatedAt = NOW - DAY }))
        assert.equals(DAY, freshMarket:ageSeconds(NOW))
        assert.is_false(freshMarket:isStale(NOW))
        assert.is_false(freshMarket:stats(NOW).stale)

        local oldMarket = load(store, header({ target = "old", generatedAt = NOW - DAY - 1 }))
        assert.equals(DAY + 1, oldMarket:ageSeconds(NOW))
        assert.is_true(oldMarket:isStale(NOW))
        assert.is_true(oldMarket:stats(NOW).stale)
    end)

    it("is computed against the caller's clock, never stored", function()
        local _, store = fresh()
        local market = load(store, header({ generatedAt = NOW - 100 }), { items = { [194784] = ITEM_WORD } })
        assert.is_false(market:isStale(NOW))
        assert.is_false(market:isStale(NOW + DAY - 100))
        assert.is_true(market:isStale(NOW + DAY - 99))
        -- Stale data is shown, not hidden: the records are still there a day later.
        assert.same({ word = ITEM_WORD }, store:record(194784))
        assert.equals(NOW - 100, market.generatedAt)
    end)
end)

describe("Store: registration", function()
    it("numbers markets in load order", function()
        local _, store = fresh()
        assert.equals(1, load(store, header({ target = "alpha" })).slot)
        assert.equals(2, load(store, header({ target = "beta" })).slot)
        assert.equals(3, load(store, header({ target = "gamma" })).slot)
        assert.same({ "alpha", "beta", "gamma" }, targets(store))
        local slots = {}
        for i, market in ipairs(store:markets()) do
            slots[i] = market.slot
        end
        assert.same({ 1, 2, 3 }, slots)
    end)

    it("lets a rejected market keep its slot number, so the numbers match the slot files", function()
        local _, store = fresh()
        commit(store, header({ version = 3 }))
        local market = load(store, header())
        assert.equals(2, market.slot)
        assert.equals(1, store:rejected()[1].slot)
        assert.equals(3, load(store, header({ target = "beta" })).slot)
    end)

    it("replaces an earlier market with the same target and reports it", function()
        local _, store = fresh()
        local first = commit(store, header({ generatedAt = NOW - 9000 }))
        commit(store, header({ target = "beta" }))
        commit(store, header({ target = "gamma" }))
        local second, reports = load(store, header({ generatedAt = NOW - 60 }))
        assert.same({ "market \"alpha\" from slot 4 replaces slot 1" }, reports)
        assert.equals(4, second.slot)
        assert.same({ "beta", "gamma", "alpha" }, targets(store))
        assert.equals(second, store:get("alpha"))
        assert.not_equals(first, store:get("alpha"))
        -- One market per target: the earlier one is gone, not hidden.
        assert.equals(3, #store:markets())
        for _, market in ipairs(store:markets()) do
            assert.not_equals(first, market)
        end
        -- What lookups answer is the later market's data. The replaced target now
        -- sits in the last slot, so with no explicit choice the primary is beta.
        assert.equals(60, store:get("alpha"):ageSeconds(NOW))
        assert.equals("beta", store:primaryTarget())
    end)

    it("answers nil for an unknown target", function()
        local _, store = fresh()
        commit(store, header())
        assert.not_nil(store:get("alpha"))
        assert.is_nil(store:get("nope"))
        assert.is_nil(store:get(nil))
    end)

    it("hands out a fresh market list that may be kept", function()
        local _, store = fresh()
        commit(store, header())
        local list = store:markets()
        rawset(list, #list + 1, "junk")
        assert.equals(1, #store:markets())
    end)
end)

describe("Store: the primary market", function()
    it("is nil with nothing loaded, and never ambiguous", function()
        local _, store = fresh()
        assert.is_nil(store:primary())
        assert.is_nil(store:primaryTarget())
        assert.is_nil(store:explicitPrimary())
        assert.is_false(store:primaryIsAmbiguous())
    end)

    it("is the only loaded market", function()
        local _, store = fresh()
        local market = load(store, header())
        assert.equals(market, store:primary())
        assert.equals("alpha", store:primaryTarget())
        assert.is_false(store:primaryIsAmbiguous())
    end)

    it("is the first market in slot order when several are loaded and nothing is chosen", function()
        local _, store = fresh()
        commit(store, header({ target = "alpha" }))
        commit(store, header({ target = "beta" }))
        commit(store, header({ target = "gamma" }))
        assert.equals("alpha", store:primaryTarget())
        assert.is_true(store:primaryIsAmbiguous())
    end)

    it("is the explicit choice while that target is loaded", function()
        local _, store = fresh()
        commit(store, header({ target = "alpha" }))
        local beta = commit(store, header({ target = "beta" }))
        assert.is_true(store:setPrimary("beta"))
        assert.equals(beta, store:primary())
        assert.equals("beta", store:primaryTarget())
        assert.equals("beta", store:explicitPrimary())
        assert.is_false(store:primaryIsAmbiguous())
    end)

    it("refuses setPrimary for a target that is not loaded", function()
        local _, store = fresh()
        commit(store, header({ target = "alpha" }))
        commit(store, header({ target = "beta" }))
        local ok, message = store:setPrimary("delta")
        assert.is_false(ok)
        assert.equals("market \"delta\" is not loaded", message)
        assert.equals("alpha", store:primaryTarget())
        assert.is_nil(store:explicitPrimary())
        ok, message = store:setPrimary(nil)
        assert.is_false(ok)
        assert.equals("market id is required", message)
    end)

    it("falls back when the explicit choice names no loaded market", function()
        local _, store = fresh()
        commit(store, header({ target = "alpha" }))
        commit(store, header({ target = "beta" }))
        store:setExplicitPrimary("gone")
        assert.equals("gone", store:explicitPrimary())
        assert.equals("alpha", store:primaryTarget())
        assert.is_true(store:primaryIsAmbiguous())
        store:setExplicitPrimary(nil)
        assert.is_nil(store:explicitPrimary())
        assert.equals("alpha", store:primaryTarget())
    end)

    it("keeps the choice through a replacing registration of the same target", function()
        local _, store = fresh()
        commit(store, header({ target = "alpha" }))
        store:setPrimary("alpha")
        commit(store, header({ target = "beta" }))
        local again = commit(store, header({ target = "alpha" }))
        assert.equals(again, store:primary())
        assert.is_false(store:primaryIsAmbiguous())
    end)

    it("reads through the primary market by default", function()
        local _, store = fresh()
        commit(store, header({ target = "alpha" }), {
            items = { [194784] = ITEM_WORD },
            pets = { [2546] = PET_WORD },
        })
        commit(store, header({ target = "beta" }), { items = { [194784] = COMMODITY_WORD } })
        assert.same({ word = ITEM_WORD }, store:record(194784))
        assert.same({ word = PET_WORD }, store:petRecord(2546))
        store:setPrimary("beta")
        assert.same({ word = COMMODITY_WORD }, store:record(194784))
        assert.is_nil(store:petRecord(2546))
    end)
end)

describe("Store: lookups", function()
    local store

    before_each(function()
        local _, freshStore = fresh()
        store = freshStore
        commit(store, header(), {
            items = { [194784] = ITEM_WORD, [25] = fixture.items[25] },
            commodities = { [194784] = COMMODITY_WORD, [191460] = fixture.commodities[191460] },
            pets = { [2546] = PET_WORD },
        })
    end)

    it("looks in items first, then commodities, when no category is given", function()
        assert.same({ word = ITEM_WORD }, store:record(194784))
        assert.same({ word = fixture.commodities[191460] }, store:record(191460))
        assert.same({ word = fixture.items[25] }, store:record(25))
    end)

    it("reads the category it is given, also for an id that is in both tables", function()
        assert.same({ word = ITEM_WORD }, store:record(194784, "item"))
        assert.same({ word = COMMODITY_WORD }, store:record(194784, "commodity"))
        assert.is_nil(store:record(191460, "item"))
        assert.is_nil(store:record(25, "commodity"))
    end)

    it("answers nil for a category that names no item table", function()
        assert.is_nil(store:record(194784, "pet"))
        assert.is_nil(store:record(194784, "items"))
        assert.is_nil(store:record(194784, ""))
        assert.is_nil(store:record(194784, 7))
    end)

    it("answers nil for an unknown id", function()
        assert.is_nil(store:record(1))
        assert.is_nil(store:record(999999, "item"))
        assert.is_nil(store:record(nil))
        assert.is_nil(store:record("194784"))
    end)

    it("reads the market it is told to, or the primary one", function()
        commit(store, header({ target = "beta" }), { items = { [194784] = COMMODITY_WORD } })
        assert.same({ word = ITEM_WORD }, store:record(194784, nil, "alpha"))
        assert.same({ word = COMMODITY_WORD }, store:record(194784, nil, "beta"))
        assert.same({ word = ITEM_WORD }, store:record(194784))
        assert.is_nil(store:record(194784, nil, "nope"))
        assert.same({ word = PET_WORD }, store:petRecord(2546, "alpha"))
        assert.is_nil(store:petRecord(2546, "beta"))
    end)

    it("answers nil with no market loaded", function()
        local _, empty = fresh()
        assert.is_nil(empty:record(194784))
        assert.is_nil(empty:record(194784, "item", "alpha"))
        assert.is_nil(empty:petRecord(2546))
    end)

    it("decodes on every call and hands out a fresh table", function()
        local before = decodeCalls
        local first = store:record(194784)
        local second = store:record(194784)
        assert.equals(before + 2, decodeCalls)
        assert.not_equals(first, second)
        assert.same(first, second)
        first.word = "kept by the caller"
        assert.equals(ITEM_WORD, store:record(194784).word)
        before = decodeCalls
        store:record(1)
        store:record(194784, nil, "nope")
        assert.equals(before, decodeCalls)
    end)

    it("decodes pets through the market as well", function()
        local market = store:get("alpha")
        assert.same({ word = PET_WORD }, market:petRecord(2546))
        assert.is_nil(market:petRecord(1))
        assert.same({ word = COMMODITY_WORD }, market:record(194784, "commodity"))
        assert.same({ word = ITEM_WORD }, market:record(194784))
    end)
end)

describe("Store: dropped records", function()
    it("drops malformed entries and undecodable records, counts them and keeps the market", function()
        local _, store = fresh()
        local market, reports = load(store, header(), {
            items = {
                [194784] = ITEM_WORD,
                [1] = "not a record",
                [2] = 42,
                [0] = ITEM_WORD,
                [-3] = ITEM_WORD,
                [2.5] = ITEM_WORD,
                ["7"] = ITEM_WORD,
                [8] = BOOM,
                [9] = "",
            },
            commodities = { [194784] = COMMODITY_WORD, [5] = "junk" },
            pets = { [2546] = PET_WORD },
        })
        assert.same({ "market \"alpha\": 9 record(s) dropped (undecodable)" }, reports)
        assert.equals(9, market.dropped)
        assert.equals(3, market:count())
        assert.equals(9, market:stats(NOW).dropped)
        assert.same({ word = ITEM_WORD }, store:record(194784))
        for _, id in ipairs({ 1, 2, 0, -3, 2.5, 8, 9, 5 }) do
            assert.is_nil(store:record(id))
        end
        assert.is_nil(store:record(7))
        assert.equals(market, store:get("alpha"))
    end)

    it("reports nothing when every record is kept", function()
        local _, store = fresh()
        local market, reports = load(store, header(), { items = { [194784] = ITEM_WORD } })
        assert.same({}, reports)
        assert.equals(0, market.dropped)
    end)

    it("keeps a market whose records were all dropped", function()
        local _, store = fresh()
        local market = load(store, header(), { items = { [1] = "junk" } })
        assert.equals(0, market:count())
        assert.equals(1, #store:markets())
    end)

    it("registers the golden fixture whole: nine records, none dropped", function()
        local _, store = fresh()
        local market, reports = load(store, fixture.meta, fixture)
        assert.same({}, reports)
        assert.equals("forever-eu-pvp-alliance", market.target)
        assert.equals(9, market:count())
        assert.equals(0, market.dropped)
        local stats = market:stats(NOW)
        assert.equals(4, stats.items)
        assert.equals(3, stats.commodities)
        assert.equals(2, stats.pets)
        assert.equals(9, stats.records)
        assert.same({ word = fixture.items[25] }, store:record(25))
        assert.same({ word = fixture.pets[293] }, store:petRecord(293))
    end)
end)

describe("Store: the builder", function()
    it("opens with the header and three empty tables", function()
        local _, store = fresh()
        local meta = header()
        local builder, replaced = store:begin(meta)
        assert.is_false(replaced)
        assert.equals(meta, builder.meta)
        assert.same({}, builder.items)
        assert.same({}, builder.commodities)
        assert.same({}, builder.pets)
        assert.equals(builder, store.builder)
    end)

    it("discards an unclosed builder when the next one opens", function()
        local _, store = fresh()
        local first = store:begin(header({ target = "unclosed" }))
        first.items[194784] = ITEM_WORD
        local second, replaced = store:begin(header())
        assert.is_true(replaced)
        assert.not_equals(first, second)
        local market, reports = commitOpen(store)
        assert.same({}, reports)
        assert.equals("alpha", market.target)
        assert.equals(0, market:count())
    end)

    it("is closed by a commit, accepted or not", function()
        local _, store = fresh()
        store:begin(header())
        store:commit(NOW)
        assert.is_nil(store.builder)
        store:begin(header({ version = 9 }))
        store:commit(NOW)
        assert.is_nil(store.builder)
    end)
end)

describe("Store: no open builder", function()
    it("answers nil and a report, and consumes no slot", function()
        local _, store = fresh()
        local market, reports = store:commit(NOW)
        assert.is_nil(market)
        assert.same({ "no open market to commit" }, reports)
        assert.same({}, store:rejected())
        local after = load(store, header())
        assert.equals(1, after.slot)
        market, reports = store:commit(NOW)
        assert.is_nil(market)
        assert.same({ "no open market to commit" }, reports)
        assert.equals(1, #store:markets())
    end)
end)

describe("Store: update callbacks", function()
    it("fire for a new market and for a replacing one", function()
        local _, store = fresh()
        local fired = 0
        store:onUpdate(function()
            fired = fired + 1
        end)
        commit(store, header())
        assert.equals(1, fired)
        commit(store, header({ target = "beta" }))
        assert.equals(2, fired)
        commit(store, header({ target = "alpha", generatedAt = NOW - 5 }))
        assert.equals(3, fired)
    end)

    it("see the new state when they run", function()
        local _, store = fresh()
        local seen = {}
        store:onUpdate(function()
            seen[#seen + 1] = store:primaryTarget() .. "/" .. #store:markets()
        end)
        commit(store, header({ target = "alpha" }))
        commit(store, header({ target = "beta" }))
        assert.same({ "alpha/1", "alpha/2" }, seen)
    end)

    it("never fire for a rejection or for a commit with nothing open", function()
        local _, store = fresh()
        local fired = 0
        store:onUpdate(function()
            fired = fired + 1
        end)
        commit(store, header({ version = 3 }))
        store:commit(NOW)
        assert.equals(0, fired)
    end)

    it("stop after unsubscribing; unsubscribing twice is harmless", function()
        local _, store = fresh()
        local mine, theirs = 0, 0
        local unsubscribe = store:onUpdate(function()
            mine = mine + 1
        end)
        store:onUpdate(function()
            theirs = theirs + 1
        end)
        commit(store, header())
        assert.equals(1, mine)
        assert.equals(1, theirs)
        unsubscribe()
        unsubscribe()
        commit(store, header({ target = "beta" }))
        assert.equals(1, mine)
        assert.equals(2, theirs)
    end)

    it("run every other listener when one raises", function()
        local _, store = fresh()
        local order = {}
        store:onUpdate(function()
            order[#order + 1] = "first"
        end)
        store:onUpdate(function()
            error("listener two is broken")
        end)
        store:onUpdate(function()
            order[#order + 1] = "third"
        end)
        commit(store, header())
        assert.same({ "first", "third" }, order)
        local errors = store:fireUpdate()
        assert.equals(1, #errors)
        assert.is_true(errors[1]:find("listener two is broken", 1, true) ~= nil)
        -- The commit fires "first", "third"; the explicit fireUpdate repeats them.
        assert.same({ "first", "third", "first", "third" }, order)
    end)

    it("run the same callback once per subscription", function()
        local _, store = fresh()
        local fired = 0
        local callback = function()
            fired = fired + 1
        end
        store:onUpdate(callback)
        store:onUpdate(callback)
        store:fireUpdate()
        assert.equals(2, fired)
    end)

    it("are copied before they run: a listener that unsubscribes or subscribes mid-pass is safe", function()
        local _, store = fresh()
        local fired = { late = 0 }
        -- Declared first: the callback unsubscribes itself, so it must close over
        -- the local that the assignment below fills.
        local unsubscribe
        unsubscribe = store:onUpdate(function()
            unsubscribe()
            store:onUpdate(function()
                fired.late = fired.late + 1
            end)
        end)
        store:fireUpdate()
        assert.equals(0, fired.late)
        store:fireUpdate()
        assert.equals(1, fired.late)
    end)

    it("refuse a callback that is not a function", function()
        local _, store = fresh()
        assert.has_error(function()
            withJunk(store.onUpdate, store, nil)
        end)
    end)

    it("fire when the primary market changes and not when it does not", function()
        local _, store = fresh()
        local fired = 0
        store:onUpdate(function()
            fired = fired + 1
        end)
        commit(store, header({ target = "alpha" }))
        commit(store, header({ target = "beta" }))
        assert.equals(2, fired)
        -- alpha is already the first in slot order: nothing changes.
        assert.is_true(store:setPrimary("alpha"))
        assert.equals(2, fired)
        assert.is_true(store:setPrimary("beta"))
        assert.equals(3, fired)
        assert.is_false(store:setPrimary("nope"))
        assert.equals(3, fired)
        -- The raw setter never fires: it loads a saved choice.
        store:setExplicitPrimary("alpha")
        assert.equals(3, fired)
    end)
end)

describe("Store: stats", function()
    it("lists every market in slot order with the documented fields", function()
        local _, store = fresh()
        commit(store, header({ target = "alpha", generatedAt = NOW - DAY - 5 }), {
            items = { [194784] = ITEM_WORD, [1] = "junk" },
            pets = { [2546] = PET_WORD },
        })
        commit(store, header({ target = "beta" }), { commodities = { [194784] = COMMODITY_WORD } })
        commit(store, header({ target = "gamma", version = 3 }))
        local stats = store:stats(NOW)
        assert.equals(2, #stats)
        assert.same({
            slot = 1,
            target = "alpha",
            records = 2,
            items = 1,
            commodities = 0,
            pets = 1,
            generatedAt = NOW - DAY - 5,
            ageSeconds = DAY + 5,
            stale = true,
            dropped = 1,
        }, stats[1])
        assert.same({
            slot = 2,
            target = "beta",
            records = 1,
            items = 0,
            commodities = 1,
            pets = 0,
            generatedAt = NOW - 1500,
            ageSeconds = 1500,
            stale = false,
            dropped = 0,
        }, stats[2])
        assert.same({
            "ageSeconds",
            "commodities",
            "dropped",
            "generatedAt",
            "items",
            "pets",
            "records",
            "slot",
            "stale",
            "target",
        }, keys(stats[1]))
        assert.equals(store:get("alpha"):stats(NOW).target, "alpha")
        assert.equals(store:get("beta"):count(), 1)
    end)

    it("is empty with nothing loaded", function()
        local _, store = fresh()
        assert.same({}, store:stats(NOW))
    end)
end)

describe("Market: snapshotAge", function()
    it("answers per kind from the header stamps", function()
        local _, store = fresh()
        local market = load(store, header())
        assert.equals(1500, market:snapshotAge("auctions", NOW))
        assert.equals(3900, market:snapshotAge("commodities", NOW))
        assert.equals(1500 + 60, market:snapshotAge("auctions", NOW + 60))
    end)

    it("answers nil for an absent stamp or an unknown kind", function()
        local _, store = fresh()
        local market = load(store, header({ auctionsAt = MISSING, commoditiesAt = "bad" }))
        assert.is_nil(market:snapshotAge("auctions", NOW))
        assert.is_nil(market:snapshotAge("commodities", NOW))
        assert.is_nil(market:snapshotAge("items", NOW))
        assert.is_nil(withJunk(market.snapshotAge, market, nil, NOW))
    end)
end)

describe("Store: rejected", function()
    it("remembers every refused commit with the slot it would have taken", function()
        local _, store = fresh()
        commit(store, header())
        commit(store, header({ version = 3 }))
        commit(store, nil)
        commit(store, header({ target = "ahead", generatedAt = NOW + 900 }))
        commit(store, header({ target = 42 }))
        local rejected = store:rejected()
        assert.equals(4, #rejected)
        assert.same({
            { slot = 2, target = "alpha", reason = "feed version 3 is not supported (expected 4)" },
            { slot = 3, target = nil, reason = "header is missing" },
            { slot = 4, target = "ahead", reason = "generatedAt is 900 s in the future (diverged generator)" },
            -- A numeric target is named in the report but is no market id.
            { slot = 5, target = nil, reason = "header field target is missing or malformed" },
        }, rejected)
    end)

    it("hands out fresh copies", function()
        local _, store = fresh()
        commit(store, header({ version = 3 }))
        local rejected = store:rejected()
        rejected[1].reason = "edited"
        rawset(rejected, #rejected + 1, {})
        assert.equals("feed version 3 is not supported (expected 4)", store:rejected()[1].reason)
        assert.equals(1, #store:rejected())
    end)

    it("is empty until something is refused", function()
        local _, store = fresh()
        assert.same({}, store:rejected())
        commit(store, header())
        assert.same({}, store:rejected())
    end)
end)
