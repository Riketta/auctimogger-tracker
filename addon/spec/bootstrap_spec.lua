-- Specs for bootstrap.lua, the composition root: the frame it creates and the
-- lines PLAYER_LOGIN prints (SPEC 7.11).
--
-- CreateFrame is stubbed to hand out a frame that records RegisterEvent and
-- SetScript. The stub goes into _G with rawset: a spec file's own globals are
-- private to it, and the addon is loaded with loadfile. The real namespace is
-- loaded in TOC order, slot files excepted, and PLAYER_LOGIN is delivered by
-- calling the recorded OnEvent script.

local Loader = require("spec.helpers.loader")

local NOW = 1795560000
local HOUR = 3600
local DAY = 24 * HOUR
local GOLDEN = Loader.FIXTURES .. "/database_v4.lua"
local TARGET = "forever-eu-pvp-alliance"
local OTHER = "forever-eu-pvp-horde"
local THIRD = "forever-eu-pvp-blackwater"
local GENERATED_AT = 1795559034

-- The house colors of SPEC 7.5.4, written out so that a line pins its colors
-- by their bytes and not through the formatter that renders them.
local TITLE = "|cffff7d0aAuctiMogger Database|r"
local MUTED = "|cff99a1b3"
local WARN = "|cffffd100"
local HOT = "|cffff8040"
local BAD = "|cffff4040"
local RESET = "|r"

local NO_DATA = TITLE .. ": " .. BAD .. "no market data — copy the tracker's feed.lua over Market1.lua" .. RESET
local PICK_ONE = "several markets loaded, pick one: /amdb market <id>"

--- Everything the TOC loads, slot files excepted.
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
    "bootstrap.lua",
}

local MARKET = [[
local _, A = ...
local m = A.BeginMarket({ version = %d, trackerVersion = "0.1.0", target = %s, generatedAt = %s })
A.EndMarket()
]]

