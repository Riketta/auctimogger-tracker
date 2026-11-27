-- Specs for bootstrap.lua, the composition root: the frame it creates, the
-- settings lifecycle (ADDON_LOADED, PLAYER_LOGOUT), the adapters it builds at
-- PLAYER_LOGIN and the lines the login prints (SPEC 7.11).
--
-- CreateFrame is stubbed: an event frame that records RegisterEvent and
-- SetScript, and a tooltip frame that accepts its setup calls. The stubs go
-- into _G with rawset: a spec file's own globals are private to it, and the
-- addon is loaded with loadfile. The real namespace is loaded in TOC order,
-- slot files excepted, and events are delivered by calling the recorded
-- OnEvent script.

local Loader = require("spec.helpers.loader")

local ADDON = "AuctiMoggerDatabase"
local SETTINGS = "AuctiMoggerDatabaseSettings"
local CHARACTER = "AuctiMoggerDatabaseCharacter"

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
-- The note of SPEC 7.8, as the bootstrap records it (info prints muted).
local NO_PANEL = "no options panel on this client — the /amdb command covers everything"
-- What a version 3 carrier left in a slot is told by (SPEC 7.11).
local LEGACY_FEED = "a version 3 tracker feed is in a market slot and was not read — update the tracker"
    .. " and download the market again (that slot file opens with AuctiMoggerTrackerFeed)"

--- Everything the TOC loads, slot files excepted.
local FILES = {
    "Core/pack.lua",
    "Core/record.lua",
    "Core/market.lua",
    "Core/settings.lua",
    "Core/format.lua",
    "Core/resolve.lua",
    "Core/tooltip_lines.lua",
    "Core/anchor.lua",
    "Core/panel_rows.lua",
    "Adapters/market_files.lua",
    "Adapters/compat.lua",
    "Adapters/extra_tip.lua",
    "Adapters/tooltip.lua",
    "Adapters/options.lua",
    "Adapters/public_api.lua",
    "bootstrap.lua",
}

local MARKET = [[
local _, A = ...
local m = A.BeginMarket({ version = %d, trackerVersion = "0.1.0", target = %s, generatedAt = %s })
A.EndMarket()
]]

local STUBS = {
    "CreateFrame",
    "UIParent",
    "SlashCmdList",
    "SLASH_AMDB1",
    "SLASH_AMDB2",
    "AuctiMoggerDatabase",
    "GetAuctionBuyout",
    SETTINGS,
    CHARACTER,
    "Settings",
    "MenuUtil",
    "GetMoneyString",
    "AuctiMoggerTrackerFeed",
}

-- What registering the options panel creates at the login, after the tooltip:
-- its frame, then one control per row of the model (Core/panel_rows.lua): the
-- master switch's checkbox, the two preset buttons, eight line checkboxes, Shift
-- detail's and the coin icons', and the primary-market picker. A font string is
-- not a frame.
local PANEL_CREATED = {
    "Frame",
    "CheckButton",
    "Button",
    "Button",
    "CheckButton",
    "CheckButton",
    "CheckButton",
    "CheckButton",
    "CheckButton",
    "CheckButton",
    "CheckButton",
    "CheckButton",
    "CheckButton",
    "CheckButton",
    "Button",
}
local CREATED_AT_LOGIN = { "Frame", "GameTooltip", unpack(PANEL_CREATED) }

