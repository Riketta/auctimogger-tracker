local ADDON, A = ...

-- Adapters/public_api.lua - the namespace table, published as the global the
-- game and other addons read (SPEC 7.9).
--
-- Everything here is a defensive view over the Store: one flat, fresh
-- `AmdbMarketInfo` per call, the span evidence other addons value with
-- (`GetSpans`, nil while the market is stale, SPEC 5.9), the market list, the
-- update subscription and Tekkub's `GetAuctionBuyout`. Callers keep and may
-- change what they are handed: nothing is shared with the Store or between two
-- calls. An unknown item answers nil, never an error.
--
-- Time comes from `A.clock()` (Adapters/market_files.lua): nothing here reads
-- the game clock, so a spec moves it there.

-- Wiring invariant of the TOC order, failed loudly rather than at the first call.
assert(type(A.clock) == "function", ADDON .. ": A.clock must be set before Adapters/public_api.lua loads")

rawset(_G, ADDON, A)

---@type AmdbStore
local store = A.store
---@type AmdbResolveModule
local Resolve = A.Core.Resolve
---@type AmdbMarketModule
local Market = A.Core.Market

--- Whatever addon defined `GetAuctionBuyout` before this one loaded. Tekkub's
--- convention: an addon that cannot answer steps aside for the one before it.
local previousBuyout = rawget(_G, "GetAuctionBuyout")

---@class AmdbMarketOptions
---@field category AmdbCategory|nil "item" or "commodity": the pool to read when an item id is in both; nil: items first
---@field market string|nil a target id; nil: the primary market
---@field species integer|nil a battle-pet species id, named directly instead of through `item`

--- The public, flat view of one record (SPEC 7.9). Fresh on every call;
--- fields the record does not carry are nil, never zero or false.
---@class AmdbMarketInfo
---@field itemID integer the species id for a pet
---@field category AmdbCategory "item", "commodity" or "pet"
---@field market string the target id the record was read from
---@field generatedAt integer epoch seconds: the market's freshness stamp
---@field ageSeconds number now minus generatedAt
---@field snapshotAgeSeconds number|nil now minus the record's own snapshot (auctions; commodities for a commodity)
---@field stale boolean the market is older than 24 hours
---@field floorNow integer|nil copper: the cheapest unit price listed now
---@field floorQty integer|nil units listed at exactly the floor
---@field listedNow integer|nil listings in the snapshot
---@field p05Now integer|nil copper: the 5th percentile of the unit prices in the newest snapshot, nearest rank
---@field seenFloor integer|nil copper: today's cheapest listing that is gone
---@field daysSinceListed integer|nil whole days since the item was last seen listed; 0 = today
---@field value number|nil copper: the record's reference value
---@field valueSource string|nil "sold", "listed" or the span name behind `value`
---@field trend number|nil fraction, signed
---@field soldMedian14 integer|nil copper
---@field saleRate number|nil fraction: the 14-day sale rate
---@field saleRate180d number|nil fraction
---@field soldCount integer|nil
---@field expiredCount integer|nil
---@field soldPerDay number|nil units a day
---@field supplyDays integer|nil 65535: at least that many days of stock
---@field daysSinceLastSale integer|nil
---@field closuresSinceLastSale integer|nil
---@field low30 integer|nil copper
---@field lowRank30d number|nil fraction
---@field vendorPrice integer|nil copper
---@field craft AmdbCraft|nil a fresh copy
---@field deal AmdbDeal|nil a fresh copy
---@field spans table<string, AmdbSpanBlock> a fresh copy: d1/d7/d14/d30 -> the span block

--- One market of `GetMarkets()`. Fresh on every call.
---@class AmdbMarketSummary
---@field target string
---@field slot integer position in load order, 1-based
---@field records integer kept records over the three record tables
---@field generatedAt integer epoch seconds
---@field ageSeconds number now minus generatedAt
---@field stale boolean

--- A record that was found: what names it, the pool it came from, its market.
---@class AmdbLocated
---@field id integer the item id, or the species id of a pet
---@field category AmdbCategory
---@field market AmdbMarket
---@field record AmdbRecord

