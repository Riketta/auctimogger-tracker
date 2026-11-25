-- Specs for Core/panel_rows.lua: the row model of the options panel (SPEC 7.8).
--
-- The real settings module runs behind it. Every optional line and every
-- preset of that module must have its row, so the panel cannot drift from the
-- /amdb command. Pure functions over a context: nothing reads a game global or
-- a clock.

local Loader = require("spec.helpers.loader")

local TARGET = "forever-eu-pvp-alliance"
local OTHER = "forever-eu-pvp-horde"

-- The rows in display order: the master switch, the presets, the lines under
-- their title (seven today, then Shift detail), the market under its own, and
-- the status line.
local KINDS = {
    "check",
    "buttons",
    "header",
    "check",
    "check",
    "check",
    "check",
    "check",
    "check",
    "check",
    "check",
    "header",
    "market",
    "status",
}

--- The context the rows are built against: two loaded markets, the first of
--- them primary, a status line, and `overrides` on top. A key of `cleared` is
--- set to nil, which a table literal cannot say.
---@param A table namespace
---@param overrides table|nil
---@param cleared table<string, boolean>|nil
---@return table context
local function context(A, overrides, cleared)
    local merged = {
        settings = A.Core.Settings.defaults(),
        markets = { { target = TARGET }, { target = OTHER } },
        primary = TARGET,
        status = "STATUS LINE",
    }
    for key, value in pairs(overrides or {}) do
        merged[key] = value
    end
    for key in pairs(cleared or {}) do
        merged[key] = nil
    end
    return merged
end