--- A frame that records RegisterEvent, SetScript and the font strings it is
--- asked for; any other method is a no-op, so a tooltip frame accepts the
--- setup calls the adapter makes and a control the ones the panel makes.
---@return table frame
local function newFrame(...)
    local frame = { args = { ... }, events = {}, scripts = {}, strings = {} }
    function frame:RegisterEvent(event)
        self.events[#self.events + 1] = event
    end
    function frame:SetScript(scriptType, handler)
        self.scripts[scriptType] = handler
    end
    function frame:CreateFontString()
        local text = setmetatable({ shown = true }, {
            __index = function()
                return function() end
            end,
        })
        function text:SetText(value)
            self.text = value
        end
        function text:Hide()
            self.shown = false
        end
        self.strings[#self.strings + 1] = text
        return text
    end
    return setmetatable(frame, {
        __index = function()
            return function() end
        end,
    })
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
    local tipFrame
    local panelFrame
    local created
    local saved
    local settingsCanvases
    local settingsCategories

    --- What CreateFrame hands out: the bootstrap's event frame is the first
    --- plain frame, the tooltip adapter's frame the GameTooltip, and any later
    --- plain frame the options panel's. The kind asked for is recorded; a
    --- control of the panel belongs to no one of the three.
    local function makeFrame(...)
        local args = { ... }
        created[#created + 1] = args[1]
        local made = newFrame(unpack(args))
        if args[1] == "GameTooltip" then
            tipFrame = made
        elseif args[1] == "Frame" then
            if frame == nil then
                frame = made
            else
                panelFrame = made
            end
        end
        return made
    end

    before_each(function()
        saved = {}
        for _, name in ipairs(STUBS) do
            saved[name] = rawget(_G, name)
        end
        created = {}
        frame = nil
        tipFrame = nil
        panelFrame = nil
        settingsCanvases = {}
        settingsCategories = {}
        rawset(_G, "CreateFrame", makeFrame)
        rawset(_G, "UIParent", {})
        rawset(_G, "SlashCmdList", {})
        rawset(_G, "SLASH_AMDB1", nil)
        rawset(_G, "SLASH_AMDB2", nil)
        rawset(_G, SETTINGS, nil)
        rawset(_G, CHARACTER, nil)
        -- The Settings API, as a current client has it: the options panel
        -- registers with it and nothing is said of it.
        rawset(_G, "Settings", {
            RegisterCanvasLayoutCategory = function(owner, name)
                local category = { name = name }
                local layout = { anchors = {} }
                function layout:AddAnchorPoint(point, x, y)
                    self.anchors[#self.anchors + 1] = { point, x, y }
                end
                settingsCanvases[#settingsCanvases + 1] = { frame = owner, name = name, layout = layout }
                return category, layout
            end,
            RegisterAddOnCategory = function(category)
                settingsCategories[#settingsCategories + 1] = category
            end,
        })
        rawset(_G, "MenuUtil", nil)
        rawset(_G, "GetMoneyString", nil)
        rawset(_G, "AuctiMoggerTrackerFeed", nil)
    end)

    after_each(function()
        for _, name in ipairs(STUBS) do
            rawset(_G, name, saved[name])
        end
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
    ---@param arg1 any the event's first argument
    local function fire(event, arg1)
        local handler = frame.scripts.OnEvent
        assert.is_not_nil(handler, "bootstrap installed no OnEvent script")
        handler(frame, event, arg1)
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

    ------------------------------------------------------------- the frame

    it("returns the namespace", function()
        local namespace = Loader.load(Loader.namespace(), unpack(FILES, 1, #FILES - 1))
        local chunk = assert(loadfile(Loader.ROOT .. "/bootstrap.lua"))
        local module = chunk(Loader.ADDON, namespace)
        assert.equals(namespace, module)
    end)

    it("creates its event frame at load and registers the lifecycle events", function()
        boot()
        assert.same({ "Frame" }, created)
        assert.same({ "ADDON_LOADED", "PLAYER_LOGIN", "MODIFIER_STATE_CHANGED", "PLAYER_LOGOUT" }, frame.events)
        assert.same({ "OnEvent" }, keysOf(frame.scripts))
        assert.equals("function", type(frame.scripts.OnEvent))
    end)

    it("builds the tooltip adapter and the /amdb command once, at the login", function()
        local A = boot()
        assert.is_nil(A.tooltip)
        assert.is_nil(A.options)
        fire("PLAYER_LOGIN")
        assert.is_table(A.tooltip)
        assert.equals("none", A.tooltip.path)
        assert.is_table(A.options)
        assert.equals("function", type(rawget(_G, "SlashCmdList").AMDB))
        -- The frame the adapter created is the one CreateFrame handed out.
        assert.equals(tipFrame, A.tooltip.tip.tip)
        assert.same(CREATED_AT_LOGIN, created)
        -- A second login builds nothing twice.
        fire("PLAYER_LOGIN")
        assert.same(CREATED_AT_LOGIN, created)
    end)

    it("applies the coin icons the saved settings ask for at the login, once the client can draw them", function()
        rawset(_G, "GetMoneyString", function(copper)
            return "<" .. tostring(copper) .. ">"
        end)
        local A = boot()
        rawset(_G, SETTINGS, { coins = true })
        fire("ADDON_LOADED", ADDON)
        assert.is_true(A.settings.coins)
        -- Nothing is applied before the adapters are built at the login.
        assert.equals("1g 23s 45c", A.Core.Format.money(12345))
        fire("PLAYER_LOGIN")
        assert.equals("<12345>", A.Core.Format.money(12345))
    end)

    it("keeps the saved coin icons off where the client has no GetMoneyString", function()
        local A = boot()
        rawset(_G, SETTINGS, { coins = true })
        fire("ADDON_LOADED", ADDON)
        fire("PLAYER_LOGIN")
        -- The setting is the player's and survives; the prices stay in text.
        assert.is_true(A.settings.coins)
        assert.equals("1g 23s 45c", A.Core.Format.money(12345))
    end)

    ----------------------------------------------------------- SavedVariables

    describe("SavedVariables", function()
        it("loads the settings at ADDON_LOADED, keeping only the literals", function()
            local A = boot()
            rawset(_G, SETTINGS, { enabled = false, shiftDetail = "yes", lines = { deal = false, craft = "nope" } })
            fire("ADDON_LOADED", ADDON)
            assert.is_false(A.settings.enabled)
            assert.is_true(A.settings.shiftDetail)
            assert.is_false(A.settings.lines.deal)
            assert.is_true(A.settings.lines.craft)
            -- The cleaned table is what the client will save.
            assert.equals(A.settings, rawget(_G, SETTINGS))
        end)

        it("answers the defaults for a first run", function()
            local A = boot()
            fire("ADDON_LOADED", ADDON)
            assert.same(A.Core.Settings.defaults(), A.settings)
            assert.same({ primaryMarket = nil }, A.character)
        end)

        it("ignores the ADDON_LOADED of another addon", function()
            local A = boot()
            rawset(_G, SETTINGS, { enabled = false })
            fire("ADDON_LOADED", "SomeOtherAddon")
            assert.is_nil(A.settings)
            fire("ADDON_LOADED", ADDON)
            assert.is_false(A.settings.enabled)
        end)

        it("restores the primary market this character chose", function()
            local A, lines = boot()
            Loader.carrier(A, GOLDEN)
            secondMarket(A, OTHER, NOW - 600)
            rawset(_G, CHARACTER, { primaryMarket = OTHER })
            fire("ADDON_LOADED", ADDON)
            assert.equals(OTHER, A.store:primaryTarget())
            fire("PLAYER_LOGIN")
            assert.same({ status("2 markets", OTHER, MUTED, "10m") }, lines)
        end)

        it("keeps a choice that names no loaded market, and the login line asks", function()
            local A, lines = boot()
            Loader.carrier(A, GOLDEN)
            secondMarket(A, OTHER, NOW - 600)
            rawset(_G, CHARACTER, { primaryMarket = "not-loaded" })
            fire("ADDON_LOADED", ADDON)
            fire("PLAYER_LOGIN")
            assert.equals(status("2 markets", TARGET, MUTED, "16m") .. " · " .. PICK_ONE, lines[1])
        end)

        it("writes a change back the moment it happens, and again at the logout", function()
            local A, lines = boot()
            Loader.carrier(A, GOLDEN)
            fire("ADDON_LOADED", ADDON)
            fire("PLAYER_LOGIN")
            clear(lines)
            assert.equals(A.settings, rawget(_G, SETTINGS))
            A.options:run("deal off")
            assert.is_false(rawget(_G, SETTINGS).lines.deal)
            -- The command's own answer, and nothing about the saving.
            assert.same({ TITLE .. " deal line off" }, lines)
            fire("PLAYER_LOGOUT")
            assert.equals(A.settings, rawget(_G, SETTINGS))
            assert.is_false(rawget(_G, SETTINGS).lines.deal)
        end)

        it("writes the primary market of this character back when it changes", function()
            local A = boot()
            Loader.carrier(A, GOLDEN)
            secondMarket(A, OTHER, NOW - 600)
            fire("ADDON_LOADED", ADDON)
            fire("PLAYER_LOGIN")
            A.options:run("market " .. OTHER)
            assert.equals(OTHER, rawget(_G, CHARACTER).primaryMarket)
        end)

        it("finds settings at a login that no ADDON_LOADED preceded", function()
            local A, lines = boot()
            rawset(_G, SETTINGS, { enabled = false })
            fire("PLAYER_LOGIN")
            assert.is_false(A.settings.enabled)
            assert.equals(A.settings, rawget(_G, SETTINGS))
            assert.same({ NO_DATA }, lines)
        end)
    end)

    -------------------------------------------------------------- the login

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
        -- The settings are loaded before a choice is made, as in the client.
        fire("ADDON_LOADED", ADDON)
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

    it("reports a version 3 feed left in a slot, in bad, once", function()
        -- The v3 carrier is a plain table: it calls no loader, so nothing was
        -- recorded while it loaded and no slot can be named. bootstrap.lua reads
        -- its global at load, after every slot has run.
        rawset(_G, "AuctiMoggerTrackerFeed", { version = 3, generatedAt = NOW - 600 })
        local A, lines = boot()
        Loader.carrier(A, GOLDEN)
        fire("PLAYER_LOGIN")
        assert.same({
            status("1 market", TARGET, MUTED, "16m"),
            TITLE .. " " .. BAD .. LEGACY_FEED .. RESET,
        }, lines)
        assert.same({}, A.TakeReports())
        -- Not ours to clear: the global is still there, and it is not said twice.
        assert.equals("table", type(rawget(_G, "AuctiMoggerTrackerFeed")))
        clear(lines)
        fire("PLAYER_LOGIN")
        assert.same({ status("1 market", TARGET, MUTED, "16m") }, lines)
    end)

    it("says it after the no-data line when the old feed is all there is", function()
        rawset(_G, "AuctiMoggerTrackerFeed", { version = 3 })
        local _, lines = boot()
        fire("PLAYER_LOGIN")
        assert.same({ NO_DATA, TITLE .. " " .. BAD .. LEGACY_FEED .. RESET }, lines)
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

    ------------------------------------------------------- a feature that fails

    it("leaves the tooltips out, reported in bad, when their frame cannot be created", function()
        rawset(_G, "CreateFrame", function(kind, ...)
            if kind == "GameTooltip" then
                error("no tooltips on this client", 0)
            end
            return makeFrame(kind, ...)
        end)
        local A, lines = boot()
        Loader.carrier(A, GOLDEN)
        local fired = 0
        A.OnUpdate(function()
            fired = fired + 1
        end)
        fire("PLAYER_LOGIN")
        assert.is_nil(A.tooltip)
        -- The command is there all the same, and knows no tooltip adapter.
        assert.is_table(A.options)
        assert.is_nil(A.options.tooltip)
        assert.same({
            status("1 market", TARGET, MUTED, "16m"),
            TITLE .. " " .. BAD .. "tooltips could not start: no tooltips on this client" .. RESET,
        }, lines)
        assert.equals(1, fired)
    end)

    it("reports a /amdb command that cannot be registered, and goes on", function()
        rawset(_G, "SlashCmdList", nil)
        local A, lines = boot()
        Loader.carrier(A, GOLDEN)
        fire("PLAYER_LOGIN")
        assert.is_table(A.options)
        assert.equals(2, #lines)
        assert.equals(status("1 market", TARGET, MUTED, "16m"), lines[1])
        -- The interpreter's message follows; the lead is ours.
        assert.equals(1, lines[2]:find(TITLE .. " " .. BAD .. "the /amdb command could not start: ", 1, true))
    end)

    ---------------------------------------------------------- the options panel

    --- The text of the panel's last font string: the status line's row.
    ---@return string|nil text
    local function panelStatus()
        assert.is_not_nil(panelFrame, "the options panel built no frame")
        local strings = panelFrame.strings
        return strings[#strings].text
    end

    it("registers the options panel with the Settings API at the login, once", function()
        local A, lines = boot()
        Loader.carrier(A, GOLDEN)
        assert.same({}, settingsCanvases)
        fire("PLAYER_LOGIN")
        assert.equals(1, #settingsCanvases)
        assert.equals("AuctiMogger Database", settingsCanvases[1].name)
        assert.equals(panelFrame, settingsCanvases[1].frame)
        assert.equals(A.options.panel.frame, panelFrame)
        assert.same({ { "TOPLEFT", 16, -16 } }, settingsCanvases[1].layout.anchors)
        assert.same({ { name = "AuctiMogger Database" } }, settingsCategories)
        -- The panel's last row is the status line of SPEC 7.11, as the clock had
        -- it; the login itself prints that line and says nothing of the panel.
        assert.same({ status("1 market", TARGET, MUTED, "16m") }, lines)
        assert.equals(status("1 market", TARGET, MUTED, "16m"), panelStatus())
        -- A second login registers nothing twice.
        fire("PLAYER_LOGIN")
        assert.equals(1, #settingsCanvases)
        assert.same(CREATED_AT_LOGIN, created)
    end)

    it("notes a client without the Settings API once, and goes on", function()
        rawset(_G, "Settings", nil)
        local A, lines = boot()
        Loader.carrier(A, GOLDEN)
        fire("PLAYER_LOGIN")
        assert.same({
            status("1 market", TARGET, MUTED, "16m"),
            TITLE .. " " .. MUTED .. NO_PANEL .. RESET,
        }, lines)
        assert.is_nil(A.options.panel)
        assert.same({ "Frame", "GameTooltip" }, created)
        -- The command covers everything, as the note says.
        A.options:run("deal off")
        assert.is_false(A.settings.lines.deal)
    end)

    it("reports an options panel that cannot start, in bad, and keeps the command", function()
        rawset(_G, "CreateFrame", function(kind, ...)
            if kind == "CheckButton" then
                error("no checkboxes on this client", 0)
            end
            return makeFrame(kind, ...)
        end)
        local A, lines = boot()
        Loader.carrier(A, GOLDEN)
        fire("PLAYER_LOGIN")
        assert.same({
            status("1 market", TARGET, MUTED, "16m"),
            TITLE .. " " .. BAD .. "the options panel could not start: no checkboxes on this client" .. RESET,
        }, lines)
        assert.is_nil(A.options.panel)
        -- Nothing half-built reached the client.
        assert.same({}, settingsCanvases)
        assert.same({}, settingsCategories)
        -- The command is there all the same.
        A.options:run("deal off")
        assert.is_false(A.settings.lines.deal)
    end)

    ---------------------------------------------------------------- the events

    it("routes MODIFIER_STATE_CHANGED to the tooltip adapter", function()
        local A = boot()
        fire("PLAYER_LOGIN")
        local seen = {}
        A.tooltip.onModifier = function(_, key)
            seen[#seen + 1] = key
        end
        fire("MODIFIER_STATE_CHANGED", "LSHIFT")
        fire("MODIFIER_STATE_CHANGED", "RSHIFT")
        assert.same({ "LSHIFT", "RSHIFT" }, seen)
    end)

    it("survives a modifier handler that raised", function()
        local A, lines = boot()
        Loader.carrier(A, GOLDEN)
        fire("PLAYER_LOGIN")
        A.tooltip.onModifier = function()
            error("shift broke", 0)
        end
        fire("MODIFIER_STATE_CHANGED", "LSHIFT")
        assert.same({
            status("1 market", TARGET, MUTED, "16m"),
            TITLE .. " " .. BAD .. "shift broke" .. RESET,
        }, lines)
    end)

    it("ignores an event it did not register for", function()
        local A, lines = boot()
        Loader.carrier(A, GOLDEN)
        fire("PLAYER_ENTERING_WORLD")
        fire("ADDON_LOADED")
        assert.same({}, lines)
        assert.same({}, A.TakeReports())
    end)
end)
