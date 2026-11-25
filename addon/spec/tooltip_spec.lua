-- Specs for Adapters/tooltip.lua: the hook registration on both paths, the
-- lifecycle, Shift, secret values, the re-entrancy guard and the once-only
-- error report of SPEC 7.5 (section 10 "tooltip").
--
-- The real store, decoder, row builder and attached frame run together: the
-- golden carrier is loaded, the owners are frames that record their hooks,
-- CreateFrame hands out the attached frame, and C_Timer queues its callbacks
-- so a test decides when the next frame comes. The stubs go into _G with
-- rawset: a spec file's own globals are private to it.

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
    "Adapters/market_files.lua",
    "Adapters/extra_tip.lua",
    "Adapters/tooltip.lua",
}

local NOW = 1795560000
local GENERATED_AT = 1795559034
local GOLDEN = Loader.FIXTURES .. "/database_v4.lua"
local TARGET = "forever-eu-pvp-alliance"
local OTHER = "forever-eu-pvp-horde"

-- Item 25 of the golden carrier, and the single-variant species 2546.
local LINK = "|cff1eff00|Hitem:25::::::::120:::::|h[An Item]|h|r"

local DIM = "|cff666e80"
local MUTED = "|cff99a1b3"
local RESET = "|r"
local TITLE = "|cffff7d0aAuctiMogger Database|r"

-- What the golden carrier's record of item 25 renders on the Min buyout line.
local FLOOR_25 = "1g 50s" .. DIM .. "  ×1" .. RESET
-- The footer of item 25's detail level.
local FOOTER_25 = DIM .. "25 · item · feed v4" .. RESET

-- A value the secret stub refuses: the point is only that it is refused.
local SECRET = 999999

--- An owner tooltip that records its hooks.
---@param link any what GetItem answers as the link
---@return table owner
local function newOwner(link)
    local owner = { link = link, shown = false, collapsed = false, hooks = {} }
    function owner:HookScript(script, handler)
        self.hooks[script] = handler
    end
    function owner:IsShown()
        return self.shown
    end
    function owner:GetItem()
        return "An Item", self.link
    end
    function owner:IsTooltipCollapsed()
        return self.collapsed
    end
    function owner:GetLeft()
        return 400
    end
    function owner:GetRight()
        return 600
    end
    function owner:GetEffectiveScale()
        return 1
    end
    return owner
end