--- The version of the addon, and the only carrier version it reads (SPEC 5.2).
A.VERSION = "0.1.1"
A.FEED_VERSION = Market.FEED_VERSION

--- `opts` when it is a table, else nil: junk options are no options.
---@param opts AmdbMarketOptions|nil
---@return AmdbMarketOptions|nil options
local function optionsOf(opts)
    if type(opts) == "table" then
        return opts
    end
    return nil
end

--- The market `options` select: the one `options.market` names, else the
--- primary. An option that is not a target id, or that names no loaded
--- market, selects nothing.
---@param options AmdbMarketOptions|nil
---@return AmdbMarket|nil market
local function selectMarket(options)
    local target = options ~= nil and options.market or nil
    if target == nil then
        return store:primary()
    end
    if type(target) ~= "string" then
        return nil
    end
    return store:get(target)
end

--- The record `item` names in the selected market (SPEC 7.9): an item id, an
--- item link or an `item:` string; a `battlepet:` link or string is a pet.
--- `options.species` names a species directly and wins over `item`.
---@param item any
---@param options AmdbMarketOptions|nil
---@return AmdbLocated|nil located nil when the item is unknown in that market
local function find(item, options)
    local market = selectMarket(options)
    if market == nil then
        return nil
    end

    local species = options ~= nil and options.species or nil
    if species ~= nil then
        local speciesID = Resolve.species(species)
        if speciesID == nil then
            return nil
        end
        local record = market:petRecord(speciesID)
        if record == nil then
            return nil
        end
        return { id = speciesID, category = "pet", market = market, record = record }
    end

    local kind, id = Resolve.resolve(item)
    if kind == nil or id == nil then
        return nil
    end
    if kind == "pet" then
        local record = market:petRecord(id)
        if record == nil then
            return nil
        end
        return { id = id, category = "pet", market = market, record = record }
    end

    local category = options ~= nil and options.category or nil
    if category ~= nil then
        local record = market:record(id, category)
        if record == nil then
            return nil
        end
        return { id = id, category = category, market = market, record = record }
    end
    -- No pool named: items first, then commodities, which is the Store's own
    -- default. The first lookup settles the category the caller is told.
    local record = market:record(id, "item")
    if record ~= nil then
        return { id = id, category = "item", market = market, record = record }
    end
    record = market:record(id, "commodity")
    if record ~= nil then
        return { id = id, category = "commodity", market = market, record = record }
    end
    return nil
end

--- A fresh copy of a span table: the v3 shape, same span names, and of each
--- block only the fields it carries. Copied on purpose: this module promises
--- fresh tables even if the Store ever hands out shared ones.
---@param spans table<string, AmdbSpanBlock>
---@return table<string, AmdbSpanBlock> copy
local function copySpans(spans)
    local copy = {}
    for name, block in pairs(spans) do
        local fields = {}
        for field, value in pairs(block) do
            fields[field] = value
        end
        copy[name] = fields
    end
    return copy
end

