local _, A = ...
A.Core = A.Core or {}

-- Core/market.lua - the market store (pure Lua).
--
-- One Market is a loaded carrier: its validated header plus the packed record
-- strings it shipped. The Store owns every market. It validates and registers
-- a carrier as its slot file runs, numbers markets in load order, binds the
-- primary market and answers lookups by decoding on every call (about 30
-- microseconds per record), so there is no cache and callers keep the tables
-- they are given. `now` is always a parameter: nothing here reads a clock, a
-- game global or the file system.
--
-- Everything a carrier hands over is untrusted. A bad header rejects the
-- market and remembers why; a bad record is dropped and counted while the
-- market stays. Nothing in this file throws on carrier data.

---@class AmdbMarketModule
---@field FEED_VERSION integer the only carrier version this addon reads
---@field STALE_AFTER_SECONDS number age past which a market is flagged stale
---@field FUTURE_TOLERANCE_SECONDS number clock skew a generator is allowed before it counts as diverged
---@field CATEGORIES AmdbCategory[] API spelling of the carrier's record tables
---@field Market AmdbMarket the class of one loaded market
---@field Store AmdbStore the class of the store
local Market = {}
A.Core.Market = Market

Market.FEED_VERSION = 4
Market.STALE_AFTER_SECONDS = 24 * 3600
Market.FUTURE_TOLERANCE_SECONDS = 5 * 60
Market.CATEGORIES = { "item", "commodity", "pet" }

---@type AmdbRecordCodec
local Record = A.Core.Record

---@alias AmdbCategory "item"|"commodity"|"pet"
---@alias AmdbUpdateCallback fun()

--- The header exactly as a carrier wrote it. Untrusted: every field is checked
--- before use, so its values are typed `any` on purpose.
---@alias AmdbRawHeader table<string, any>

--- A carrier header after validation: the known keys only, each of its
--- declared type or absent. An optional field of the wrong type is dropped,
--- never fatal.
---@class AmdbMarketMeta
---@field version integer always FEED_VERSION
---@field trackerVersion string
---@field target string the market's key
---@field generatedAt integer epoch seconds; the market's freshness stamp
---@field flavor string|nil
---@field region string|nil
---@field rulesetId integer|nil
---@field houseId integer|nil
---@field connectedRealmId integer|nil
---@field auctionsAt integer|nil fetch time of the newest auctions snapshot
---@field commoditiesAt integer|nil fetch time of the newest commodities snapshot

--- The tables a carrier fills between `A.BeginMarket(meta)` and `A.EndMarket()`.
---@class AmdbMarketBuilder
---@field meta AmdbRawHeader|nil
---@field items table<integer, string> itemID -> packed record
---@field commodities table<integer, string> itemID -> packed record
---@field pets table<integer, string> speciesID -> packed record

--- The packed strings a market kept, per record table. Internal: read records
--- through the lookup methods, which decode a fresh table every time.
---@class AmdbMarketRaw
---@field items table<integer, string>
---@field commodities table<integer, string>
---@field pets table<integer, string>

---@class AmdbMarket
---@field slot integer position in load order among every commit, 1-based
---@field target string
---@field meta AmdbMarketMeta validated header copy
---@field generatedAt integer
---@field dropped integer records dropped at commit (malformed entry or undecodable)
---@field raw AmdbMarketRaw kept packed strings (internal)
local LoadedMarket = {}
LoadedMarket.__index = LoadedMarket

---@class AmdbUpdateListener
---@field callback AmdbUpdateCallback|nil nil once unsubscribed

--- A commit the store refused, kept for `/amdb status`.
---@class AmdbMarketRejection
---@field slot integer the slot the market would have taken
---@field target string|nil the raw target when it was a string
---@field reason string why, without the `market ... rejected:` lead

---@class AmdbMarketStats
---@field slot integer
---@field target string
---@field records integer kept records
---@field items integer
---@field commodities integer
---@field pets integer
---@field generatedAt integer
---@field ageSeconds number now minus generatedAt; negative inside the future tolerance
---@field stale boolean
---@field dropped integer

---@class AmdbStore
---@field marketList AmdbMarket[] registered markets in slot order (internal)
---@field byTarget table<string, AmdbMarket> target -> market (internal)
---@field builder AmdbMarketBuilder|nil the open builder a carrier is filling
---@field nextSlot integer the number the next commit takes
---@field explicitTarget string|nil the per-character choice; may name a market that is not loaded
---@field listeners AmdbUpdateListener[] (internal)
---@field rejections AmdbMarketRejection[] refused commits (internal)
local Store = {}
Store.__index = Store

