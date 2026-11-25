-- Specs for Core/settings.lua: defaults, strict coercion both ways, presets
-- and the per-line setters (SPEC 7.7, section 10 "settings").

local Loader = require("spec.helpers.loader")

--- The module under test. Declared before the helpers that read LINE_KEYS.
---@type AmdbSettingsModule
local Settings

---@return AmdbSettingsModule Settings
local function fresh()
    local A = Loader.namespace()
    Loader.load(A, "Core/settings.lua")
    return A.Core.Settings
end

---@param value any
---@return any a deep copy, to detect mutation of the input
local function deepCopy(value)
    if type(value) ~= "table" then
        return value
    end
    local copy = {}
    for key, inner in pairs(value) do
        copy[key] = deepCopy(inner)
    end
    return copy
end

---@param lines table
---@return boolean[] the state of every line key, in LINE_KEYS order
local function lineStates(lines)
    local states = {}
    for i, key in ipairs(Settings.LINE_KEYS) do
        states[i] = lines[key]
    end
    return states
end

--- One `it` per row; each row is captured for its own test (a Lua 5.1 loop
--- variable is shared by every closure made in the loop).
local function each(name, rows, fn)
    for i = 1, #rows do
        local row = rows[i]
        it(name .. " (" .. tostring(row[1]) .. ")", function()
            fn(row)
        end)
    end
end

Settings = fresh()

--- Calls a module function with deliberately wrong-typed arguments: the module
--- must answer, never throw. Typed `any` so the type checker does not flag
--- exactly the wrong types the test means to pass.
---@param fn function
---@param ... any
---@return any
local function withJunk(fn, ...)
    return fn(...)
end

describe("Settings module", function()
    it("lists the optional lines and the presets", function()
        local module = fresh()
        assert.same({ "sold", "lastSale", "low", "sparkline", "craft", "deal", "markets" }, module.LINE_KEYS)
        assert.same({ "basic", "standard" }, module.PRESETS)
    end)

    it("registers into the namespace and returns its table", function()
        local A = Loader.namespace()
        local chunk = assert(loadfile(Loader.ROOT .. "/Core/settings.lua"))
        local module = chunk(Loader.ADDON, A)
        assert.equals(A.Core.Settings, module)
    end)
end)

