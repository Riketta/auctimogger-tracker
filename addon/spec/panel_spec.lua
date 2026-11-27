-- Specs for the options panel of Adapters/options.lua (SPEC 7.8): its
-- registration with the client's Settings API, the controls it builds from the
-- row model, what a click does, the refresh, the primary-market picker, and
-- what happens where the client cannot carry it.
--
-- The real store, settings module and row model run behind it, with the golden
-- carrier loaded. CreateFrame hands out regions that record what is set on
-- them; the Settings API, MenuUtil and the GameTooltip record what they are
-- asked. The stubs go into _G with rawset: a spec file's own globals are
-- private to it.

local Loader = require("spec.helpers.loader")
local Encode = require("spec.helpers.encode")

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
    "Adapters/options.lua",
}

local NOW = 1795560000
local GOLDEN = Loader.FIXTURES .. "/database_v4.lua"
local TARGET = "forever-eu-pvp-alliance"
local OTHER = "forever-eu-pvp-horde"

local CATEGORY = "AuctiMogger Database"

-- What the panel writes on its font strings, in creation order: the master
-- switch's label, the preset row, the lines under their title, the market
-- under its own. The status line comes after them.
local LABELS = {
    "Enable tooltips",
    "Preset",
    "Tooltip lines",
    "Live p05",
    "Sold per day and supply",
    "Last sale",
    "Last listed",
    "30-day low",
    "30-day chart",
    "Crafting",
    "Deal",
    "Other markets",
    "Hold Shift for detail",
    "Coin icons for prices",
    "Market",
    "Primary market",
}

local STUBS = { "CreateFrame", "GameTooltip", "UIParent", "SlashCmdList", "MenuUtil", "Settings", "GetMoneyString" }