--- A frame that records what the addon does to it.
---@param ... any what CreateFrame was given
---@return table frame
local function newFrame(...)
    local frame = { args = { ... }, events = {}, scripts = {} }
    function frame:RegisterEvent(event)
        self.events[#self.events + 1] = event
    end
    function frame:SetScript(scriptType, handler)
        self.scripts[scriptType] = handler
    end
    return frame
end

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

--- Empties `list` in place: a closure that appends to it holds the table,
--- not a name, so reassigning a caller's variable would not reset it.
---@param list any[]
local function clear(list)
    for i = #list, 1, -1 do
        list[i] = nil
    end
end

--- The status line of SPEC 7.11 as bootstrap builds it: what a test expects.
---@param count string "1 market" or "2 markets"
---@param target string the primary target
---@param color string the color code of the age's tier
---@param age string the age text
---@return string line
local function status(count, target, color, age)
    return TITLE .. ": " .. count .. " · primary " .. target .. " · data " .. color .. age .. RESET .. " old"
end

--- The suffix for a stale market other than the primary.
---@param target string
---@param age string the age text
---@return string suffix
local function resync(target, age)
    return " · " .. HOT .. target .. " is " .. age .. " old — resync" .. RESET
end

describe("bootstrap", function()
    local frame
    local created
    local savedCreateFrame, savedNamespace, savedBuyout

    before_each(function()
        savedCreateFrame = rawget(_G, "CreateFrame")
        savedNamespace = rawget(_G, "AuctiMoggerDatabase")
        savedBuyout = rawget(_G, "GetAuctionBuyout")
        created = 0
        rawset(_G, "CreateFrame", function(...)
            created = created + 1
            frame = newFrame(...)
            return frame
        end)
    end)

    after_each(function()
        rawset(_G, "CreateFrame", savedCreateFrame)
        rawset(_G, "AuctiMoggerDatabase", savedNamespace)
        rawset(_G, "GetAuctionBuyout", savedBuyout)
    end)

    --- The addon in TOC order, the chat recorded and the clock pinned to NOW.
    ---@return table A namespace
    ---@return string[] lines what reached chat
    local function boot()
        local lines = {}
        local A = Loader.load(Loader.namespace(), unpack(FILES))
        A.chat = function(text)
            lines[#lines + 1] = text
        end
        A.clock = function()
            return NOW
        end
        return A, lines
    end

    --- Delivers an event to the frame's OnEvent script.
    ---@param event string
    local function fire(event)
        local handler = frame.scripts.OnEvent
        assert.is_not_nil(handler, "bootstrap installed no OnEvent script")
        handler(frame, event)
    end

    --- Registers another market: a header and no records, which is all the
    --- status line reads.
    ---@param A table namespace
    ---@param target string
    ---@param generatedAt number epoch seconds
    ---@param version integer|nil the feed version to write, 4 by default
    local function secondMarket(A, target, generatedAt, version)
        local source = MARKET:format(version or 4, string.format("%q", target), tostring(generatedAt))
        local chunk = assert(loadstring(source, "=carrier"))
        chunk(Loader.ADDON, A)
    end

    it("returns the namespace", function()
        local namespace = Loader.load(Loader.namespace(), unpack(FILES, 1, #FILES - 1))
        local chunk = assert(loadfile(Loader.ROOT .. "/bootstrap.lua"))
        local module = chunk(Loader.ADDON, namespace)
        assert.equals(namespace, module)
    end)

    it("creates one frame, registers PLAYER_LOGIN and installs OnEvent", function()
        boot()
        assert.equals(1, created)
        assert.same({ "Frame" }, frame.args)
        assert.same({ "PLAYER_LOGIN" }, frame.events)
        assert.same({ "OnEvent" }, keysOf(frame.scripts))
        assert.equals("function", type(frame.scripts.OnEvent))
    end)

    it("prints nothing before the login", function()
        local A, lines = boot()
        Loader.carrier(A, GOLDEN)
        A.Say("recorded while loading", "bad")
        assert.same({}, lines)
    end)

    it("prints the no-data line, in bad, when no carrier registered", function()
        local _, lines = boot()
        fire("PLAYER_LOGIN")
        assert.same({ NO_DATA }, lines)
    end)

    it("prints the status line of one market, its age in the color of its tier", function()
        local A, lines = boot()
        Loader.carrier(A, GOLDEN)
        fire("PLAYER_LOGIN")
        assert.same({ status("1 market", TARGET, MUTED, "16m") }, lines)
    end)

    it("colors the age by its tier: warn from 2 h, hot from 12 h, bad from 24 h", function()
        local A, lines = boot()
        Loader.carrier(A, GOLDEN)
        local function lineAt(age)
            A.clock = function()
                return GENERATED_AT + age
            end
            clear(lines)
            fire("PLAYER_LOGIN")
            assert.equals(1, #lines)
            return lines[1]
        end
        assert.equals(status("1 market", TARGET, MUTED, "0m"), lineAt(0))
        assert.equals(status("1 market", TARGET, MUTED, "1h 59m"), lineAt(2 * HOUR - 60))
        assert.equals(status("1 market", TARGET, WARN, "2h 00m"), lineAt(2 * HOUR))
        assert.equals(status("1 market", TARGET, HOT, "12h 00m"), lineAt(12 * HOUR))
        assert.equals(status("1 market", TARGET, HOT, "23h 59m"), lineAt(DAY - 60))
        assert.equals(status("1 market", TARGET, BAD, "1d"), lineAt(DAY))
        assert.equals(status("1 market", TARGET, BAD, "2d"), lineAt(2 * DAY + HOUR))
    end)

    it("counts the markets and asks for a choice when there are several and none is made", function()
        local A, lines = boot()
        Loader.carrier(A, GOLDEN)
        secondMarket(A, OTHER, NOW - 600)
        fire("PLAYER_LOGIN")
        assert.same({ status("2 markets", TARGET, MUTED, "16m") .. " · " .. PICK_ONE }, lines)
        -- A choice that names no loaded market is no choice.
        A.store:setExplicitPrimary("not-loaded")
        clear(lines)
        fire("PLAYER_LOGIN")
        assert.is_true(A.store:primaryIsAmbiguous())
        -- One chat line, the hint included.
        assert.equals(1, #lines)
        assert.equals(status("2 markets", TARGET, MUTED, "16m") .. " · " .. PICK_ONE, lines[1])
    end)

    it("follows the explicit choice and then has nothing to ask", function()
        local A, lines = boot()
        Loader.carrier(A, GOLDEN)
        secondMarket(A, OTHER, NOW - 600)
        assert.is_true(A.store:setPrimary(OTHER))
        fire("PLAYER_LOGIN")
        assert.same({ status("2 markets", OTHER, MUTED, "10m") }, lines)
    end)

    it("names each stale market other than the primary, in hot", function()
        local A, lines = boot()
        Loader.carrier(A, GOLDEN)
        secondMarket(A, OTHER, NOW - 2 * DAY - HOUR)
        secondMarket(A, THIRD, NOW - 3 * DAY)
        fire("PLAYER_LOGIN")
        assert.same({
            status("3 markets", TARGET, MUTED, "16m") .. " · " .. PICK_ONE .. resync(OTHER, "2d") .. resync(
                THIRD,
                "3d"
            ),
        }, lines)
    end)

    it("does not repeat a stale primary as a suffix: its own age is in the line", function()
        local A, lines = boot()
        local late = GENERATED_AT + 2 * DAY + HOUR
        A.clock = function()
            return late
        end
        Loader.carrier(A, GOLDEN)
        secondMarket(A, OTHER, late - 600)
        fire("PLAYER_LOGIN")
        -- The other market is fresh, so nothing follows; the primary's age is in bad.
        assert.same({ status("2 markets", TARGET, BAD, "2d") .. " · " .. PICK_ONE }, lines)
    end)

    it("prints the recorded reports once, in the color of their level, and drains them", function()
        local A, lines = boot()
        Loader.carrier(A, GOLDEN)
        A.Say("2 record(s) dropped", "warn")
        A.Say("market \"x\" rejected: feed version 3 is not supported", "bad")
        A.Say("slot 2 replaces slot 1")
        assert.equals(3, #A.reports)
        fire("PLAYER_LOGIN")
        assert.same({
            status("1 market", TARGET, MUTED, "16m"),
            TITLE .. " " .. WARN .. "2 record(s) dropped" .. RESET,
            TITLE .. " " .. BAD .. "market \"x\" rejected: feed version 3 is not supported" .. RESET,
            TITLE .. " " .. MUTED .. "slot 2 replaces slot 1" .. RESET,
        }, lines)
        assert.same({}, A.TakeReports())
        -- A second login does not print them again.
        clear(lines)
        fire("PLAYER_LOGIN")
        assert.same({ status("1 market", TARGET, MUTED, "16m") }, lines)
    end)

    it("prints what a bad slot file recorded while it loaded", function()
        local A, lines = boot()
        Loader.carrier(A, GOLDEN)
        secondMarket(A, OTHER, NOW - 600, 3)
        assert.equals(1, #A.store:markets())
        local text = A.reports[1].text
        assert.is_true(text:find("rejected", 1, true) ~= nil)
        assert.is_true(text:find(OTHER, 1, true) ~= nil)
        fire("PLAYER_LOGIN")
        assert.same({
            status("1 market", TARGET, MUTED, "16m"),
            TITLE .. " " .. BAD .. text .. RESET,
        }, lines)
        -- Printed once: the report is gone.
        assert.same({}, A.TakeReports())
    end)

    it("fires the update callbacks at login", function()
        local A, lines = boot()
        Loader.carrier(A, GOLDEN)
        local fired = 0
        local unsubscribe = A.OnUpdate(function()
            fired = fired + 1
        end)
        -- The carrier registered before the subscription: nothing fired yet.
        assert.equals(0, fired)
        fire("PLAYER_LOGIN")
        assert.equals(1, fired)
        fire("PLAYER_LOGIN")
        assert.equals(2, fired)
        unsubscribe()
        fire("PLAYER_LOGIN")
        assert.equals(2, fired)
        -- Each of the three logins printed its status line and nothing else.
        assert.equals(3, #lines)
        for i = 1, #lines do
            assert.equals(status("1 market", TARGET, MUTED, "16m"), lines[i])
        end
    end)

    it("reports a callback that raised, once, in bad, and keeps the others running", function()
        local A, lines = boot()
        Loader.carrier(A, GOLDEN)
        local fired = 0
        A.OnUpdate(function()
            error("listener exploded", 0)
        end)
        A.OnUpdate(function()
            fired = fired + 1
        end)
        fire("PLAYER_LOGIN")
        assert.equals(1, fired)
        assert.same({
            status("1 market", TARGET, MUTED, "16m"),
            TITLE .. " " .. BAD .. "listener exploded" .. RESET,
        }, lines)
        -- It is a report like any other, recorded once and drained by the printing.
        assert.same({}, A.TakeReports())
        -- In verbose mode Say prints it at once: still exactly once.
        A.verbose = true
        clear(lines)
        fire("PLAYER_LOGIN")
        assert.equals(2, fired)
        assert.same({
            status("1 market", TARGET, MUTED, "16m"),
            TITLE .. " " .. BAD .. "listener exploded" .. RESET,
        }, lines)
        A.verbose = false
    end)

    it("ignores an event it did not register for", function()
        local A, lines = boot()
        Loader.carrier(A, GOLDEN)
        fire("PLAYER_LOGOUT")
        fire("ADDON_LOADED")
        assert.same({}, lines)
        assert.same({}, A.TakeReports())
    end)
end)
