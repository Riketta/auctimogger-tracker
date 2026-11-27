local ADDON, A = ...

-- bootstrap.lua - the composition root.
--
-- Loads last (TOC order) and wires what the earlier files published on the
-- namespace into the game. It owns one frame and the SavedVariables
-- lifecycle:
--
--   ADDON_LOADED   the client hands over the saved settings. They are
--                  coerced (only the literals count), the primary market
--                  this character chose is applied, and both tables are
--                  written back in their clean shape;
--   PLAYER_LOGIN   the first moment the player can read chat. The tooltip
--                  adapter, the /amdb command and the options panel are built
--                  here, once: the attached frame is created and the hooks
--                  registered. Then the status line of SPEC 7.11 and every
--                  report the slot files recorded while they loaded are
--                  printed, and the update callbacks run, so subscribers see
--                  the markets;
--   PLAYER_LOGOUT  the settings are written back once more. Every change
--                  already was, the moment it happened;
--   MODIFIER_STATE_CHANGED  reaches the tooltip adapter, which rebuilds a
--                  shown frame at the other detail level.
--
-- A feature that cannot start is reported in bad and left out; the login
-- line and the update callbacks run all the same.
--
-- A version 3 feed left in a market slot registers nothing and records
-- nothing; it is reported here, at load, once every slot has run (SPEC 7.11).
--
-- The status line is a pure function of the store and `now`: a spec reads it
-- through a stubbed frame, without a client.

-- Wiring invariants of the TOC order, failed loudly rather than at the first call.
assert(
    type(A.Tooltip) == "table" and type(A.Tooltip.new) == "function",
    ADDON .. ": A.Tooltip must be set before bootstrap.lua loads"
)
assert(
    type(A.Options) == "table" and type(A.Options.new) == "function",
    ADDON .. ": A.Options must be set before bootstrap.lua loads"
)

-- The TOC loads this file after every slot. A version 3 feed left in one
-- called no loader, so nothing recorded it: said here, the login prints it
-- with the reports the slot files did record.
A.ReportLegacyFeed()

---@type AmdbFormatModule
local Format = A.Core.Format
---@type AmdbSettingsModule
local Preferences = A.Core.Settings
---@type AmdbTooltip
local Tooltip = A.Tooltip
---@type AmdbOptions
local Options = A.Options

---@type AmdbStatusTexts
local TEXT = Options.TEXT

--- The slice of the client's Frame this file calls. The client's frame type is
--- declared nowhere in this workspace, so the methods used are listed here.
---@class AmdbEventFrame
---@field RegisterEvent fun(self: AmdbEventFrame, eventName: string)
---@field SetScript fun(self: AmdbEventFrame, scriptType: string, handler: fun(self: AmdbEventFrame, event: string, arg1: any))

-- The two SavedVariables the TOC declares.
local SETTINGS_VARIABLE = ADDON .. "Settings"
local CHARACTER_VARIABLE = ADDON .. "Character"

--- SPEC 7.11, for a market other than the primary that is past 24 hours:
--- shown rather than hidden (SPEC 5.9), so the user knows to refresh it.
local RESYNC = " old — resync"

--- Writes both settings tables back to the SavedVariables, so the client
--- saves what the player chose. Called as soon as anything changes, and once
--- more at the logout.
function A.PersistSettings()
    if A.settings ~= nil then
        rawset(_G, SETTINGS_VARIABLE, A.settings)
    end
    if A.character ~= nil then
        rawset(_G, CHARACTER_VARIABLE, A.character)
    end
end

--- Reads the SavedVariables into the live settings (`A.settings`,
--- `A.character`). Saved data is untrusted: Core/settings.lua keeps only the
--- literals and answers the defaults for everything else. The primary market
--- this character chose is applied even when that market is not loaded: the
--- store then resolves to its default and `/amdb status` says so.
local function loadSettings()
    A.settings = Preferences.coerce(rawget(_G, SETTINGS_VARIABLE))
    A.character = Preferences.coerceCharacter(rawget(_G, CHARACTER_VARIABLE))
    A.PersistSettings()
    A.store:setExplicitPrimary(A.character.primaryMarket)
end