--- A frame, a control or a font string: it records what is set on it, so a
--- test reads a control back. A `Set...` call lands under its own name:
--- `SetText` in `text`, `SetTextColor` in `textColor`.
---@param kind string the frame type CreateFrame was asked for
---@param template string|nil
---@param strings table[]|nil where a font string it creates is recorded
---@return table region
local function newRegion(kind, template, strings)
    local region = { kind = kind, template = template, scripts = {}, points = {}, shown = true, enabled = true }
    function region:SetScript(scriptType, handler)
        self.scripts[scriptType] = handler
    end
    function region:SetPoint(...)
        local args = { ... }
        self.points[#self.points + 1] = args
    end
    function region:Show()
        self.shown = true
    end
    function region:Hide()
        self.shown = false
    end
    function region:GetChecked()
        return self.checked
    end
    if strings ~= nil then
        function region:CreateFontString(_, layer, font)
            local text = newRegion("FontString", nil, strings)
            text.layer = layer
            text.font = font
            strings[#strings + 1] = text
            return text
        end
    end
    return setmetatable(region, {
        __index = function(self, method)
            -- rawget: `missing` is itself absent until a test sets it, and an
            -- indexed read of it would come straight back here.
            local missing = rawget(self, "missing")
            if missing and missing[method] then
                return nil
            end
            -- Only a `Set...` call is synthesized. Anything else is simply absent,
            -- so a test can tell an unset field from a set one, and a method the
            -- fake does not list fails loudly instead of passing silently.
            if type(method) ~= "string" or method:sub(1, 3) ~= "Set" then
                return nil
            end
            return function(owner, ...)
                local args = { ... }
                owner[method:sub(4, 4):lower() .. method:sub(5)] = #args > 1 and args or args[1]
            end
        end,
    })
end

--- One span block, worth `price` and nothing else.
---@param price integer copper
---@return table block
local function block(price)
    return {
        n = 1,
        min = price,
        p05 = price,
        p25 = price,
        median = price,
        p75 = price,
        p95 = price,
        max = price,
        mad = 0,
        vwap = price,
        qtySum = 1,
        listingCount = 1,
    }
end

--- A carrier of one market: item 25 worth `price`.
---@param target string
---@param generatedAt number epoch seconds
---@param price integer copper
---@return string text the carrier's Lua
local function carrier(target, generatedAt, price)
    return table.concat({
        "local _, A = ...",
        ("local m = A.BeginMarket({ version = 4, trackerVersion = '0.1.0', target = %s, generatedAt = %s })"):format(
            string.format("%q", target),
            tostring(generatedAt)
        ),
        "m.items[25] = '" .. Encode.record({ spans = { d30 = block(price) } }) .. "'",
        "A.EndMarket()",
    }, "\n")
end

--- Runs carrier text the way the client runs a slot file.
---@param A table namespace
---@param text string the carrier's Lua
local function runCarrier(A, text)
    local chunk = assert(loadstring(text, "=carrier"))
    chunk(Loader.ADDON, A)
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

describe("the options panel", function()
    local A
    local options
    local created
    local strings
    local registered
    local categories
    local menus
    local gameTooltip
    local tooltip
    local said
    local lines
    local persisted
    local statusCalls
    local now
    local savedGlobals

    --- Everything CreateFrame made so far of `kind`, in creation order.
    ---@param kind string
    ---@return table[] frames
    local function createdOf(kind)
        local picked = {}
        for index = 1, #created do
            if created[index].kind == kind then
                picked[#picked + 1] = created[index]
            end
        end
        return picked
    end

    --- The font string anchored to `control`: a checkbox's label.
    ---@param control table
    ---@return table label
    local function labelOf(control)
        for index = 1, #strings do
            local anchor = strings[index].points[1]
            if anchor ~= nil and anchor[2] == control then
                return strings[index]
            end
        end
        error("no label is anchored to this control")
    end

    --- The checkbox whose label reads `text`.
    --- The checkbox whose label reads `text`, the newest that is shown: a
    --- replaced row hides its old control and keeps its label.
    ---@param text string
    ---@return table checkbox
    local function checkboxOf(text)
        local boxes = createdOf("CheckButton")
        for index = #boxes, 1, -1 do
            if boxes[index].shown and labelOf(boxes[index]).text == text then
                return boxes[index]
            end
        end
        error("no checkbox is labelled " .. text)
    end

    --- The button whose text is `text`.
    --- The button whose text is `text`, the newest that is shown.
    ---@param text string
    ---@return table button
    local function buttonOf(text)
        local buttons = createdOf("Button")
        for index = #buttons, 1, -1 do
            if buttons[index].shown and buttons[index].text == text then
                return buttons[index]
            end
        end
        error("no button reads " .. text)
    end

    --- The primary-market picker: the third button, after the two presets.
    ---@return table picker
    local function pickerOf()
        return createdOf("Button")[3]
    end

    --- The status line's font string: the last one made.
    ---@return table status
    local function statusText()
        return strings[#strings]
    end

    --- Whether every optional line's checkbox is checked: all or none.
    ---@param on boolean
    ---@return boolean every
    local function linesChecked(on)
        local boxes = createdOf("CheckButton")
        -- The first box is the master switch; the last two are Shift detail and
        -- the coin icons.
        for index = 2, #boxes - 2 do
            if boxes[index].checked ~= on then
                return false
            end
        end
        return true
    end

    --- A Settings API that records the canvas and the category it is asked to
    --- register.
    local function settingsApi()
        rawset(_G, "Settings", {
            RegisterCanvasLayoutCategory = function(frame, name)
                local category = { name = name }
                local layout = { anchors = {} }
                function layout:AddAnchorPoint(point, x, y)
                    self.anchors[#self.anchors + 1] = { point, x, y }
                end
                registered[#registered + 1] = { frame = frame, name = name, layout = layout }
                return category, layout
            end,
            RegisterAddOnCategory = function(category)
                categories[#categories + 1] = category
            end,
        })
    end

    --- A MenuUtil that records the menus it is asked to open.
    local function menuUtil()
        menus = {}
        rawset(_G, "MenuUtil", {
            CreateContextMenu = function(owner, generator)
                menus[#menus + 1] = { owner = owner, generator = generator }
            end,
        })
    end

    --- Runs the generator of the menu that was opened last, against a root
    --- description that records what it is asked for.
    ---@return table root
    local function openMenu()
        local menu = menus[#menus]
        local root = { radios = {} }
        function root:CreateTitle(text)
            self.title = text
        end
        function root:CreateRadio(text, isSelected, setSelected)
            self.radios[#self.radios + 1] = { text = text, isSelected = isSelected, setSelected = setSelected }
        end
        menu.generator(nil, root)
        return root
    end

    --- The options over a namespace, the clock pinned to `now` and the status
    --- line counting its calls.
    ---@param namespace table
    ---@return table options
    local function build(namespace)
        namespace.clock = function()
            return now
        end
        namespace.SayNow = function(text, level)
            said[#said + 1] = { text = text, level = level }
        end
        return namespace.Options.new({
            store = namespace.store,
            settings = namespace.Core.Settings.defaults(),
            character = { primaryMarket = nil },
            tooltip = tooltip,
            say = function(text)
                lines[#lines + 1] = text
            end,
            persist = function()
                persisted = persisted + 1
            end,
            statusLine = function(store, at)
                statusCalls = statusCalls + 1
                return "STATUS " .. tostring(store:primaryTarget()) .. " @" .. at
            end,
        })
    end

    --- Registers the panel against the recording Settings API.
    local function register()
        settingsApi()
        assert.is_true(options:registerPanel())
    end

    before_each(function()
        now = NOW
        created = {}
        strings = {}
        registered = {}
        categories = {}
        menus = {}
        said = {}
        lines = {}
        persisted = 0
        statusCalls = 0

        -- The stubs this spec owns: saved for the restore, absent until a test
        -- installs them.
        savedGlobals = {}
        for _, name in ipairs(STUBS) do
            savedGlobals[name] = rawget(_G, name)
            rawset(_G, name, nil)
        end

        gameTooltip = { lines = {}, shown = false }
        function gameTooltip:SetOwner(owner, anchor)
            self.owner = owner
            self.anchor = anchor
        end
        function gameTooltip:AddLine(text, r, g, b)
            self.lines[#self.lines + 1] = { text, r, g, b }
        end
        function gameTooltip:Show()
            self.shown = true
        end
        function gameTooltip:Hide()
            self.shown = false
        end
        rawset(_G, "CreateFrame", function(kind, name, parent, template)
            local frame = newRegion(kind, template, strings)
            frame.name = name
            frame.parent = parent
            created[#created + 1] = frame
            return frame
        end)
        rawset(_G, "GameTooltip", gameTooltip)
        rawset(_G, "UIParent", {})
        rawset(_G, "SlashCmdList", {})
        tooltip = { path = "processor", switches = {} }
        function tooltip:setEnabled(on)
            self.switches[#self.switches + 1] = on
        end

        A = Loader.load(Loader.namespace(), unpack(FILES))
        -- Pinned before the carrier commits: the store stamps it with the clock.
        A.clock = function()
            return now
        end
        Loader.carrier(A, GOLDEN)
        options = build(A)
    end)

    after_each(function()
        for _, name in ipairs(STUBS) do
            rawset(_G, name, savedGlobals[name])
        end
    end)

    ------------------------------------------------------------ registration

    describe("registration", function()
        it(
            "registers the frame as a canvas category under the addon's title, anchored at its top-left corner",
            function()
                register()
                assert.equals(1, #registered)
                local canvas = registered[1]
                assert.equals(CATEGORY, canvas.name)
                assert.equals(options.panel.frame, canvas.frame)
                assert.same({ { "TOPLEFT", 16, -16 } }, canvas.layout.anchors)
                assert.same({ { name = CATEGORY } }, categories)
            end
        )

        it("hands the Settings UI a hidden frame, as wide as the model and as tall as its rows", function()
            register()
            local frame = options.panel.frame
            -- Ours until the Settings UI parents it: it must not stand on the screen.
            assert.is_false(frame.shown)
            assert.same({ 600, 0 }, frame.size)
            assert.equals(516, frame.height)
            assert.equals("Frame", frame.kind)
            assert.is_nil(frame.template)
            assert.equals(rawget(_G, "UIParent"), frame.parent)
        end)

        it(
            "carries one script only, its tick: nothing of ours runs when the Settings window opens or closes",
            function()
                register()
                local frame = options.panel.frame
                assert.same({ "OnUpdate" }, keysOf(frame.scripts))
                assert.equals("function", type(frame.scripts.OnUpdate))
                for _, name in ipairs({ "OnShow", "OnHide", "OnRefresh", "OnCommit", "OnDefault" }) do
                    assert.is_nil(rawget(frame, name))
                end
            end
        )

        it("registers once: a second call builds nothing and asks the client for nothing", function()
            register()
            local count = #created
            assert.is_true(options:registerPanel())
            assert.equals(count, #created)
            assert.equals(1, #registered)
            assert.equals(1, #categories)
        end)

        it("is left out, and nothing is built, where the client has no Settings API", function()
            assert.is_false(options:registerPanel())
            assert.same({}, created)
            assert.is_nil(options.panel)
        end)

        it("is left out too where the global cannot register a canvas", function()
            rawset(_G, "Settings", { RegisterCanvasLayoutCategory = function() end })
            assert.is_false(options:registerPanel())
            rawset(_G, "Settings", { RegisterAddOnCategory = function() end })
            assert.is_false(options:registerPanel())
            rawset(_G, "Settings", "junk")
            assert.is_false(options:registerPanel())
            assert.same({}, created)
            assert.is_nil(options.panel)
        end)

        it("raises, and registers nothing, where the client cannot create a control", function()
            settingsApi()
            rawset(_G, "CreateFrame", function(kind)
                if kind == "CheckButton" then
                    error("no checkboxes on this client", 0)
                end
                local frame = newRegion(kind, nil, strings)
                created[#created + 1] = frame
                return frame
            end)
            assert.has_error(function()
                options:registerPanel()
            end, "no checkboxes on this client")
            assert.is_nil(options.panel)
            assert.same({}, registered)
            assert.same({}, categories)
        end)
    end)

    ------------------------------------------------------------- the controls

    describe("the controls", function()
        it("builds one control per row of the model, on the templates the sniper runs on Forever", function()
            register()
            assert.same({
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
                "CheckButton",
                "Button",
            }, pluck(created, "kind"))
            assert.equals("UICheckButtonTemplate", createdOf("CheckButton")[1].template)
            assert.equals("UIPanelButtonTemplate", pickerOf().template)
            -- Every control hangs on the frame the Settings UI shows.
            for index = 2, #created do
                assert.equals(options.panel.frame, created[index].parent)
            end
        end)

        it("writes the model's labels on its font strings, row by row", function()
            register()
            local texts = {}
            for index = 1, #LABELS do
                texts[index] = strings[index].text
            end
            assert.same(LABELS, texts)
            assert.equals(#LABELS + 1, #strings)
            assert.equals("STATUS " .. TARGET .. " @" .. NOW, statusText().text)
        end)

        it("gives the titles the brand color of SPEC 7.5.4, and the labels the client's own", function()
            register()
            assert.same({ 1, 125 / 255, 10 / 255 }, strings[3].textColor)
            assert.same({ 1, 125 / 255, 10 / 255 }, strings[15].textColor)
            assert.is_nil(strings[1].textColor)
            assert.equals("GameFontHighlight", strings[1].font)
            assert.equals("GameFontNormal", statusText().font)
        end)

        it("wraps the status line, within its width, from its top", function()
            register()
            local status = statusText()
            assert.equals(560, status.width)
            assert.is_true(status.wordWrap)
            assert.equals("TOP", status.justifyV)
        end)

        it("shows the status line unwrapped where the client cannot wrap", function()
            settingsApi()
            rawset(_G, "CreateFrame", function(kind, _, _, template)
                local frame = newRegion(kind, template, nil)
                function frame:CreateFontString(_, layer, font)
                    local text = newRegion("FontString", nil, strings)
                    text.layer = layer
                    text.font = font
                    text.missing = { SetWordWrap = true }
                    strings[#strings + 1] = text
                    return text
                end
                created[#created + 1] = frame
                return frame
            end)
            assert.is_true(options:registerPanel())
            local status = statusText()
            assert.is_nil(status.wordWrap)
            assert.equals("STATUS " .. TARGET .. " @" .. NOW, status.text)
        end)

        it("lets a checkbox take its label's clicks, and no others", function()
            register()
            local box = checkboxOf("Deal")
            assert.same({ 0, -280, 0, 0 }, box.hitRectInsets)
            assert.equals("Deal", labelOf(box).text)
        end)

        it("names the primary market on the picker", function()
            register()
            assert.equals(TARGET, pickerOf().text)
            assert.is_true(pickerOf().enabled)
            assert.equals("Primary market", strings[16].text)
        end)

        it("reads the settings it is registered under", function()
            A.Core.Settings.setEnabled(options.settings, false)
            A.Core.Settings.setLine(options.settings, "deal", false)
            A.Core.Settings.setShiftDetail(options.settings, false)
            register()
            assert.is_false(checkboxOf("Enable tooltips").checked)
            assert.is_false(checkboxOf("Deal").checked)
            assert.is_true(checkboxOf("Crafting").checked)
            assert.is_true(checkboxOf("Live p05").checked)
            assert.is_false(checkboxOf("Hold Shift for detail").checked)
            -- Text is the default form of a price.
            assert.is_false(checkboxOf("Coin icons for prices").checked)
        end)
    end)

    ----------------------------------------------------------------- a click

    describe("a click", function()
        it("on a line's checkbox switches that line through the command's own path, and writes it back", function()
            register()
            local box = checkboxOf("Deal")
            box.scripts.OnClick(box)
            assert.is_false(options.settings.lines.deal)
            assert.is_false(box.checked)
            assert.equals(1, persisted)
            -- And back.
            box.scripts.OnClick(box)
            assert.is_true(options.settings.lines.deal)
            assert.is_true(box.checked)
            assert.equals(2, persisted)
        end)

        it("on the master switch reaches the tooltip adapter too", function()
            register()
            local box = checkboxOf("Enable tooltips")
            box.scripts.OnClick(box)
            assert.is_false(options.settings.enabled)
            assert.same({ false }, tooltip.switches)
            assert.equals(1, persisted)
        end)

        it("on Shift detail switches it", function()
            register()
            local box = checkboxOf("Hold Shift for detail")
            box.scripts.OnClick(box)
            assert.is_false(options.settings.shiftDetail)
            assert.equals(1, persisted)
        end)

        it("on the coin icons switches them, which needs the client's GetMoneyString", function()
            register()
            local box = checkboxOf("Coin icons for prices")
            -- Without the money string nothing changes, and the box comes back off.
            box.scripts.OnClick(box)
            assert.is_false(options.settings.coins)
            assert.is_false(box.checked)
            assert.equals(0, persisted)
            assert.equals("12s 50c", A.Core.Format.money(1250))
            rawset(_G, "GetMoneyString", function()
                return "coin"
            end)
            box.scripts.OnClick(box)
            assert.is_true(options.settings.coins)
            assert.is_true(box.checked)
            assert.equals(1, persisted)
            assert.equals("coin", A.Core.Format.money(1250))
            -- And back, through the same path.
            box.scripts.OnClick(box)
            assert.is_false(options.settings.coins)
            assert.is_false(box.checked)
            assert.equals("12s 50c", A.Core.Format.money(1250))
        end)

        it("on a preset button applies the preset, and the line checkboxes follow at once", function()
            register()
            local basic = buttonOf("Basic")
            basic.scripts.OnClick(basic)
            assert.is_true(linesChecked(false))
            assert.equals(1, persisted)
            local standard = buttonOf("Standard")
            standard.scripts.OnClick(standard)
            assert.is_true(linesChecked(true))
            assert.equals(2, persisted)
        end)

        it("says nothing in chat: the panel is its own answer", function()
            register()
            local box = checkboxOf("Deal")
            box.scripts.OnClick(box)
            local basic = buttonOf("Basic")
            basic.scripts.OnClick(basic)
            assert.same({}, lines)
        end)
    end)

    ------------------------------------------------------------- the refresh

    describe("the refresh", function()
        it("follows a change made through the command", function()
            register()
            options:run("basic")
            options:run("off")
            options:run("shift off")
            assert.is_true(linesChecked(false))
            assert.is_false(checkboxOf("Enable tooltips").checked)
            assert.is_false(checkboxOf("Hold Shift for detail").checked)
            options:run("deal on")
            assert.is_true(checkboxOf("Deal").checked)
            -- The master switch is no preset's to touch.
            options:run("standard")
            assert.is_true(linesChecked(true))
            assert.is_false(checkboxOf("Enable tooltips").checked)
        end)

        it("updates its controls in place: a model of the same shape builds nothing", function()
            runCarrier(A, carrier(OTHER, NOW - 600, 100))
            register()
            local count = #created
            local made = #strings
            local button = pickerOf()
            options:run("basic")
            options:run("market " .. OTHER)
            now = NOW + 1
            options.panel.frame.scripts.OnUpdate(options.panel.frame)
            assert.equals(count, #created)
            assert.equals(made, #strings)
            assert.equals(button, pickerOf())
            -- Nothing was taken down either: every control is still up.
            for index = 2, #created do
                assert.is_true(created[index].shown)
            end
        end)

        it("names the market the command chose, at once", function()
            runCarrier(A, carrier(OTHER, NOW - 600, 100))
            register()
            assert.equals(TARGET, pickerOf().text)
            options:run("market " .. OTHER)
            assert.equals(OTHER, pickerOf().text)
        end)

        it("refreshes on its tick at most once a second, and the status line follows the clock", function()
            register()
            local frame = options.panel.frame
            local status = statusText()
            assert.equals("STATUS " .. TARGET .. " @" .. NOW, status.text)
            local calls = statusCalls
            -- The same second: nothing is rebuilt.
            frame.scripts.OnUpdate(frame)
            assert.equals(calls, statusCalls)
            now = NOW + 60
            frame.scripts.OnUpdate(frame)
            assert.equals(calls + 1, statusCalls)
            assert.equals(status, statusText())
            assert.equals("STATUS " .. TARGET .. " @" .. (NOW + 60), status.text)
            -- Within that second, quiet again.
            frame.scripts.OnUpdate(frame)
            assert.equals(calls + 1, statusCalls)
        end)

        it("takes the old controls down when a row of another kind takes a row's place", function()
            register()
            local old = checkboxOf("Enable tooltips")
            local oldLabel = labelOf(old)
            ---@type any
            local rows = A.Core.PanelRows.build(options:panelContext())
            rows[1] = { kind = "status", height = 60, text = "in its place" }
            local realBuild = A.Core.PanelRows.build
            A.Core.PanelRows.build = function()
                return rows
            end
            assert.is_true(options:refreshPanel())
            assert.is_false(old.shown)
            assert.is_false(oldLabel.shown)
            -- The frame follows the rows: 26 pixels for 60.
            assert.equals(516 - 26 + 60, options.panel.frame.height)
            local standIn = statusText()
            A.Core.PanelRows.build = realBuild
            assert.is_true(options:refreshPanel())
            assert.is_true(checkboxOf("Enable tooltips").shown)
            assert.is_false(standIn.shown)
            assert.equals(516, options.panel.frame.height)
        end)

        it("takes the rows down that a shorter model no longer has", function()
            register()
            local status = statusText()
            ---@type any
            local rows = A.Core.PanelRows.build(options:panelContext())
            rows[#rows] = nil
            local realBuild = A.Core.PanelRows.build
            A.Core.PanelRows.build = function()
                return rows
            end
            assert.is_true(options:refreshPanel())
            assert.is_false(status.shown)
            assert.is_nil(options.panel.regions[#rows + 1])
            assert.equals(516 - 60, options.panel.frame.height)
            A.Core.PanelRows.build = realBuild
        end)
    end)

    ------------------------------------------------------ a refresh that fails

    describe("a refresh that fails", function()
        before_each(function()
            register()
        end)

        it("takes the panel out of service and reports it once", function()
            A.Core.PanelRows.build = function()
                error("model broke", 0)
            end
            assert.is_false(options:refreshPanel())
            assert.is_nil(options.panel)
            assert.same({ { text = "the options panel stopped refreshing: model broke", level = "bad" } }, said)
            -- Quiet from here on: a command still works, and says nothing of the panel.
            options:run("deal off")
            assert.is_false(options.settings.lines.deal)
            assert.equals(1, #said)
        end)

        it("stops its tick when it fails on the tick", function()
            local frame = options.panel.frame
            A.Core.PanelRows.build = function()
                error("model broke", 0)
            end
            now = NOW + 1
            frame.scripts.OnUpdate(frame)
            assert.is_nil(frame.scripts.OnUpdate)
            assert.is_nil(options.panel)
            assert.equals(1, #said)
        end)
    end)

    -------------------------------------------------- the primary-market picker

    describe("the primary-market picker", function()
        it("names the primary market, and offers the loaded ones in the client's own menu", function()
            menuUtil()
            register()
            local button = pickerOf()
            assert.equals(TARGET, button.text)
            button.scripts.OnClick(button)
            assert.equals(1, #menus)
            assert.equals(button, menus[1].owner)
            -- The menu reads the store at every opening: a market that loaded since.
            runCarrier(A, carrier(OTHER, NOW - 600, 100))
            button.scripts.OnClick(button)
            local root = openMenu()
            assert.equals("Primary market", root.title)
            assert.same({ TARGET, OTHER }, pluck(root.radios, "text"))
            assert.is_true(root.radios[1].isSelected())
            assert.is_false(root.radios[2].isSelected())
        end)

        it(
            "chooses through the command's own path: the store follows, it is written back, the picker follows",
            function()
                runCarrier(A, carrier(OTHER, NOW - 600, 100))
                menuUtil()
                register()
                local button = pickerOf()
                button.scripts.OnClick(button)
                openMenu().radios[2].setSelected()
                assert.equals(OTHER, A.store:primaryTarget())
                assert.equals(OTHER, options.character.primaryMarket)
                assert.equals(1, persisted)
                assert.equals(OTHER, pickerOf().text)
                -- Opened again, the menu marks the new choice.
                button.scripts.OnClick(button)
                local root = openMenu()
                assert.is_false(root.radios[1].isSelected())
                assert.is_true(root.radios[2].isSelected())
            end
        )

        it("moves to the next market on each click where the client has no menu", function()
            runCarrier(A, carrier(OTHER, NOW - 600, 100))
            register()
            local button = pickerOf()
            button.scripts.OnClick(button)
            assert.equals(OTHER, A.store:primaryTarget())
            assert.equals(OTHER, pickerOf().text)
            -- Round to the first.
            button.scripts.OnClick(button)
            assert.equals(TARGET, A.store:primaryTarget())
            assert.equals(2, persisted)
            assert.same({}, menus)
        end)

        it("moves to the next market too where the menu raises on opening", function()
            runCarrier(A, carrier(OTHER, NOW - 600, 100))
            rawset(_G, "MenuUtil", {
                CreateContextMenu = function()
                    error("this fork has no such menu", 0)
                end,
            })
            register()
            local button = pickerOf()
            button.scripts.OnClick(button)
            assert.equals(OTHER, A.store:primaryTarget())
            -- A menu that cannot open is no error to report: the picker moves on.
            assert.same({}, said)
        end)

        it("has nothing to pick without a loaded market", function()
            options = build(Loader.load(Loader.namespace(), unpack(FILES)))
            register()
            local button = pickerOf()
            assert.is_false(button.enabled)
            assert.equals("none loaded", button.text)
            button.scripts.OnClick(button)
            assert.equals(0, persisted)
            assert.is_nil(options.store:primaryTarget())
        end)
    end)

    ------------------------------------------------------------- the hover help

    describe("the hover help", function()
        it("shows a control's label and help in the client's tooltip, at the cursor", function()
            register()
            local box = checkboxOf("Deal")
            box.scripts.OnEnter(box)
            assert.equals(box, gameTooltip.owner)
            assert.equals("ANCHOR_CURSOR", gameTooltip.anchor)
            assert.same({
                { "Deal", 1, 125 / 255, 10 / 255 },
                { "The tracker's deal rank: how far under its value the item is listed.", 1, 1, 1 },
            }, gameTooltip.lines)
            assert.is_true(gameTooltip.shown)
            box.scripts.OnLeave(box)
            assert.is_false(gameTooltip.shown)
        end)

        it("answers for a preset button and the picker as well", function()
            register()
            local basic = buttonOf("Basic")
            basic.scripts.OnEnter(basic)
            assert.equals(
                "Only the three basic lines: min buyout, market value and sale rate.",
                gameTooltip.lines[2][1]
            )
            -- The tooltip is cleared between hovers, as the client clears it.
            gameTooltip.lines = {}
            gameTooltip.shown = false
            local button = pickerOf()
            button.scripts.OnEnter(button)
            assert.same({
                { "Primary market", 1, 125 / 255, 10 / 255 },
                {
                    "The market whose prices the tooltip shows. Other markets show as cross-market lines.",
                    1,
                    1,
                    1,
                },
            }, gameTooltip.lines)
            assert.equals(button, gameTooltip.owner)
            assert.is_true(gameTooltip.shown)
        end)

        it("is decoration: a client without a tooltip gets none and raises nothing", function()
            rawset(_G, "GameTooltip", nil)
            register()
            local box = checkboxOf("Deal")
            box.scripts.OnEnter(box)
            box.scripts.OnLeave(box)
            rawset(_G, "GameTooltip", { SetOwner = function() end })
            box.scripts.OnEnter(box)
            assert.same({}, said)
        end)
    end)
end)