--- The rows of `kind`, in display order.
---@param rows table[]
---@param kind string
---@return table[] rows
local function ofKind(rows, kind)
    local picked = {}
    for index = 1, #rows do
        if rows[index].kind == kind then
            picked[#picked + 1] = rows[index]
        end
    end
    return picked
end

--- The `field` of every entry of `list`, in order.
---@param list table[]
---@param field string
---@return any[] values
local function pluck(list, field)
    local values = {}
    for index = 1, #list do
        values[index] = list[index][field]
    end
    return values
end

--- The checkbox rows of the optional lines: every check that is neither the
--- master switch nor Shift detail.
---@param rows table[]
---@return table[] lines
local function lineChecks(rows)
    local checks = ofKind(rows, "check")
    local lines = {}
    for index = 1, #checks do
        local key = checks[index].key
        if key ~= "enabled" and key ~= "shiftDetail" then
            lines[#lines + 1] = checks[index]
        end
    end
    return lines
end

--- Whether `value` is in `list`.
---@param list any[]
---@param value any
---@return boolean present
local function holds(list, value)
    for index = 1, #list do
        if list[index] == value then
            return true
        end
    end
    return false
end

describe("the panel rows", function()
    local A
    local Rows

    before_each(function()
        A = Loader.load(Loader.namespace(), "Core/settings.lua", "Core/panel_rows.lua")
        Rows = A.Core.PanelRows
    end)

    it("registers into the namespace and returns its table", function()
        local namespace = Loader.load(Loader.namespace(), "Core/settings.lua")
        local chunk = assert(loadfile(Loader.ROOT .. "/Core/panel_rows.lua"))
        local module = chunk(Loader.ADDON, namespace)
        assert.equals(namespace.Core.PanelRows, module)
    end)

    it("lays the rows out in the order of SPEC 7.8", function()
        local rows = Rows.build(context(A))
        assert.same(KINDS, pluck(rows, "kind"))
    end)

    ---------------------------------------------------------------- the switch

    it("opens with the master switch, on unless it is literally switched off", function()
        local first = Rows.build(context(A))[1]
        assert.equals("check", first.kind)
        assert.equals("enabled", first.key)
        assert.equals("Enable tooltips", first.text)
        assert.is_true(first.on)
        assert.is_not_nil(first.help)
        -- The tooltip adapter's own reading: anything but a literal false is on.
        first = Rows.build(context(A, { settings = { enabled = "nope" } }))[1]
        assert.is_true(first.on)
        first = Rows.build(context(A, { settings = { enabled = false } }))[1]
        assert.is_false(first.on)
    end)

    ---------------------------------------------------------------- the presets

    it("gives every preset of the settings module a button, left to right", function()
        local row = Rows.build(context(A))[2]
        assert.equals("buttons", row.kind)
        assert.equals("Preset", row.text)
        local presets = A.Core.Settings.PRESETS
        assert.equals(#presets, #row.buttons)
        assert.same(presets, pluck(row.buttons, "preset"))
        assert.same({ "Basic", "Standard" }, pluck(row.buttons, "text"))
        for index = 1, #row.buttons do
            local button = row.buttons[index]
            assert.is_not_nil(button.help)
            -- A label of its own: a preset without one would print its key.
            assert.not_equals(button.preset, button.text)
        end
    end)

    ----------------------------------------------------------------- the lines

    it("gives every optional line of the settings module a checkbox, in its order and state", function()
        local settings = A.Core.Settings.defaults()
        local lineKeys = A.Core.Settings.LINE_KEYS
        for index = 1, #lineKeys do
            -- Even lines off, odd lines on: the model must follow each one.
            A.Core.Settings.setLine(settings, lineKeys[index], index % 2 == 1)
        end
        local lines = lineChecks(Rows.build(context(A, { settings = settings })))
        assert.equals(#lineKeys, #lines)
        assert.same(lineKeys, pluck(lines, "key"))
        for index = 1, #lines do
            local row = lines[index]
            assert.equals(A.Core.Settings.isLineOn(settings, row.key), row.on)
            assert.equals(index % 2 == 1, row.on)
        end
    end)

    it("names and explains every optional line, each in words of its own", function()
        local lines = lineChecks(Rows.build(context(A)))
        local labels = {}
        local help = {}
        for index = 1, #lines do
            local row = lines[index]
            -- A label of its own: a line without one would print its key.
            assert.not_equals(row.key, row.text)
            assert.is_nil(labels[row.text])
            assert.is_not_nil(row.help)
            assert.is_nil(help[row.help])
            labels[row.text] = index
            help[row.help] = index
        end
        -- The names of SPEC 7.6, as a player meets them in the tooltip.
        assert.is_true(holds(pluck(lines, "text"), "30-day low"))
        assert.is_true(holds(pluck(lines, "text"), "Crafting"))
    end)

    it("reads Shift detail as the tooltip does: off unless it is literally on", function()
        local function shift(settings)
            return Rows.build(context(A, { settings = settings }))[11]
        end
        local row = shift(A.Core.Settings.defaults())
        assert.equals("check", row.kind)
        assert.equals("shiftDetail", row.key)
        assert.equals("Hold Shift for detail", row.text)
        assert.is_true(row.on)
        assert.is_not_nil(row.help)
        assert.is_false(shift({ shiftDetail = "yes" }).on)
        assert.is_false(shift({}).on)
        assert.is_true(shift({ shiftDetail = true }).on)
    end)

    it("titles the two sections", function()
        local titles = pluck(ofKind(Rows.build(context(A)), "header"), "text")
        assert.same({ "Tooltip lines", "Market" }, titles)
    end)

    ---------------------------------------------------------------- the market

    it("offers the loaded markets in slot order, and names the primary one", function()
        local row = Rows.build(context(A, {
            markets = { { target = "b" }, { target = "a" }, { target = "c" } },
            primary = "a",
        }))[13]
        assert.equals("market", row.kind)
        assert.equals("Primary market", row.text)
        assert.same({ "b", "a", "c" }, row.choices)
        assert.equals("a", row.target)
        assert.is_not_nil(row.help)
    end)

    it("has nothing to offer without a loaded market, and skips what is not one", function()
        local row = Rows.build(context(A, { markets = { "junk", { slot = 2 } } }, { primary = true }))[13]
        assert.same({}, row.choices)
        assert.is_nil(row.target)
        row = Rows.build(context(A, nil, { markets = true, primary = true }))[13]
        assert.same({}, row.choices)
        assert.is_nil(row.target)
    end)

    ---------------------------------------------------------------- the status

    it("closes with the status line it was handed, and with an empty line for none", function()
        local rows = Rows.build(context(A))
        assert.equals("status", rows[#rows].kind)
        assert.equals("STATUS LINE", rows[#rows].text)
        rows = Rows.build(context(A, nil, { status = true }))
        assert.equals("", rows[#rows].text)
    end)

    -------------------------------------------------------------- the geometry

    it("sizes the frame as wide as the rows need, and as tall as the padding and every row", function()
        assert.equals(600, Rows.WIDTH)
        local rows = Rows.build(context(A))
        local sum = Rows.TOP + Rows.BOTTOM
        for index = 1, #rows do
            sum = sum + rows[index].height
        end
        assert.equals(sum, Rows.height(rows))
        -- The model as it is: the switch, the presets, seven lines, Shift
        -- detail and the picker at 26; two titles at 38; the status at 60.
        assert.equals(6 + 10 + 11 * 26 + 2 * 38 + 60, Rows.height(rows))
        assert.equals(438, Rows.height(rows))
        assert.equals(Rows.TOP + Rows.BOTTOM, Rows.height({}))
    end)

    it("builds a fresh model on every call", function()
        local first = Rows.build(context(A))
        first[1].on = false
        first[#first].text = "changed"
        local second = Rows.build(context(A))
        assert.is_true(second[1].on)
        assert.equals("STATUS LINE", second[#second].text)
        assert.not_equals(first[1], second[1])
    end)
end)