--- The attached frame, recording what is rendered into it. A setup method it
--- does not list is a no-op: the frame's creation calls are not its subject.
---@return table frame
local function newFrame()
    local frame = { lines = {}, shown = false, shownCount = 0 }
    function frame:NumLines()
        return #self.lines
    end
    function frame:ClearLines()
        self.lines = {}
    end
    function frame:AddLine(text)
        self.lines[#self.lines + 1] = { text }
    end
    function frame:AddDoubleLine(left, right)
        self.lines[#self.lines + 1] = { left, right }
    end
    function frame:Show()
        self.shown = true
        self.shownCount = self.shownCount + 1
    end
    function frame:Hide()
        self.shown = false
    end
    function frame:GetWidth()
        return 200
    end
    function frame:GetEffectiveScale()
        return 1
    end
    function frame:GetName()
        return "AuctiMoggerDatabaseTip"
    end
    return setmetatable(frame, {
        __index = function()
            return function() end
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

--- A second market with one item, worth `price`, loaded after the golden one.
---@param A table namespace
---@param target string
---@param id integer the item id
---@param price integer copper
local function addMarket(A, target, id, price)
    local market = A.BeginMarket({ version = 4, trackerVersion = "0.1.0", target = target, generatedAt = GENERATED_AT })
    market.items[id] = Encode.record({ spans = { d30 = block(price) } })
    A.EndMarket()
end

--- The right-hand text of the first row whose label is `left`.
---@param frame table
---@param left string
---@return string|nil right
local function rightOf(frame, left)
    for index = 1, #frame.lines do
        if frame.lines[index][1] == left then
            return frame.lines[index][2]
        end
    end
    return nil
end

--- The right-hand text of the one line another market adds.
---@param frame table
---@param target string the other market's target id
---@return string|nil right
local function otherLine(frame, target)
    return rightOf(frame, MUTED .. target .. RESET)
end

local STUBS = {
    "CreateFrame",
    "UIParent",
    "GameTooltip",
    "ItemRefTooltip",
    "TooltipDataProcessor",
    "Enum",
    "C_Timer",
    "IsShiftKeyDown",
    "issecretvalue",
}

describe("the tooltip adapter", function()
    local A
    local tooltip
    local frame
    local gameTooltip
    local itemRef
    local timers
    local said
    local shift
    local createdKind
    local saved

    --- Lets the next frame come: every queued callback runs.
    local function nextFrame()
        local pending = timers
        timers = {}
        for index = 1, #pending do
            pending[index]()
        end
    end

    --- Hovers an item on an owner: the owner is shown, the hook fires, a
    --- frame passes.
    ---@param id integer|nil the item id; 25 by default
    ---@param owner table|nil GameTooltip by default
    local function hover(id, owner)
        local target = owner or gameTooltip
        target.shown = true
        tooltip:onTooltip(target, { id = id or 25, type = "item" })
        nextFrame()
    end

    --- A TooltipDataProcessor that records its post-calls, and the Enum of
    --- tooltip data types with, or without, the battle-pet type.
    ---@param pets boolean whether Enum.TooltipDataType.BattlePet exists
    ---@return table calls the registered post-calls, in order
    local function processor(pets)
        local calls = {}
        rawset(_G, "TooltipDataProcessor", {
            AddTooltipPostCall = function(kind, callback)
                calls[#calls + 1] = { kind = kind, callback = callback }
            end,
        })
        local types = { Item = "item" }
        if pets then
            types.BattlePet = "battlepet"
        end
        rawset(_G, "Enum", { TooltipDataType = types })
        return calls
    end

    --- Refuses `value` as secret from now on.
    ---@param value any
    local function keepSecret(value)
        rawset(_G, "issecretvalue", function(candidate)
            return candidate == value
        end)
    end

    before_each(function()
        saved = {}
        for _, name in ipairs(STUBS) do
            saved[name] = rawget(_G, name)
        end
        timers = {}
        said = {}
        shift = false
        frame = newFrame()
        createdKind = nil
        rawset(_G, "CreateFrame", function(kind)
            createdKind = kind
            return frame
        end)
        rawset(_G, "UIParent", {})
        rawset(_G, "C_Timer", {
            After = function(_, callback)
                timers[#timers + 1] = callback
            end,
        })
        rawset(_G, "IsShiftKeyDown", function()
            return shift
        end)
        gameTooltip = newOwner(LINK)
        itemRef = newOwner(LINK)
        rawset(_G, "GameTooltip", gameTooltip)
        rawset(_G, "ItemRefTooltip", itemRef)

        A = Loader.load(Loader.namespace(), unpack(FILES))
        A.clock = function()
            return NOW
        end
        Loader.carrier(A, GOLDEN)
        tooltip = A.Tooltip.new({
            store = A.store,
            settings = A.Core.Settings.defaults(),
            say = function(text, level)
                said[#said + 1] = { text, level }
            end,
        })
    end)

    after_each(function()
        for _, name in ipairs(STUBS) do
            rawset(_G, name, saved[name])
        end
    end)

    ------------------------------------------------------------------ setup

    it("creates the attached frame when it is built, and hooks nothing before register", function()
        assert.equals("GameTooltip", createdKind)
        assert.equals("none", tooltip.path)
        assert.same({}, gameTooltip.hooks)
        assert.same({}, itemRef.hooks)
        assert.same({}, timers)
        assert.is_true(tooltip:isEnabled())
    end)

    it("registers into the namespace and returns its table", function()
        local namespace = Loader.load(Loader.namespace(), unpack(FILES, 1, #FILES - 1))
        local chunk = assert(loadfile(Loader.ROOT .. "/Adapters/tooltip.lua"))
        local module = chunk(Loader.ADDON, namespace)
        assert.equals(namespace.Tooltip, module)
    end)

    ---------------------------------------------------------- registration

    describe("registration", function()
        it("prefers the processor: a post-call for items, and one for battle pets where the type exists", function()
            local calls = processor(true)
            assert.equals("processor", tooltip:register())
            assert.equals(2, #calls)
            assert.equals("item", calls[1].kind)
            assert.equals("battlepet", calls[2].kind)
            -- Never both paths.
            assert.is_nil(gameTooltip.hooks.OnTooltipSetItem)
            assert.is_nil(itemRef.hooks.OnTooltipSetItem)
        end)

        it("registers no pet post-call where the client lacks the type", function()
            local calls = processor(false)
            assert.equals("processor", tooltip:register())
            assert.equals(1, #calls)
            assert.equals("item", calls[1].kind)
        end)

        it("falls back to OnTooltipSetItem on both frames without the processor", function()
            assert.equals("script", tooltip:register())
            assert.equals("function", type(gameTooltip.hooks.OnTooltipSetItem))
            assert.equals("function", type(itemRef.hooks.OnTooltipSetItem))
        end)

        it("falls back without the Enum of tooltip data types", function()
            rawset(_G, "TooltipDataProcessor", {
                AddTooltipPostCall = function()
                    error("must not be called", 0)
                end,
            })
            assert.equals("script", tooltip:register())
        end)

        it("falls back when the processor raises on registration", function()
            rawset(_G, "TooltipDataProcessor", {
                AddTooltipPostCall = function()
                    error("no tooltip processor here", 0)
                end,
            })
            rawset(_G, "Enum", { TooltipDataType = { Item = "item", BattlePet = "battlepet" } })
            assert.equals("script", tooltip:register())
            assert.equals("function", type(gameTooltip.hooks.OnTooltipSetItem))
            assert.same({}, said)
        end)

        it("hooks nothing, and raises nothing, without frames to hook", function()
            rawset(_G, "GameTooltip", nil)
            rawset(_G, "ItemRefTooltip", nil)
            assert.equals("none", tooltip:register())
        end)

        it("installs the owner's hide hooks on both paths", function()
            processor(true)
            assert.equals("processor", tooltip:register())
            assert.equals("function", type(gameTooltip.hooks.OnTooltipCleared))
            assert.equals("function", type(gameTooltip.hooks.OnHide))
            assert.equals("function", type(itemRef.hooks.OnTooltipCleared))
            assert.equals("function", type(itemRef.hooks.OnHide))
            assert.is_nil(gameTooltip.hooks.OnTooltipSetItem)
        end)

        it("installs the owner's hide hooks on the script path too", function()
            assert.equals("script", tooltip:register())
            assert.equals("function", type(gameTooltip.hooks.OnTooltipCleared))
            assert.equals("function", type(gameTooltip.hooks.OnHide))
            assert.equals("function", type(itemRef.hooks.OnTooltipCleared))
            assert.equals("function", type(itemRef.hooks.OnHide))
        end)

        it("is idempotent: a second register adds no hook", function()
            processor(true)
            assert.equals("processor", tooltip:register())
            local hooks = 0
            for _ in pairs(gameTooltip.hooks) do
                hooks = hooks + 1
            end
            assert.equals("processor", tooltip:register())
            for _ in pairs(gameTooltip.hooks) do
                hooks = hooks - 1
            end
            assert.equals(0, hooks)
        end)

        it("reads the link from GetItem on the script path, and shows the frame", function()
            assert.equals("script", tooltip:register())
            gameTooltip.shown = true
            gameTooltip.hooks.OnTooltipSetItem(gameTooltip)
            nextFrame()
            assert.is_true(frame.shown)
            assert.equals(FLOOR_25, rightOf(frame, "Min buyout"))
        end)

        it("shows nothing on the script path for an owner without a link", function()
            assert.equals("script", tooltip:register())
            gameTooltip.link = nil
            gameTooltip.shown = true
            gameTooltip.hooks.OnTooltipSetItem(gameTooltip)
            nextFrame()
            assert.is_false(frame.shown)
        end)
    end)

    -------------------------------------------------------------- lifecycle

    describe("the lifecycle", function()
        local callback
        local petCallback

        before_each(function()
            local calls = processor(true)
            assert.equals("processor", tooltip:register())
            callback = calls[1].callback
            petCallback = calls[2].callback
        end)

        it("shows the frame on the next frame, not at once", function()
            gameTooltip.shown = true
            callback(gameTooltip, { id = 25, type = "item" })
            assert.is_false(frame.shown)
            assert.equals(1, #timers)
            nextFrame()
            assert.is_true(frame.shown)
            assert.is_true(tooltip:isShown())
        end)

        it("renders the record of the hovered item", function()
            hover()
            assert.equals(TITLE, frame.lines[1][1])
            assert.equals(TARGET .. " · " .. MUTED .. "33m" .. RESET, frame.lines[1][2])
            assert.equals(FLOOR_25, rightOf(frame, "Min buyout"))
        end)

        it("reads the id from the processor payload", function()
            -- Item 117 is listed near ten billion copper: five-byte varints.
            hover(117)
            assert.equals("999999g 99s 99c" .. DIM .. "  ×1" .. RESET, rightOf(frame, "Min buyout"))
        end)

        it("stays hidden when the owner is no longer shown by the next frame", function()
            gameTooltip.shown = true
            callback(gameTooltip, { id = 25, type = "item" })
            gameTooltip.shown = false
            nextFrame()
            assert.is_false(frame.shown)
            assert.is_false(tooltip:isShown())
        end)

        it("takes the frame down when the owner is cleared, and when it is hidden", function()
            hover()
            assert.is_true(frame.shown)
            gameTooltip.hooks.OnTooltipCleared(gameTooltip)
            assert.is_false(frame.shown)
            assert.is_false(tooltip:isShown())
            hover()
            gameTooltip.hooks.OnHide(gameTooltip)
            assert.is_false(frame.shown)
            -- The other owner's hooks reach the same frame.
            hover(25, itemRef)
            assert.is_true(frame.shown)
            itemRef.hooks.OnTooltipCleared(itemRef)
            assert.is_false(frame.shown)
        end)

        it("takes the frame down for a tooltip that shows something without a record", function()
            hover()
            hover(123456)
            assert.is_false(frame.shown)
            assert.is_false(tooltip:isShown())
        end)

        it("takes the frame down for an owner that no longer names an item", function()
            hover()
            gameTooltip.link = nil
            gameTooltip.shown = true
            tooltip:onTooltip(gameTooltip, nil)
            nextFrame()
            assert.is_false(frame.shown)
        end)

        it("leaves comparison tooltips and every other frame alone", function()
            hover(25, newOwner(LINK))
            assert.is_false(frame.shown)
            assert.equals(0, #timers)
        end)

        it("reads the item record before the commodity one for an id in both tables", function()
            -- 194784 is an item with spans only, and a commodity with everything.
            hover(194784)
            assert.equals(DIM .. "not listed" .. RESET, rightOf(frame, "Min buyout"))
        end)

        it("resolves a battle pet from the pet post-call", function()
            gameTooltip.shown = true
            petCallback(gameTooltip, { id = 2546, type = "battlepet" })
            nextFrame()
            assert.is_true(frame.shown)
            assert.equals("250g" .. DIM .. "  ×1" .. RESET, rightOf(frame, "Min buyout"))
        end)

        it("shows nothing while the master switch is off", function()
            tooltip:setEnabled(false)
            assert.is_false(tooltip:isEnabled())
            hover()
            assert.is_false(frame.shown)
            assert.equals(0, #timers)
        end)

        it("takes a shown frame down when the switch goes off", function()
            hover()
            assert.is_true(frame.shown)
            tooltip:setEnabled(false)
            assert.is_false(frame.shown)
            assert.is_false(tooltip:isShown())
        end)

        it("voids a scheduled display when the switch goes off before the next frame", function()
            gameTooltip.shown = true
            callback(gameTooltip, { id = 25, type = "item" })
            tooltip:setEnabled(false)
            nextFrame()
            assert.is_false(frame.shown)
        end)

        it("adds one line per other market that has the item, against the primary", function()
            addMarket(A, OTHER, 25, 5000)
            hover()
            -- The primary is worth 70s 30c there (its listing value), the other
            -- market 50s: 29% cheaper.
            assert.equals("50s  -29%", otherLine(frame, OTHER))
        end)

        it("adds no line for a market that lacks the item", function()
            addMarket(A, OTHER, 117, 5000)
            hover()
            assert.is_nil(otherLine(frame, OTHER))
        end)

        it("displays at once where the client has no timer", function()
            rawset(_G, "C_Timer", nil)
            gameTooltip.shown = true
            callback(gameTooltip, { id = 25, type = "item" })
            assert.is_true(frame.shown)
        end)
    end)

    ------------------------------------------------------------------ secrets

    describe("secret values", function()
        local callback

        before_each(function()
            local calls = processor(true)
            assert.equals("processor", tooltip:register())
            callback = calls[1].callback
        end)

        it("does not use a secret payload id: the owner's link stands in", function()
            keepSecret(SECRET)
            gameTooltip.shown = true
            callback(gameTooltip, { id = SECRET, type = "item" })
            nextFrame()
            -- The link names item 25.
            assert.equals(FLOOR_25, rightOf(frame, "Min buyout"))
        end)

        it("treats a secret link as no link", function()
            keepSecret(SECRET)
            hover()
            gameTooltip.link = SECRET
            gameTooltip.shown = true
            tooltip:onTooltip(gameTooltip, nil)
            nextFrame()
            assert.is_false(frame.shown)
        end)

        it("stays hidden when IsShown answers a secret", function()
            keepSecret("yes")
            gameTooltip.IsShown = function()
                return "yes"
            end
            gameTooltip.shown = true
            callback(gameTooltip, { id = 25, type = "item" })
            nextFrame()
            assert.is_false(frame.shown)
        end)
    end)

    ---------------------------------------------------------------------- Shift

    describe("Shift", function()
        before_each(function()
            processor(true)
            assert.equals("processor", tooltip:register())
        end)

        it("builds the detail level while Shift is held", function()
            shift = true
            hover()
            -- The footer ends the detail level; the standard level has none.
            assert.equals(FOOTER_25, frame.lines[#frame.lines][1])
        end)

        it("rebuilds a shown frame at the detail level when Shift is pressed, and back when released", function()
            hover()
            assert.not_equals(FOOTER_25, frame.lines[#frame.lines][1])
            assert.equals(1, frame.shownCount)
            shift = true
            tooltip:onModifier("LSHIFT")
            -- On the next frame, like any display.
            assert.equals(1, frame.shownCount)
            nextFrame()
            assert.equals(2, frame.shownCount)
            assert.equals(FOOTER_25, frame.lines[#frame.lines][1])
            shift = false
            tooltip:onModifier("RSHIFT")
            nextFrame()
            assert.equals(3, frame.shownCount)
            assert.not_equals(FOOTER_25, frame.lines[#frame.lines][1])
            -- The owner was never asked to refresh: it has no way to be, and
            -- nothing was reported.
            assert.same({}, said)
        end)

        it("does nothing while the detail level would not change", function()
            hover()
            tooltip:onModifier("LSHIFT")
            assert.equals(0, #timers)
            assert.equals(1, frame.shownCount)
        end)

        it("does nothing while the frame is hidden, or for another modifier", function()
            hover()
            gameTooltip.hooks.OnHide(gameTooltip)
            shift = true
            tooltip:onModifier("LSHIFT")
            assert.equals(0, #timers)
            hover()
            shift = true
            tooltip:onModifier("LCTRL")
            assert.equals(0, #timers)
        end)

        it("does nothing when the settings turn the detail level off", function()
            assert.is_true(A.Core.Settings.setShiftDetail(tooltip.settings, false))
            hover()
            shift = true
            tooltip:onModifier("LSHIFT")
            assert.equals(0, #timers)
        end)

        it("keeps a collapsed owner at the basics, Shift or not", function()
            gameTooltip.collapsed = true
            shift = true
            hover()
            assert.equals(4, #frame.lines)
        end)

        it("reads Shift again when the owner is hovered, not only on the event", function()
            hover()
            shift = true
            hover()
            assert.equals(FOOTER_25, frame.lines[#frame.lines][1])
        end)
    end)

    ------------------------------------------------------------ errors, re-entry

    describe("errors and re-entrancy", function()
        it("reports a render that raised once, in bad, and takes the frame down", function()
            -- The frame is already up when the render fails.
            frame.Show = function(self)
                self.shown = true
                error("the frame broke", 0)
            end
            hover()
            assert.is_false(frame.shown)
            assert.is_false(tooltip:isShown())
            hover()
            assert.same({ { "tooltip error (reported once): the frame broke", "bad" } }, said)
            assert.is_false(frame.shown)
        end)

        it("drops a display that is re-entered while a render runs", function()
            -- Synchronous timers: the re-entrant request would display at
            -- once, inside the render, and recurse without the guard.
            rawset(_G, "C_Timer", {
                After = function(_, queued)
                    queued()
                end,
            })
            frame.Show = function(self)
                self.shownCount = self.shownCount + 1
                tooltip:onTooltip(gameTooltip, { id = 117, type = "item" })
            end
            hover()
            -- Rendered once: the display the re-entrant call asked for was
            -- dropped, and nothing was reported.
            assert.equals(1, frame.shownCount)
            assert.same({}, said)
        end)

        it("reports a row builder that raised, and goes on serving", function()
            local lines = A.Core.TooltipLines
            local build = lines.build
            lines.build = function()
                error("rows failed", 0)
            end
            hover()
            assert.same({ { "tooltip error (reported once): rows failed", "bad" } }, said)
            lines.build = build
            hover()
            assert.is_true(frame.shown)
            assert.equals(1, #said)
        end)
    end)
end)
