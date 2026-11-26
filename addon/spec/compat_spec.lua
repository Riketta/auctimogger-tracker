-- Specs for Adapters/compat.lua: the manifest of SPEC 7.4 and the check()
-- that `/amdb status` reads.

local Loader = require("spec.helpers.loader")

local REQUIRED_GLOBALS = { "CreateFrame", "GameTooltip", "UIParent", "time", "SlashCmdList" }
local REQUIRED_EVENTS = { "ADDON_LOADED", "PLAYER_LOGIN" }
local OPTIONAL_GLOBALS = {
    "TooltipDataProcessor",
    "Enum",
    "ItemRefTooltip",
    "issecretvalue",
    "IsShiftKeyDown",
    "Settings",
    "MenuUtil",
    "print",
}
local OPTIONAL_EVENTS = { "MODIFIER_STATE_CHANGED", "PLAYER_LOGOUT" }
local AUTHORED = { "SLASH_AMDB1", "SLASH_AMDB2", "AuctiMoggerDatabase", "AuctiMoggerDatabaseTip" }
local TIERS = { "requiredGlobals", "requiredEvents", "optionalGlobals", "optionalEvents", "authored" }

-- Spelled in two parts, so that this file never holds the forbidden name either.
local FORBIDDEN = "C_Auction" .. "House"

--- A fresh namespace with compat.lua loaded alone, the way the guard loads it.
---@return table A namespace
---@return table Compat the manifest module
local function fresh()
    local A = Loader.load(Loader.namespace(), "Adapters/compat.lua")
    return A, A.Compat
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

--- An environment that defines every global the manifest names.
---@return table env
local function full()
    local env = {}
    for _, list in ipairs({ REQUIRED_GLOBALS, OPTIONAL_GLOBALS }) do
        for i = 1, #list do
            env[list[i]] = true
        end
    end
    return env
end