Market.Market = LoadedMarket
Market.Store = Store

-- Optional header fields and the type each must have to survive.
local OPTIONAL_FIELDS = {
    { name = "flavor", isString = true },
    { name = "region", isString = true },
    { name = "rulesetId", isString = false },
    { name = "houseId", isString = false },
    { name = "connectedRealmId", isString = false },
    { name = "auctionsAt", isString = false },
    { name = "commoditiesAt", isString = false },
}

--- The tracker's own grammar for a target id (crates/core/src/config.rs): a
--- lowercase ASCII letter or digit, then lowercase alphanumerics, `.`, `_`
--- or `-`. A target is printed in chat and in tooltips, so the header is held
--- to these characters instead of being sanitized at every use.
local TARGET_ID = "^[%l%d][%l%d%._%-]*$"

--- A finite number without a fraction; NaN and the infinities are not.
---@param value any
---@return boolean
local function isInteger(value)
    if type(value) ~= "number" then
        return false
    end
    if value ~= value or value == math.huge or value == -math.huge then
        return false
    end
    return math.floor(value) == value
end

---@param value any
---@return boolean
local function isPositiveInteger(value)
    return type(value) == "number" and value > 0 and value < math.huge and math.floor(value) == value
end

---@param value any
---@return boolean
local function isNonEmptyString(value)
    return type(value) == "string" and #value > 0
end

--- A value for a rejection report: numbers as written, strings quoted and cut.
---@param value any
---@return string
local function describe(value)
    local kind = type(value)
    if kind == "number" then
        return tostring(value)
    end
    if kind == "string" then
        return "\"" .. value:sub(1, 32) .. "\""
    end
    return kind
end

---@param seconds number
---@return string
local function secondsText(seconds)
    return tostring(math.floor(seconds + 0.5))
end

--- `market "x"` for a report, or the bare `market` when the target is unusable.
--- A report is one line of plain chat: control characters and color escapes
--- are stripped and a long id is cut.
---@param target string|number|nil
---@return string
local function subject(target)
    local kind = type(target)
    if kind ~= "string" and kind ~= "number" then
        return "market"
    end
    local clean = tostring(target):gsub("[%c|]", "")
    if #clean > 64 then
        clean = clean:sub(1, 64) .. "..."
    end
    return "market \"" .. clean .. "\""
end

--- The target as the carrier wrote it, when it is a string or a number, so a
--- rejected market can still be named in its report.
---@param meta AmdbRawHeader|nil
---@return string|number|nil target
local function rawTarget(meta)
    if meta == nil or type(meta) ~= "table" then
        return nil
    end
    local value = rawget(meta, "target")
    if type(value) == "string" or type(value) == "number" then
        return value
    end
    return nil
end

