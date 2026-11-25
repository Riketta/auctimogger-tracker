-- Specs for Adapters/extra_tip.lua: the attached frame of SPEC 7.4, how it is
-- created, filled, placed (SPEC 7.5.3) and taken down, and its sparkline.
--
-- CreateFrame is stubbed to hand out a GameTooltip that records what is done
-- to it. The stubs go into _G with rawset: a spec file's own globals are
-- private to it, and the addon is loaded with loadfile.

local Loader = require("spec.helpers.loader")

local A = Loader.load(Loader.namespace(), "Core/anchor.lua", "Adapters/extra_tip.lua")
local ExtraTip = A.ExtraTip

local NAME = "AuctiMoggerDatabaseTip"

--- A texture that records what is done to it.
---@return table texture
local function newTexture()
    local texture = { points = {}, shown = false }
    function texture:SetColorTexture(r, g, b, a)
        self.color = { r, g, b, a }
    end
    function texture:ClearAllPoints()
        self.points = {}
    end
    function texture:SetPoint(...)
        self.points[#self.points + 1] = { ... }
    end
    function texture:SetWidth(width)
        self.width = width
    end
    function texture:SetHeight(height)
        self.height = height
    end
    function texture:Show()
        self.shown = true
    end
    function texture:Hide()
        self.shown = false
    end
    return texture
end

--- A GameTooltip that records what the addon does to it.
---@return table frame
local function newTipFrame()
    local frame = {
        lines = {},
        points = {},
        textures = {},
        calls = {},
        shown = false,
        width = 200,
        scale = 1,
    }
    function frame:SetFrameStrata(strata)
        self.strata = strata
    end
    function frame:SetClampedToScreen(clamped)
        self.clamped = clamped
    end
    function frame:SetOwner(owner, anchor)
        self.owner = owner
        self.anchor = anchor
    end
    function frame:ClearLines()
        self.lines = {}
        self.calls[#self.calls + 1] = "ClearLines"
    end
    function frame:ClearAllPoints()
        self.points = {}
    end
    function frame:SetPoint(...)
        self.points[#self.points + 1] = { ... }
    end
    function frame:AddLine(text)
        self.lines[#self.lines + 1] = { text }
    end
    function frame:AddDoubleLine(left, right)
        self.lines[#self.lines + 1] = { left, right }
    end
    function frame:NumLines()
        return #self.lines
    end
    function frame:Show()
        self.shown = true
        self.calls[#self.calls + 1] = "Show"
    end
    function frame:Hide()
        self.shown = false
        self.calls[#self.calls + 1] = "Hide"
    end
    function frame:GetWidth()
        return self.width
    end
    function frame:GetEffectiveScale()
        return self.scale
    end
    function frame:GetName()
        return NAME
    end
    function frame:CreateTexture()
        local texture = newTexture()
        self.textures[#self.textures + 1] = texture
        return texture
    end
    return frame
end

--- Something with layout: an owner tooltip, or UIParent, with its own scale.
---@param left any left edge, as a getter answers it
---@param right any right edge, as a getter answers it
---@param scale number effective scale
---@return table region
local function newRegion(left, right, scale)
    return {
        GetLeft = function()
            return left
        end,
        GetRight = function()
            return right
        end,
        GetEffectiveScale = function()
            return scale
        end,
    }
end

--- The rows of a small tooltip: a header, a line, a single line.
---@return AmdbTipRow[] rows
local function rows()
    return {
        { left = "AuctiMogger Database", right = "market · 25m" },
        { left = "Min buyout", right = "380g 50s" },
        { left = "194784 · commodity · feed v4", right = nil },
    }
end

--- The calls of one kind the frame saw, in order.
---@param kind string "Show" or "Hide"
---@return integer count
local function countCalls(frame, kind)
    local count = 0
    for index = 1, #frame.calls do
        if frame.calls[index] == kind then
            count = count + 1
        end
    end
    return count
end

--- How many of the frame's textures are shown.
---@param frame table
---@return integer count
local function shownBars(frame)
    local count = 0
    for index = 1, #frame.textures do
        if frame.textures[index].shown then
            count = count + 1
        end
    end
    return count
end

describe("ExtraTip", function()
    local frame
    local created
    local screen
    local owner
    local saved

    local STUBS = { "CreateFrame", "UIParent", "issecretvalue" }

    before_each(function()
        saved = {}
        for _, name in ipairs(STUBS) do
            saved[name] = rawget(_G, name)
        end
        created = {}
        rawset(_G, "CreateFrame", function(...)
            created[#created + 1] = { ... }
            frame = newTipFrame()
            return frame
        end)
        -- A 1920 px screen at scale 1.
        screen = newRegion(0, 1920, 1)
        rawset(_G, "UIParent", screen)
        -- An owner 400..600, so a 200 px frame fits on its right.
        owner = newRegion(400, 600, 1)
    end)

    after_each(function()
        for _, name in ipairs(STUBS) do
            rawset(_G, name, saved[name])
        end
    end)

    it("registers into the namespace and returns its table", function()
        local namespace = Loader.load(Loader.namespace(), "Core/anchor.lua")
        local chunk = assert(loadfile(Loader.ROOT .. "/Adapters/extra_tip.lua"))
        local module = chunk(Loader.ADDON, namespace)
        assert.equals(namespace.ExtraTip, module)
    end)

    it("creates one named GameTooltip from the template, owned by UIParent with no anchor", function()
        local extra = ExtraTip.new()
        assert.equals(1, #created)
        assert.same({ "GameTooltip", NAME, screen, "GameTooltipTemplate" }, created[1])
        assert.equals(extra.tip, frame)
        assert.equals("TOOLTIP", frame.strata)
        assert.is_true(frame.clamped)
        assert.equals(screen, frame.owner)
        assert.equals("ANCHOR_NONE", frame.anchor)
    end)

    it("starts hidden", function()
        local extra = ExtraTip.new()
        assert.is_false(extra:isShown())
        assert.is_false(frame.shown)
    end)

    it("renders a double line through AddDoubleLine and a single line through AddLine, in order", function()
        local extra = ExtraTip.new()
        extra:show(owner, rows())
        assert.same({
            { "AuctiMogger Database", "market · 25m" },
            { "Min buyout", "380g 50s" },
            { "194784 · commodity · feed v4" },
        }, frame.lines)
    end)

    it("shows the frame, and says so", function()
        local extra = ExtraTip.new()
        extra:show(owner, rows())
        assert.is_true(frame.shown)
        assert.is_true(extra:isShown())
    end)

    it("clears the previous lines before every fill", function()
        local extra = ExtraTip.new()
        extra:show(owner, rows())
        extra:show(owner, { { left = "only line", right = nil } })
        assert.same({ { "only line" } }, frame.lines)
        assert.equals(2, countCalls(frame, "ClearLines"))
    end)

    it("puts the frame's top-left at the owner's top-right, a gap and the offset apart", function()
        local extra = ExtraTip.new()
        extra:show(owner, rows())
        assert.same({ { "TOPLEFT", owner, "TOPRIGHT", 8, -10 } }, frame.points)
    end)

    it("stays right while the frame fits, and mirrors once it would cross the screen edge", function()
        local extra = ExtraTip.new()
        -- 1712 + 8 + 200 = 1920: the frame ends on the edge.
        local fitting = newRegion(400, 1712, 1)
        extra:show(fitting, rows())
        assert.same({ { "TOPLEFT", fitting, "TOPRIGHT", 8, -10 } }, frame.points)
        -- One pixel further and it crosses.
        local crossing = newRegion(400, 1713, 1)
        extra:show(crossing, rows())
        assert.same({ { "TOPRIGHT", crossing, "TOPLEFT", -8, -10 } }, frame.points)
    end)

    it("reads every edge in pixels, through its frame's effective scale", function()
        local extra = ExtraTip.new()
        -- In UI units the frame would cross the edge; in pixels, with the frame
        -- at a tenth of the owner's scale, it fits.
        frame.width = 2000
        frame.scale = 0.1
        rawset(_G, "UIParent", newRegion(0, 1920, 1))
        extra:show(newRegion(800, 1713, 0.5), rows())
        assert.equals("TOPLEFT", frame.points[1][1])
        -- And the other way round: in UI units it fits, in pixels it crosses.
        frame.width = 200
        frame.scale = 2
        extra:show(newRegion(400, 800, 2), rows())
        assert.equals("TOPRIGHT", frame.points[1][1])
    end)

    it("keeps the default side when an edge cannot be read", function()
        local extra = ExtraTip.new()
        -- No getter.
        extra:show({}, rows())
        assert.equals("TOPLEFT", frame.points[1][1])
        -- A getter that raises.
        local raising = newRegion(400, 1713, 1)
        raising.GetRight = function()
            error("no layout yet", 0)
        end
        extra:show(raising, rows())
        assert.equals("TOPLEFT", frame.points[1][1])
        -- A getter that answers something else than a number.
        extra:show(newRegion("400", "1713", 1), rows())
        assert.equals("TOPLEFT", frame.points[1][1])
        -- A zero scale would divide nothing: it is no scale.
        extra:show(newRegion(400, 1713, 0), rows())
        assert.equals("TOPLEFT", frame.points[1][1])
    end)

    it("never compares a secret layout value", function()
        rawset(_G, "issecretvalue", function(value)
            return value == 1713
        end)
        local extra = ExtraTip.new()
        extra:show(newRegion(400, 1713, 1), rows())
        -- The secret right edge is missing, so the frame keeps its default side.
        assert.equals("TOPLEFT", frame.points[1][1])
    end)

    it("hides a shown frame, and is harmless when it is already hidden", function()
        local extra = ExtraTip.new()
        extra:show(owner, rows())
        extra:hide()
        assert.is_false(frame.shown)
        assert.is_false(extra:isShown())
        extra:hide()
        assert.equals(3, countCalls(frame, "Hide"))
    end)

    it("hides before every re-show, so nothing of the previous tooltip lingers", function()
        local extra = ExtraTip.new()
        extra:show(owner, rows())
        assert.same({ "Hide", "ClearLines", "Show" }, frame.calls)
        extra:show(owner, rows())
        assert.same({ "Hide", "ClearLines", "Show", "Hide", "ClearLines", "Show" }, frame.calls)
    end)
end)

describe("the sparkline", function()
    local frame
    local caption
    local owner
    local saved

    local STUBS = { "CreateFrame", "UIParent", "AuctiMoggerDatabaseTipTextLeft2" }

    before_each(function()
        saved = {}
        for _, name in ipairs(STUBS) do
            saved[name] = rawget(_G, name)
        end
        rawset(_G, "CreateFrame", function()
            frame = newTipFrame()
            return frame
        end)
        rawset(_G, "UIParent", newRegion(0, 1920, 1))
        -- The second row's font string, as the client names it for a named
        -- GameTooltip.
        caption = {}
        rawset(_G, NAME .. "TextLeft2", caption)
        owner = newRegion(400, 600, 1)
    end)

    after_each(function()
        for _, name in ipairs(STUBS) do
            rawset(_G, name, saved[name])
        end
    end)

    --- The rows with a sparkline in the second row.
    ---@param bars integer[]
    ---@return AmdbTipRow[] rows
    local function withBars(bars)
        return {
            { left = "AuctiMogger Database", right = "market · 25m" },
            { left = "30 days", right = nil, bars = bars },
            { left = "Min buyout", right = "380g 50s" },
        }
    end

    it("adds the caption as a line of its own, in place", function()
        local extra = ExtraTip.new()
        extra:show(owner, withBars({ 100, 200 }))
        assert.same("30 days", frame.lines[2][1])
    end)

    it("draws one bar a day, in the muted color, standing on the row's own line", function()
        local extra = ExtraTip.new()
        extra:show(owner, withBars({ 100, 200 }))
        assert.equals(2, #frame.textures)
        assert.same({ 0.60, 0.63, 0.70, 1 }, frame.textures[1].color)
        assert.same({ { "BOTTOMLEFT", caption, "BOTTOMLEFT", 60, 0 } }, frame.textures[1].points)
        assert.same({ { "BOTTOMLEFT", caption, "BOTTOMLEFT", 64, 0 } }, frame.textures[2].points)
        assert.equals(3, frame.textures[1].width)
    end)

    it("scales a bar's height to its day's low: none at zero, half at the reference, full at twice it", function()
        local extra = ExtraTip.new()
        extra:show(owner, withBars({ 0, 128, 255 }))
        assert.equals(1, frame.textures[1].height)
        assert.equals(5, frame.textures[2].height)
        assert.equals(10, frame.textures[3].height)
    end)

    it("reuses its bars, and hides the ones a shorter series does not need", function()
        local extra = ExtraTip.new()
        extra:show(owner, withBars({ 10, 20, 30, 40, 50 }))
        assert.equals(5, #frame.textures)
        assert.equals(5, shownBars(frame))
        extra:show(owner, withBars({ 10, 20 }))
        assert.equals(5, #frame.textures)
        assert.equals(2, shownBars(frame))
        -- And back up again: the same five textures.
        extra:show(owner, withBars({ 10, 20, 30, 40, 50 }))
        assert.equals(5, #frame.textures)
        assert.equals(5, shownBars(frame))
    end)

    it("takes the bars down with the frame", function()
        local extra = ExtraTip.new()
        extra:show(owner, withBars({ 10, 20 }))
        assert.equals(2, shownBars(frame))
        extra:hide()
        assert.equals(0, shownBars(frame))
    end)

    it("draws no bar when the row's font string cannot be found", function()
        rawset(_G, NAME .. "TextLeft2", nil)
        local extra = ExtraTip.new()
        extra:show(owner, withBars({ 10, 20 }))
        assert.equals(0, #frame.textures)
        -- The caption line is still there.
        assert.same("30 days", frame.lines[2][1])
    end)

    it("draws at most thirty bars, the most recent thirty days", function()
        local extra = ExtraTip.new()
        local bars = {}
        for index = 1, 32 do
            bars[index] = index * 7
        end
        extra:show(owner, withBars(bars))
        assert.equals(30, #frame.textures)
        -- Day 3 of 32 is the first drawn: 21 of 255.
        assert.equals(1, frame.textures[1].height)
        assert.equals(32, #bars)
    end)
end)