--- Every string anywhere below `value`.
---@param value any
---@param out string[]|nil collected into
---@return string[] out
local function stringsOf(value, out)
    out = out or {}
    if type(value) == "string" then
        out[#out + 1] = value
    elseif type(value) == "table" then
        for _, child in pairs(value) do
            stringsOf(child, out)
        end
    end
    return out
end

describe("compat: the module", function()
    it("loads alone, into an empty namespace, and returns its table", function()
        local namespace = Loader.namespace()
        local chunk = assert(loadfile(Loader.ROOT .. "/Adapters/compat.lua"))
        local module = chunk(Loader.ADDON, namespace)
        assert.equals(namespace.Compat, module)
        sameList(REQUIRED_GLOBALS, module.requiredGlobals)
        assert.equals("function", type(module.check))
    end)

    it("registers into the namespace", function()
        local A, Compat = fresh()
        assert.equals(Compat, A.Compat)
    end)
end)

describe("compat: the manifest", function()
    it("lists the five tiers exactly as SPEC 7.4 names them", function()
        local _, Compat = fresh()
        sameList(REQUIRED_GLOBALS, Compat.requiredGlobals)
        sameList(REQUIRED_EVENTS, Compat.requiredEvents)
        sameList(OPTIONAL_GLOBALS, Compat.optionalGlobals)
        sameList(OPTIONAL_EVENTS, Compat.optionalEvents)
        sameList(AUTHORED, Compat.authored)
    end)

    it("never lists a name twice", function()
        local _, Compat = fresh()
        local seen = {}
        for _, tier in ipairs(TIERS) do
            for _, name in ipairs(Compat[tier]) do
                assert.is_nil(seen[name], name .. " is listed twice")
                seen[name] = true
                assert.equals("string", type(name))
                assert.is_true(#name > 0)
            end
        end
    end)

    it("repeats the five tiers under manifest, as the very same arrays", function()
        local _, Compat = fresh()
        local sorted = {}
        for i, tier in ipairs(TIERS) do
            sorted[i] = tier
        end
        table.sort(sorted)
        sameList(sorted, keysOf(Compat.manifest))
        for _, tier in ipairs(TIERS) do
            assert.equals(Compat[tier], Compat.manifest[tier])
        end
    end)

    it("never names the auction house API", function()
        local _, Compat = fresh()
        for _, text in ipairs(stringsOf(Compat)) do
            assert.is_nil(text:find(FORBIDDEN, 1, true), "the manifest names " .. FORBIDDEN)
        end
        for _, name in ipairs(keysOf(Compat)) do
            assert.is_nil(tostring(name):find(FORBIDDEN, 1, true))
        end
    end)
end)

describe("compat: fallbacks", function()
    it("documents a fallback for every optional entry, and for nothing else", function()
        local _, Compat = fresh()
        local expected = {}
        for _, list in ipairs({ OPTIONAL_GLOBALS, OPTIONAL_EVENTS }) do
            for i = 1, #list do
                expected[#expected + 1] = list[i]
            end
        end
        table.sort(expected)
        sameList(expected, keysOf(Compat.fallbacks))
        for _, sentence in pairs(Compat.fallbacks) do
            assert.equals("string", type(sentence))
            assert.is_true(#sentence > 0)
            assert.is_nil(sentence:find("\n", 1, true))
        end
    end)

    it("says what the addon does without each surface", function()
        local _, Compat = fresh()
        local fallbacks = Compat.fallbacks
        assert.is_not_nil(fallbacks.TooltipDataProcessor:find("OnTooltipSetItem", 1, true))
        assert.is_not_nil(fallbacks.TooltipDataProcessor:find("SPEC 7.5.1", 1, true))
        assert.is_not_nil(fallbacks.print:find("/amdb status", 1, true))
        assert.is_not_nil(fallbacks.Settings:find("slash command", 1, true))
        assert.is_not_nil(fallbacks.MenuUtil:find("next market", 1, true))
        -- A required or authored surface has no fallback to document.
        assert.is_nil(fallbacks.CreateFrame)
        assert.is_nil(fallbacks.AuctiMoggerDatabase)
        assert.is_nil(fallbacks.ADDON_LOADED)
    end)
end)

describe("compat: check", function()
    it("answers ok for an environment that defines every global", function()
        local _, Compat = fresh()
        local report = Compat.check(full())
        assert.is_true(report.ok)
        assert.same({}, report.missing)
        assert.same({}, report.absentOptional)
        sameList({ "absentOptional", "missing", "ok" }, keysOf(report))
    end)

    it("lists the required globals that are missing, in manifest order", function()
        local _, Compat = fresh()
        local env = full()
        env.UIParent = nil
        env.CreateFrame = nil
        local report = Compat.check(env)
        assert.is_false(report.ok)
        sameList({ "CreateFrame", "UIParent" }, report.missing)
        assert.same({}, report.absentOptional)
    end)

    it("lists the optional globals that are absent without failing", function()
        local _, Compat = fresh()
        local env = full()
        env.print = nil
        env.Settings = nil
        env.time = nil
        local report = Compat.check(env)
        assert.is_false(report.ok)
        sameList({ "time" }, report.missing)
        sameList({ "Settings", "print" }, report.absentOptional)
        -- Optional ones alone do not make the environment unusable.
        env.time = true
        assert.is_true(Compat.check(env).ok)
    end)

    it("never throws on a sparse table", function()
        local _, Compat = fresh()
        local report = Compat.check({ CreateFrame = true })
        assert.is_false(report.ok)
        sameList({ "GameTooltip", "UIParent", "time", "SlashCmdList" }, report.missing)
        sameList(OPTIONAL_GLOBALS, report.absentOptional)
        -- An array with holes, and nothing at all.
        assert.is_false(Compat.check({ "one", "two" }).ok)
        assert.is_false(Compat.check({}).ok)
        sameList(REQUIRED_GLOBALS, Compat.check({}).missing)
    end)

    it("never throws on junk in place of a table", function()
        local _, Compat = fresh()
        for _, junk in ipairs({ 42, "CreateFrame", true, false, print, Compat }) do
            local report = Compat.check(junk)
            assert.is_false(report.ok)
            sameList(REQUIRED_GLOBALS, report.missing)
            sameList(OPTIONAL_GLOBALS, report.absentOptional)
        end
    end)

    it("reads the global environment when none is given", function()
        local _, Compat = fresh()
        local report = Compat.check()
        assert.equals("boolean", type(report.ok))
        assert.equals(#report.missing == 0, report.ok)
        for _, name in ipairs(report.missing) do
            assert.is_true(holds(REQUIRED_GLOBALS, name))
        end
        for _, name in ipairs(report.absentOptional) do
            assert.is_true(holds(OPTIONAL_GLOBALS, name))
        end
        -- The same answer as a copy of _G probed explicitly.
        local copy = {}
        for name, value in pairs(_G) do
            copy[name] = value
        end
        sameList(Compat.check(copy).missing, report.missing)
        sameList(Compat.check(copy).absentOptional, report.absentOptional)
    end)

    it("reads with rawget: a metatable's fallback is not a definition", function()
        local _, Compat = fresh()
        local shadowed = setmetatable({}, { __index = full() })
        assert.is_not_nil(shadowed.CreateFrame)
        local report = Compat.check(shadowed)
        assert.is_false(report.ok)
        sameList(REQUIRED_GLOBALS, report.missing)
        sameList(OPTIONAL_GLOBALS, report.absentOptional)
    end)

    it("leaves the environment it reads as it found it", function()
        local _, Compat = fresh()
        local env = full()
        env.UIParent = nil
        local before = keysOf(env)
        Compat.check(env)
        Compat.check(env)
        sameList(before, keysOf(env))
        assert.is_nil(env.UIParent)
    end)

    it("checks globals only: the event tiers are listed, not probed", function()
        local _, Compat = fresh()
        local env = full()
        for _, list in ipairs({ REQUIRED_EVENTS, OPTIONAL_EVENTS }) do
            for i = 1, #list do
                env[list[i]] = nil
            end
        end
        assert.is_true(Compat.check(env).ok)
        assert.same({}, Compat.check(env).absentOptional)
    end)
end)