--- The public view of one found record.
---@param located AmdbLocated
---@param now number epoch seconds
---@return AmdbMarketInfo info
local function toInfo(located, now)
    local record = located.record
    local category = located.category
    local market = located.market
    local live = record.live
    local craft = record.craft
    local deal = record.deal
    ---@type AmdbMarketInfo
    local info = {
        itemID = located.id,
        category = category,
        market = market.target,
        generatedAt = market.generatedAt,
        ageSeconds = market:ageSeconds(now),
        -- A pet sells in the auctions like an item; only a commodity has its own snapshot.
        snapshotAgeSeconds = market:snapshotAge(category == "commodity" and "commodities" or "auctions", now),
        stale = market:isStale(now),
        floorNow = live ~= nil and live.floor or nil,
        floorQty = live ~= nil and live.floorQty or nil,
        listedNow = live ~= nil and live.listed or nil,
        p05Now = live ~= nil and live.p05 or nil,
        seenFloor = live ~= nil and live.seenFloor or nil,
        daysSinceListed = live ~= nil and live.daysSinceListed or nil,
        value = record:reference(),
        valueSource = record:valueSource(),
        -- Fractions, not basis points: every field without "Bp" in its name is plain.
        trend = record.trendBp ~= nil and record.trendBp / 10000 or nil,
        soldMedian14 = record.soldMedian14,
        saleRate = record:saleRate14(),
        saleRate180d = record:saleRate180d(),
        soldCount = record.soldCount,
        expiredCount = record.expiredCount,
        soldPerDay = record:soldPerDay(),
        supplyDays = record.supplyDays,
        daysSinceLastSale = record:daysSinceLastSale(),
        closuresSinceLastSale = record:closuresSinceLastSale(),
        low30 = record.low30,
        lowRank30d = record.lowRank30d ~= nil and record.lowRank30d / 10000 or nil,
        vendorPrice = record.vendorPrice,
        craft = craft ~= nil and { cost = craft.cost, profit = craft.profit, expectedProfit = craft.expectedProfit }
            or nil,
        deal = deal ~= nil and { rank = deal.rank, scoreBp = deal.scoreBp, discountBp = deal.discountBp } or nil,
        spans = copySpans(record.spans),
    }
    return info
end

--- One record as a flat, fresh table, or nil when the item is unknown in the
--- market (SPEC 7.9).
---@param item any an item id, an item link or an `item:` string; a `battlepet:` link or string is a pet
---@param opts AmdbMarketOptions|nil `category` and `market` narrow the lookup; `species` names a pet directly
---@return AmdbMarketInfo|nil info
function A.GetMarketInfo(item, opts)
    local located = find(item, optionsOf(opts))
    if located == nil then
        return nil
    end
    return toInfo(located, A.clock())
end

--- The span evidence of one record, in the v3 shape. Nil when the item is
--- unknown and while its market is stale (SPEC 5.9): the sniper must get no
--- stale evidence. The data is still shown through `GetMarketInfo`.
---@param item any an item id, an item link or an `item:` string; a `battlepet:` link or string is a pet
---@param category AmdbCategory|nil nil: items first, then commodities
---@return table<string, AmdbSpanBlock>|nil spans a fresh copy
function A.GetSpans(item, category)
    local located = find(item, { category = category })
    if located == nil or located.market:isStale(A.clock()) then
        return nil
    end
    return copySpans(located.record.spans)
end

--- Every loaded market, in slot order. Fresh tables.
---@return AmdbMarketSummary[] markets empty when no carrier registered
function A.GetMarkets()
    local summaries = {}
    local stats = store:stats(A.clock())
    for i = 1, #stats do
        local entry = stats[i]
        summaries[i] = {
            target = entry.target,
            slot = entry.slot,
            records = entry.records,
            generatedAt = entry.generatedAt,
            ageSeconds = entry.ageSeconds,
            stale = entry.stale,
        }
    end
    return summaries
end

--- The target id of the primary market: the explicit choice when it is
--- loaded, else the first slot.
---@return string|nil target nil when no market is loaded
function A.GetPrimaryMarket()
    return store:primaryTarget()
end

--- Registers a listener that runs once at login and whenever the set of
--- markets changes. The callback takes no arguments.
---@param callback AmdbUpdateCallback
---@return fun() unsubscribe calling it twice is harmless
function A.OnUpdate(callback)
    return store:onUpdate(callback)
end

--- The reference value in copper, Tekkub's convention. An item this addon
--- does not know is handed to the `GetAuctionBuyout` that was defined before
--- this addon loaded, so its caller still gets an answer.
---@param item any an item id, an item link or an `item:` string; a `battlepet:` link or string is a pet
---@return number|nil copper nil when unknown and no earlier addon answers
function A.GetAuctionBuyout(item)
    local located = find(item, nil)
    local value = located ~= nil and located.record:reference() or nil
    if value ~= nil then
        return value
    end
    if type(previousBuyout) == "function" then
        return previousBuyout(item)
    end
    return nil
end

rawset(_G, "GetAuctionBuyout", A.GetAuctionBuyout)

return A
