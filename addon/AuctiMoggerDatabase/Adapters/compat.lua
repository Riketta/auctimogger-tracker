local _, A = ...
A.Compat = {}

-- Adapters/compat.lua - the manifest of every game surface the addon touches.
--
-- SPEC 7.4 names that surface in tiers; this file is that list, and the
-- single source of truth two gates diff the lint allowlists against
-- (scripts/check_compat.lua). A name is added here and in addon/.luacheckrc
-- and addon/.luarc.json in the same change, and nowhere else. It stays
-- self-contained: the guard runs this file alone, into an empty namespace.
--
-- Every optional entry has a fallback sentence here, so `/amdb status` can say
-- what the addon does without it. `check()` reads an environment and never
-- throws: a client that lacks half of it only gets a longer report.
--
-- Tiers:
--   required globals and events: the addon cannot work without them.
--   optional globals and events: each one degrades one feature (see `fallbacks`).
--   authored: the globals this addon itself defines. The attached frame is
--     named, so the client creates its global, and those of its font strings.

---@class AmdbCompatManifest
---@field requiredGlobals string[] without any of these the addon cannot work
---@field requiredEvents string[] events the addon cannot work without
---@field optionalGlobals string[] each one has a fallback
---@field optionalEvents string[] each one has a fallback
---@field authored string[] globals this addon defines itself

--- One sentence per optional name, global or event: what the addon does
--- without it.
---@alias AmdbCompatFallbacks table<string, string>

--- What `Compat.check()` found in an environment.
---@class AmdbCompatReport
---@field ok boolean true when every required global is defined
---@field missing string[] required globals that are not defined, in manifest order
---@field absentOptional string[] optional globals that are not defined, in manifest order

---@class AmdbCompatModule
---@field requiredGlobals string[]
---@field requiredEvents string[]
---@field optionalGlobals string[]
---@field optionalEvents string[]
---@field authored string[]
---@field fallbacks AmdbCompatFallbacks
---@field manifest AmdbCompatManifest the five tiers under one table, for a reader that wants them as data
---@field check fun(env: table|nil): AmdbCompatReport
local Compat = A.Compat

Compat.requiredGlobals = { "CreateFrame", "GameTooltip", "UIParent", "time", "SlashCmdList" }
Compat.requiredEvents = { "ADDON_LOADED", "PLAYER_LOGIN" }

Compat.optionalGlobals = {
    "TooltipDataProcessor",
    "Enum",
    "ItemRefTooltip",
    "issecretvalue",
    "IsShiftKeyDown",
    "Settings",
    "MenuUtil",
    "print",
}
Compat.optionalEvents = { "MODIFIER_STATE_CHANGED", "PLAYER_LOGOUT" }

Compat.authored = { "SLASH_AMDB1", "SLASH_AMDB2", "AuctiMoggerDatabase", "AuctiMoggerDatabaseTip" }

Compat.fallbacks = {
    TooltipDataProcessor = "hook OnTooltipSetItem on the two tooltip frames instead (SPEC 7.5.1)",
    Enum = "the processor path needs its tooltip data types: use the OnTooltipSetItem hooks instead, and battle-pet tooltips get no extra tip",
    ItemRefTooltip = "item links in chat get no extra tip; the GameTooltip still does",
    issecretvalue = "values are used unscreened; a client without it exposes no secret values",
    IsShiftKeyDown = "the detail level is unavailable: tooltips stay at the standard level",
    Settings = "the options panel is skipped with a one-time note; the slash command covers everything",
    MenuUtil = "the primary-market picker of the options panel moves to the next market on each click instead of opening a menu",
    print = "no chat output; reports still reach /amdb status",
    MODIFIER_STATE_CHANGED = "Shift is read when a tooltip is built: a held or released Shift shows on the next tooltip only",
    PLAYER_LOGOUT = "settings are written back as soon as they change, so nothing is left for the logout to save",
}

-- The same arrays, not copies: the two views cannot drift apart.
Compat.manifest = {
    requiredGlobals = Compat.requiredGlobals,
    requiredEvents = Compat.requiredEvents,
    optionalGlobals = Compat.optionalGlobals,
    optionalEvents = Compat.optionalEvents,
    authored = Compat.authored,
}

--- The names of `names` that `probe` does not define.
---@param probe table the environment read
---@param names string[] the names to look for
---@return string[] absent in the order of `names`
local function absentFrom(probe, names)
    local absent = {}
    for i = 1, #names do
        local name = names[i]
        if rawget(probe, name) == nil then
            absent[#absent + 1] = name
        end
    end
    return absent
end

--- What an environment holds, for `/amdb status`. Only globals can be probed:
--- an event is a string the client's event system answers, not a value, so
--- the event tiers are listed but never checked.
---@param env table|nil a table to probe in place of the global environment
---@return AmdbCompatReport report
function Compat.check(env)
    local probe = env
    if probe == nil then
        probe = _G
    elseif type(probe) ~= "table" then
        -- Junk is an environment that holds nothing: reported, never raised.
        probe = {}
    end
    local missing = absentFrom(probe, Compat.requiredGlobals)
    local absentOptional = absentFrom(probe, Compat.optionalGlobals)
    return { ok = #missing == 0, missing = missing, absentOptional = absentOptional }
end

return Compat