--- The login status line (SPEC 7.11). Pure: it reads the store and `now`, and
--- nothing else. Stale markets other than the primary follow as a suffix
--- each, because the primary's own age is already in the line.
---@param store AmdbStore
---@param now number epoch seconds
---@return string line
local function statusLine(store, now)
    local primary = store:primary()
    if primary == nil then
        return Format.PREFIX .. ": " .. Format.colorize("bad", TEXT.NO_DATA)
    end

    local markets = store:markets()
    local age = primary:ageSeconds(now)
    local parts = {
        #markets .. (#markets == 1 and " market" or " markets"),
        "primary " .. primary.target,
        "data " .. Format.colorize(Format.ageColor(age), Format.age(age)) .. " old",
    }
    if store:primaryIsAmbiguous() then
        parts[#parts + 1] = TEXT.PICK_ONE
    end
    for i = 1, #markets do
        local market = markets[i]
        if market ~= primary and market:isStale(now) then
            parts[#parts + 1] =
                Format.colorize("hot", market.target .. " is " .. Format.age(market:ageSeconds(now)) .. RESYNC)
        end
    end
    return Format.PREFIX .. ": " .. table.concat(parts, " · ")
end

--- Prints the reports recorded so far, one line each in the color of their
--- level, and clears them.
local function printReports()
    local reports = A.TakeReports()
    for i = 1, #reports do
        local report = reports[i]
        A.chat(A.FormatReport(report.text, report.level))
    end
end

--- The options panel of SPEC 7.8, on the options the command built. A client
--- without the Settings API goes without it, with a note; a panel that raises
--- is reported in bad and left out. The command stays either way.
---@param options AmdbOptions
local function registerPanel(options)
    local built, result = pcall(options.registerPanel, options)
    if not built then
        A.Say("the options panel could not start: " .. tostring(result), "bad")
    elseif result == false then
        A.Say(Options.TEXT.NO_PANEL, "info")
    end
end

--- Builds the tooltip adapter and the options once, and registers the hooks,
--- the slash command and the options panel. All of them touch the client's
--- UI, so each is built under pcall: what cannot start is reported in bad and
--- left out.
local function buildAdapters()
    if A.tooltip == nil then
        local ok, tooltip = pcall(Tooltip.new, { store = A.store, settings = A.settings, say = A.SayNow })
        if ok and type(tooltip) == "table" then
            A.tooltip = tooltip
            local hooked, err = pcall(tooltip.register, tooltip)
            if not hooked then
                A.Say("tooltip hooks could not be installed: " .. tostring(err), "bad")
            end
        else
            A.Say("tooltips could not start: " .. tostring(tooltip), "bad")
        end
    end
    if A.options == nil then
        local ok, options = pcall(Options.new, {
            store = A.store,
            settings = A.settings,
            character = A.character,
            tooltip = A.tooltip,
            say = A.chat,
            persist = A.PersistSettings,
            statusLine = statusLine,
        })
        if ok and type(options) == "table" then
            A.options = options
            -- The loaded settings said whether prices read as coin icons: the
            -- formatter follows them before the first tooltip is built.
            options:syncCoins()
            local registered, err = pcall(options.register, options)
            if not registered then
                A.Say("the /amdb command could not start: " .. tostring(err), "bad")
            end
            registerPanel(options)
        else
            A.Say("the /amdb command could not start: " .. tostring(options), "bad")
        end
    end
end

--- PLAYER_LOGIN: the adapters, the status line, the reports, the callbacks.
--- A login that arrives before ADDON_LOADED (no client: a harness) still
--- finds settings.
local function login()
    if A.settings == nil then
        loadSettings()
    end
    buildAdapters()
    A.chat(statusLine(A.store, A.clock()))
    printReports()
    -- The store keeps one failing callback from stopping the others; what it
    -- caught is reported here, once, in bad.
    local errors = A.store:fireUpdate()
    for i = 1, #errors do
        A.Say(errors[i], "bad")
    end
    if not A.verbose then
        -- Not printed by Say: login is the one moment these reach chat.
        printReports()
    end
end

--- The frame's OnEvent.
---@param _ AmdbEventFrame the frame the script runs on
---@param event string
---@param arg1 any the addon name (ADDON_LOADED) or the modifier key
local function onEvent(_, event, arg1)
    if event == "ADDON_LOADED" then
        if arg1 == ADDON then
            loadSettings()
        end
    elseif event == "PLAYER_LOGIN" then
        login()
    elseif event == "PLAYER_LOGOUT" then
        A.PersistSettings()
    elseif event == "MODIFIER_STATE_CHANGED" then
        if A.tooltip ~= nil then
            local ok, err = pcall(A.tooltip.onModifier, A.tooltip, arg1)
            if not ok then
                A.SayNow(tostring(err), "bad")
            end
        end
    end
end

---@type AmdbEventFrame
local frame = CreateFrame("Frame")
frame:RegisterEvent("ADDON_LOADED")
frame:RegisterEvent("PLAYER_LOGIN")
-- Optional events: a client that refuses one (a fork's event policing) only
-- loses the feature it carries, so a refusal must not fail the login.
pcall(frame.RegisterEvent, frame, "MODIFIER_STATE_CHANGED")
pcall(frame.RegisterEvent, frame, "PLAYER_LOGOUT")
frame:SetScript("OnEvent", onEvent)

return A