--- Checks a carrier header against SPEC 5.2 and 5.9.
---@param meta AmdbRawHeader|nil
---@param now number epoch seconds the generator's clock is judged against
---@return AmdbMarketMeta|nil validated nil when the market must be rejected
---@return string reason the rejection without its lead; "" when valid
local function validateHeader(meta, now)
    if meta == nil or type(meta) ~= "table" then
        return nil, "header is missing"
    end

    local expected = tostring(Market.FEED_VERSION)
    local version = rawget(meta, "version")
    if version == nil then
        return nil, "feed version is missing (expected " .. expected .. ")"
    end
    if type(version) ~= "number" or version ~= Market.FEED_VERSION then
        return nil, "feed version " .. describe(version) .. " is not supported (expected " .. expected .. ")"
    end

    local malformed = {}
    if not isNonEmptyString(rawget(meta, "trackerVersion")) then
        malformed[#malformed + 1] = "trackerVersion"
    end
    if not isNonEmptyString(rawget(meta, "target")) then
        malformed[#malformed + 1] = "target"
    end
    if not isInteger(rawget(meta, "generatedAt")) then
        malformed[#malformed + 1] = "generatedAt"
    end
    if #malformed > 0 then
        local noun = #malformed > 1 and "header fields " or "header field "
        local verb = #malformed > 1 and " are missing or malformed" or " is missing or malformed"
        return nil, noun .. table.concat(malformed, ", ") .. verb
    end
    -- Here `target` is a non-empty string; anything but the tracker's id
    -- characters in it means the header did not come from a tracker.
    if rawget(meta, "target"):match(TARGET_ID) == nil then
        return nil, "header field target is not a tracker target id ([a-z0-9][a-z0-9._-]*)"
    end

    local generatedAt = rawget(meta, "generatedAt")
    local skew = generatedAt - now
    if skew > Market.FUTURE_TOLERANCE_SECONDS then
        return nil, "generatedAt is " .. secondsText(skew) .. " s in the future (diverged generator)"
    end

    ---@type AmdbMarketMeta
    local validated = {
        version = Market.FEED_VERSION,
        trackerVersion = rawget(meta, "trackerVersion"),
        target = rawget(meta, "target"),
        generatedAt = generatedAt,
    }
    for i = 1, #OPTIONAL_FIELDS do
        local field = OPTIONAL_FIELDS[i]
        local value = rawget(meta, field.name)
        -- Written as a plain branch: `isString and type(value) == "string" or ...`
        -- would fall through to the integer test for a wrong-typed string field.
        local keeps
        if field.isString then
            keeps = type(value) == "string"
        else
            keeps = isInteger(value)
        end
        if keeps then
            validated[field.name] = value
        end
    end
    return validated, ""
end

--- Copies the well-formed, decodable entries of one carrier table.
---@param source any the table the carrier filled, or whatever it replaced it with
---@param target table<integer, string> the market's kept strings
---@return integer dropped
local function collect(source, target)
    local dropped = 0
    if type(source) ~= "table" then
        return dropped
    end
    for id, text in pairs(source) do
        if not isPositiveInteger(id) or type(text) ~= "string" then
            dropped = dropped + 1
        else
            -- The decoder is trusted code but it is also new code: a record that
            -- makes it throw is dropped like an undecodable one, never fatal.
            local ok, record = pcall(Record.decode, text)
            if ok and type(record) == "table" then
                target[id] = text
            else
                dropped = dropped + 1
            end
        end
    end
    return dropped
end

---@param values table<integer, string>
---@return integer
local function size(values)
    local count = 0
    for _ in pairs(values) do
        count = count + 1
    end
    return count
end

--- True when an explicit choice is in effect and names a loaded market.
---@param store AmdbStore
---@return boolean
local function isChosenLoaded(store)
    local chosen = store.explicitTarget
    return chosen ~= nil and store.byTarget[chosen] ~= nil
end

--- The market a lookup reads: the one `target` names, else the primary.
---@param store AmdbStore
---@param target string|nil
---@return AmdbMarket|nil
local function marketFor(store, target)
    if target ~= nil then
        return store.byTarget[target]
    end
    return store:primary()
end

---@return AmdbStore
function Store.new()
    local store = setmetatable({}, Store)
    store.marketList = {}
    store.byTarget = {}
    store.builder = nil
    store.nextSlot = 1
    store.explicitTarget = nil
    store.listeners = {}
    store.rejections = {}
    return store
end

--- Opens the builder the carrier that is running fills. An unclosed builder
--- from a malformed file is discarded here; the caller reports it.
---@param meta AmdbRawHeader|nil the header as the carrier wrote it
---@return AmdbMarketBuilder builder
---@return boolean replaced true when an unclosed builder was discarded
function Store:begin(meta)
    local replaced = self.builder ~= nil
    local builder = { meta = meta, items = {}, commodities = {}, pets = {} }
    self.builder = builder
    return builder, replaced
end

--- Validates, registers and closes the open builder. A rejected market
--- consumes its slot number too, so the numbers match the slot files the user
--- copied feeds into. Callbacks fire when the set of markets changed: for a new
--- or a replacing registration, never for a rejection.
---@param now number epoch seconds
---@return AmdbMarket|nil market nil when the market was rejected, or nothing was open
---@return string[] reports human-readable sentences, without an addon prefix
function Store:commit(now)
    local builder = self.builder
    if builder == nil then
        return nil, { "no open market to commit" }
    end
    self.builder = nil

    local slot = self.nextSlot
    self.nextSlot = slot + 1

    local meta, reason = validateHeader(builder.meta, now)
    local target = rawTarget(builder.meta)
    if meta == nil then
        -- The record keeps a string only; the report names a numeric target too.
        local named = type(target) == "string" and target or nil
        self.rejections[#self.rejections + 1] = { slot = slot, target = named, reason = reason }
        return nil, { subject(target) .. " rejected: " .. reason }
    end

    local raw = { items = {}, commodities = {}, pets = {} }
    local dropped = collect(builder.items, raw.items)
        + collect(builder.commodities, raw.commodities)
        + collect(builder.pets, raw.pets)

    local market = setmetatable({
        slot = slot,
        target = meta.target,
        meta = meta,
        generatedAt = meta.generatedAt,
        dropped = dropped,
        raw = raw,
    }, LoadedMarket)

    local reports = {}
    local previous = self.byTarget[market.target]
    if previous ~= nil then
        for i = #self.marketList, 1, -1 do
            if self.marketList[i] == previous then
                table.remove(self.marketList, i)
            end
        end
        reports[#reports + 1] = subject(market.target) .. " from slot " .. slot .. " replaces slot " .. previous.slot
    end
    self.marketList[#self.marketList + 1] = market
    self.byTarget[market.target] = market
    if dropped > 0 then
        reports[#reports + 1] = subject(market.target) .. ": " .. dropped .. " record(s) dropped (undecodable)"
    end
    self:fireUpdate()
    return market, reports
end

--- Every registered market in slot order. A fresh array: callers may keep it.
---@return AmdbMarket[]
function Store:markets()
    local list = {}
    for i, market in ipairs(self.marketList) do
        list[i] = market
    end
    return list
end

---@param target string|nil
---@return AmdbMarket|nil
function Store:get(target)
    return self.byTarget[target]
end

--- The market the tooltip reads: the explicit choice when that target is
--- loaded; else the only loaded market; else the first in slot order (with no
--- market loaded, nil).
---@return AmdbMarket|nil
function Store:primary()
    local chosen = self.explicitTarget
    if chosen ~= nil then
        local market = self.byTarget[chosen]
        if market ~= nil then
            return market
        end
    end
    return self.marketList[1]
end

---@return string|nil
function Store:primaryTarget()
    local market = self:primary()
    if market == nil then
        return nil
    end
    return market.target
end

--- The saved per-character choice, even when it names no loaded market.
---@return string|nil
function Store:explicitPrimary()
    return self.explicitTarget
end

--- True when two or more markets are loaded and no choice is in effect: the
--- primary is then merely the first slot, and the login line asks the user to pick.
---@return boolean
function Store:primaryIsAmbiguous()
    return #self.marketList >= 2 and not isChosenLoaded(self)
end

--- Raw setter for a saved choice: never fails, even when the target is not
--- loaded (resolution then falls back). Loads nothing, fires nothing.
---@param target string|nil
function Store:setExplicitPrimary(target)
    if isNonEmptyString(target) then
        self.explicitTarget = target
    else
        self.explicitTarget = nil
    end
end

--- The user action behind `/amdb market <id>`. Fires the update callbacks when
--- the effective primary market changed: whatever cached primary-market data
--- must drop it.
---@param target string|nil
---@return boolean ok
---@return string|nil message why not, when ok is false
function Store:setPrimary(target)
    if type(target) ~= "string" then
        return false, "market id is required"
    end
    if self.byTarget[target] == nil then
        return false, subject(target) .. " is not loaded"
    end
    local before = self:primaryTarget()
    self.explicitTarget = target
    if self:primaryTarget() ~= before then
        self:fireUpdate()
    end
    return true
end

--- Decodes a record of one market, or of the primary market: a fresh table on
--- every call. `category` nil looks in items first, then commodities.
---@param itemID integer|nil
---@param category AmdbCategory|nil "item" or "commodity"
---@param target string|nil nil: the primary market
---@return AmdbRecord|nil
function Store:record(itemID, category, target)
    local market = marketFor(self, target)
    if market == nil then
        return nil
    end
    return market:record(itemID, category)
end

---@param speciesID integer|nil
---@param target string|nil nil: the primary market
---@return AmdbRecord|nil
function Store:petRecord(speciesID, target)
    local market = marketFor(self, target)
    if market == nil then
        return nil
    end
    return market:petRecord(speciesID)
end

--- Registers a listener for market changes. The callback takes no arguments.
--- The returned function unsubscribes it; calling that twice is harmless.
---@param callback AmdbUpdateCallback
---@return fun() unsubscribe
function Store:onUpdate(callback)
    assert(type(callback) == "function", "onUpdate: callback must be a function")
    local listener = { callback = callback }
    self.listeners[#self.listeners + 1] = listener
    return function()
        if listener.callback == nil then
            return
        end
        listener.callback = nil
        for i = 1, #self.listeners do
            if self.listeners[i] == listener then
                table.remove(self.listeners, i)
                break
            end
        end
    end
end

--- Runs every update callback. One that raises does not stop the others.
---@return string[] errors one message per callback that raised
function Store:fireUpdate()
    local errors = {}
    -- The list is copied because a callback may unsubscribe or subscribe.
    local callbacks = {}
    for i, listener in ipairs(self.listeners) do
        callbacks[i] = listener.callback
    end
    for i = 1, #callbacks do
        local callback = callbacks[i]
        if callback ~= nil then
            local ok, err = pcall(callback)
            if not ok then
                errors[#errors + 1] = tostring(err)
            end
        end
    end
    return errors
end

--- Refused commits since the store was created, in commit order. Fresh copies.
---@return AmdbMarketRejection[]
function Store:rejected()
    local list = {}
    for i, rejection in ipairs(self.rejections) do
        list[i] = { slot = rejection.slot, target = rejection.target, reason = rejection.reason }
    end
    return list
end

--- Per market, in slot order.
---@param now number epoch seconds
---@return AmdbMarketStats[]
function Store:stats(now)
    local list = {}
    for i, market in ipairs(self.marketList) do
        list[i] = market:stats(now)
    end
    return list
end

---@param now number epoch seconds
---@return number now minus generatedAt; negative inside the future tolerance
function LoadedMarket:ageSeconds(now)
    return now - self.generatedAt
end

--- Age of the record's own snapshot: `auctionsAt` for "auctions",
--- `commoditiesAt` for "commodities"; nil when that stamp is absent or the
--- kind is unknown.
---@param kind string "auctions" or "commodities"
---@param now number epoch seconds
---@return number|nil
function LoadedMarket:snapshotAge(kind, now)
    local stamp
    if kind == "auctions" then
        stamp = self.meta.auctionsAt
    elseif kind == "commodities" then
        stamp = self.meta.commoditiesAt
    else
        return nil
    end
    if stamp == nil then
        return nil
    end
    return now - stamp
end

--- Stale data is shown rather than hidden (SPEC 5.9), so this is computed
--- against the caller's clock on every call and never stored.
---@param now number epoch seconds
---@return boolean
function LoadedMarket:isStale(now)
    return self:ageSeconds(now) > Market.STALE_AFTER_SECONDS
end

--- A fresh decoded record, or nil. `category` nil looks in items first, then
--- commodities; "pet" and unknown categories answer nil (pets have their own
--- method, since a species id is not an item id).
---@param itemID integer|nil
---@param category AmdbCategory|nil
---@return AmdbRecord|nil
function LoadedMarket:record(itemID, category)
    local text
    if category == "item" then
        text = self.raw.items[itemID]
    elseif category == "commodity" then
        text = self.raw.commodities[itemID]
    elseif category == nil then
        text = self.raw.items[itemID]
        if text == nil then
            text = self.raw.commodities[itemID]
        end
    else
        return nil
    end
    if text == nil then
        return nil
    end
    local record = Record.decode(text)
    return record
end

---@param speciesID integer|nil
---@return AmdbRecord|nil
function LoadedMarket:petRecord(speciesID)
    local text = self.raw.pets[speciesID]
    if text == nil then
        return nil
    end
    local record = Record.decode(text)
    return record
end

--- Kept records over the three tables.
---@return integer
function LoadedMarket:count()
    return size(self.raw.items) + size(self.raw.commodities) + size(self.raw.pets)
end

---@param now number epoch seconds
---@return AmdbMarketStats
function LoadedMarket:stats(now)
    local age = self:ageSeconds(now)
    return {
        slot = self.slot,
        target = self.target,
        records = self:count(),
        items = size(self.raw.items),
        commodities = size(self.raw.commodities),
        pets = size(self.raw.pets),
        generatedAt = self.generatedAt,
        ageSeconds = age,
        stale = age > Market.STALE_AFTER_SECONDS,
        dropped = self.dropped,
    }
end

return Market