describe("Settings.defaults", function()
    it("switches everything on", function()
        local settings = Settings.defaults()
        assert.is_true(settings.enabled)
        assert.is_true(settings.shiftDetail)
        assert.same({ true, true, true, true, true, true, true }, lineStates(settings.lines))
        local count = 0
        local known = {}
        for _, key in ipairs(Settings.LINE_KEYS) do
            known[key] = true
        end
        for key, value in pairs(settings.lines) do
            count = count + 1
            assert.is_true(value)
            assert.is_true(known[key] == true)
        end
        assert.equals(#Settings.LINE_KEYS, count)
    end)

    it("answers a fresh table on every call", function()
        local first = Settings.defaults()
        local second = Settings.defaults()
        assert.not_equals(first, second)
        assert.not_equals(first.lines, second.lines)
        first.enabled = false
        first.lines.sold = false
        assert.is_true(second.enabled)
        assert.is_true(second.lines.sold)
        assert.is_true(Settings.defaults().lines.sold)
    end)
end)

describe("Settings.coerce", function()
    it("answers the defaults for nil and for anything that is not a table", function()
        assert.same(Settings.defaults(), Settings.coerce(nil))
        assert.same(Settings.defaults(), Settings.coerce(true))
        assert.same(Settings.defaults(), Settings.coerce("enabled"))
        assert.same(Settings.defaults(), Settings.coerce(7))
    end)

    it("keeps the literal true and false for the switches", function()
        local settings = Settings.coerce({ enabled = false, shiftDetail = false })
        assert.is_false(settings.enabled)
        assert.is_false(settings.shiftDetail)
        settings = Settings.coerce({ enabled = true, shiftDetail = true })
        assert.is_true(settings.enabled)
        assert.is_true(settings.shiftDetail)
    end)

    it("keeps the literal true and false for every optional line", function()
        local saved = { lines = {} }
        local expected = {}
        for i, key in ipairs(Settings.LINE_KEYS) do
            saved.lines[key] = i % 2 == 0
            expected[i] = i % 2 == 0
        end
        local settings = Settings.coerce(saved)
        assert.same(expected, lineStates(settings.lines))
        settings = Settings.coerce({ lines = { sold = true, deal = true } })
        assert.is_true(settings.lines.sold)
        assert.is_true(settings.lines.deal)
    end)

    each("falls back to the default for", { { 1 }, { 0 }, { "true" }, { "false" }, { "" }, { {} } }, function(row)
        local settings =
            Settings.coerce({ enabled = row[1], shiftDetail = row[1], lines = { sold = row[1], low = row[1] } })
        assert.is_true(settings.enabled)
        assert.is_true(settings.shiftDetail)
        assert.is_true(settings.lines.sold)
        assert.is_true(settings.lines.low)
    end)

    it("falls back to the default for a line key that is missing", function()
        local settings = Settings.coerce({ lines = { sold = false } })
        assert.is_false(settings.lines.sold)
        assert.same({ false, true, true, true, true, true, true }, lineStates(settings.lines))
    end)

    it("ignores line keys that name no line", function()
        local settings = Settings.coerce({ lines = { junk = false, ["1"] = false, sold = false } })
        assert.is_nil(rawget(settings.lines, "junk"))
        assert.is_nil(rawget(settings.lines, "1"))
        assert.is_false(settings.lines.sold)
        local count = 0
        for _ in pairs(settings.lines) do
            count = count + 1
        end
        assert.equals(#Settings.LINE_KEYS, count)
    end)

    it("answers the defaults when lines is not a table", function()
        local settings = Settings.coerce({ lines = "sold", enabled = false })
        assert.is_false(settings.enabled)
        assert.same({ true, true, true, true, true, true, true }, lineStates(settings.lines))
    end)

    it("never mutates its input", function()
        local saved = {
            enabled = 1,
            shiftDetail = false,
            lines = { sold = false, low = "x", junk = 42 },
            extra = { nested = true },
        }
        local before = deepCopy(saved)
        local settings = Settings.coerce(saved)
        assert.same(before, saved)
        assert.is_false(settings.shiftDetail)
        assert.is_false(settings.lines.sold)
        assert.is_true(settings.lines.low)
        assert.not_equals(saved, settings)
        assert.not_equals(saved.lines, settings.lines)
    end)

    it("always answers every line key as a boolean, without consulting a metatable on the saved table", function()
        local saved = {
            lines = setmetatable({}, {
                __index = function()
                    return "not a boolean"
                end,
            }),
        }
        local settings = Settings.coerce(saved)
        assert.same({ true, true, true, true, true, true, true }, lineStates(settings.lines))
    end)
end)

describe("Settings.coerceCharacter", function()
    it("keeps a non-empty primary market", function()
        assert.same(
            { primaryMarket = "forever-eu-pvp-alliance" },
            Settings.coerceCharacter({ primaryMarket = "forever-eu-pvp-alliance" })
        )
    end)

    each("drops primaryMarket", { { nil }, { {} }, { "" }, { 0 }, { 42 }, { true } }, function(row)
        local settings = Settings.coerceCharacter({ primaryMarket = row[1] })
        assert.is_nil(settings.primaryMarket)
    end)

    it("answers nil for a missing key and for anything that is not a table", function()
        assert.is_nil(Settings.coerceCharacter({}).primaryMarket)
        assert.is_nil(Settings.coerceCharacter(nil).primaryMarket)
        assert.is_nil(Settings.coerceCharacter("x").primaryMarket)
    end)

    it("keeps nothing else of the saved table", function()
        local settings = Settings.coerceCharacter({ primaryMarket = "beta", enabled = true, junk = 1 })
        local count = 0
        for key in pairs(settings) do
            count = count + 1
            assert.equals("primaryMarket", key)
        end
        assert.equals(1, count)
    end)
end)

describe("Settings.isLineOn", function()
    it("reads every line as on by default, and off after setLine(false)", function()
        local settings = Settings.defaults()
        for _, key in ipairs(Settings.LINE_KEYS) do
            assert.is_true(Settings.isLineOn(settings, key))
            assert.is_true(Settings.setLine(settings, key, false))
            assert.is_false(Settings.isLineOn(settings, key))
            assert.is_true(Settings.setLine(settings, key, true))
            assert.is_true(Settings.isLineOn(settings, key))
        end
    end)

    it("reads an absent line key as on", function()
        local settings = Settings.defaults()
        settings.lines.low = nil
        assert.is_true(Settings.isLineOn(settings, "low"))
        -- A settings table without lines at all: every optional line is on.
        rawset(settings, "lines", {})
        assert.is_true(Settings.isLineOn(settings, "sold"))
        rawset(settings, "lines", nil)
        assert.is_true(Settings.isLineOn(settings, "sold"))
    end)

    it("answers false for a key that names no line", function()
        local settings = Settings.defaults()
        assert.is_false(Settings.isLineOn(settings, "junk"))
        assert.is_false(Settings.isLineOn(settings, ""))
        assert.is_false(Settings.isLineOn(settings, "sold "))
        assert.is_false(withJunk(Settings.isLineOn, settings, nil))
    end)
end)

describe("Settings.setLine", function()
    it("switches every optional line on and off", function()
        local settings = Settings.defaults()
        for _, key in ipairs(Settings.LINE_KEYS) do
            assert.is_true(Settings.setLine(settings, key, false))
            assert.is_false(settings.lines[key])
            assert.is_true(Settings.setLine(settings, key, true))
            assert.is_true(settings.lines[key])
        end
    end)

    it("creates the lines table when a settings table has none", function()
        local settings = { enabled = true, shiftDetail = true }
        assert.is_true(Settings.setLine(settings, "sold", false))
        assert.is_false(settings.lines.sold)
        assert.is_false(Settings.isLineOn(settings, "sold"))
    end)

    it("counts only the literal true as on", function()
        local settings = Settings.defaults()
        withJunk(Settings.setLine, settings, "sold", 1)
        assert.is_false(settings.lines.sold)
        withJunk(Settings.setLine, settings, "sold", nil)
        assert.is_false(settings.lines.sold)
    end)

    it("refuses a key that names no line", function()
        local settings = Settings.defaults()
        assert.is_false(Settings.setLine(settings, "junk", false))
        assert.is_false(Settings.setLine(settings, "sold ", false))
        assert.is_nil(rawget(settings.lines, "junk"))
        assert.is_true(settings.lines.sold)
    end)
end)

describe("Settings.setShiftDetail and setEnabled", function()
    it("switch both ways, literal values only", function()
        local settings = Settings.defaults()
        assert.is_true(Settings.setEnabled(settings, false))
        assert.is_false(settings.enabled)
        assert.is_true(Settings.setShiftDetail(settings, false))
        assert.is_false(settings.shiftDetail)
        assert.is_true(Settings.setEnabled(settings, true))
        assert.is_true(settings.enabled)
        assert.is_true(Settings.setShiftDetail(settings, true))
        assert.is_true(settings.shiftDetail)
        withJunk(Settings.setEnabled, settings, "true")
        assert.is_false(settings.enabled)
        withJunk(Settings.setShiftDetail, settings, 1)
        assert.is_false(settings.shiftDetail)
    end)

    it("leave the lines alone", function()
        local settings = Settings.defaults()
        Settings.setEnabled(settings, false)
        Settings.setShiftDetail(settings, false)
        assert.same({ true, true, true, true, true, true, true }, lineStates(settings.lines))
    end)
end)

describe("Settings.applyPreset", function()
    it("basic switches every optional line off", function()
        local settings = Settings.defaults()
        settings.enabled = false
        assert.is_true(Settings.applyPreset(settings, "basic"))
        assert.same({ false, false, false, false, false, false, false }, lineStates(settings.lines))
        -- The preset touches lines only.
        assert.is_false(settings.enabled)
        assert.is_true(settings.shiftDetail)
    end)

    it("standard switches every optional line on", function()
        local settings = Settings.defaults()
        Settings.applyPreset(settings, "basic")
        assert.is_true(Settings.applyPreset(settings, "standard"))
        assert.same({ true, true, true, true, true, true, true }, lineStates(settings.lines))
    end)

    it("answers false for an unknown preset and changes nothing", function()
        local settings = Settings.defaults()
        Settings.setLine(settings, "sold", false)
        local before = deepCopy(settings)
        assert.is_false(Settings.applyPreset(settings, "Basic"))
        assert.is_false(Settings.applyPreset(settings, "off"))
        assert.is_false(Settings.applyPreset(settings, ""))
        assert.is_false(withJunk(Settings.applyPreset, settings, nil))
        assert.same(before, settings)
    end)

    it("answers false when settings is not a table", function()
        assert.is_false(withJunk(Settings.applyPreset, nil, "basic"))
        assert.is_false(withJunk(Settings.applyPreset, "x", "standard"))
        assert.is_false(withJunk(Settings.setEnabled, nil, true))
        assert.is_false(withJunk(Settings.setShiftDetail, nil, true))
        assert.is_false(withJunk(Settings.setLine, nil, "sold", false))
    end)

    it("covers every name in PRESETS", function()
        for _, name in ipairs(Settings.PRESETS) do
            assert.is_true(Settings.applyPreset(Settings.defaults(), name))
        end
    end)
end)
