local _, A = ...

-- Adapters/extra_tip.lua - the attached frame.
--
-- One GameTooltip of our own (decision 6: the game's tooltip is never
-- modified), filled through the native AddLine / AddDoubleLine and placed
-- beside its owner (SPEC 7.5.3). It is created once and shown for one owner
-- at a time: every show starts by hiding, so nothing of the previous tooltip
-- lingers.
--
-- Taint: the frame is ours and unprotected, and every write is cosmetic, so
-- nothing is combat-gated. The owner is only read, and every layout read is
-- pcall'd and screened: a secret or unreadable number is simply missing, and
-- a missing number leaves the frame on its default side (Core/anchor.lua).

---@type AmdbAnchorModule
local Anchor = A.Core.Anchor

--- The slice of the client's GameTooltip this file calls. The client's frame
--- types are declared nowhere in this workspace, so the methods are listed.
---@class AmdbTipFrame
---@field SetFrameStrata fun(self: AmdbTipFrame, strata: string)
---@field SetClampedToScreen fun(self: AmdbTipFrame, clamped: boolean)
---@field SetOwner fun(self: AmdbTipFrame, owner: table, anchor: string)
---@field ClearLines fun(self: AmdbTipFrame)
---@field ClearAllPoints fun(self: AmdbTipFrame)
---@field SetPoint fun(self: AmdbTipFrame, point: string, relativeTo: table, relativePoint: string, offsetX: number, offsetY: number)
---@field AddLine fun(self: AmdbTipFrame, text: string, r: number, g: number, b: number)
---@field AddDoubleLine fun(self: AmdbTipFrame, left: string, right: string, lR: number, lG: number, lB: number, rR: number, rG: number, rB: number)
---@field Show fun(self: AmdbTipFrame)
---@field Hide fun(self: AmdbTipFrame)
---@field GetWidth fun(self: AmdbTipFrame): number
---@field GetEffectiveScale fun(self: AmdbTipFrame): number
---@field GetName fun(self: AmdbTipFrame): string
---@field NumLines fun(self: AmdbTipFrame): number
---@field CreateTexture fun(self: AmdbTipFrame, name: nil, layer: string, template: nil): AmdbTipTexture

--- The slice of a texture this file draws a sparkline bar with.
---@class AmdbTipTexture
---@field SetColorTexture fun(self: AmdbTipTexture, r: number, g: number, b: number, a: number)
---@field ClearAllPoints fun(self: AmdbTipTexture)
---@field SetPoint fun(self: AmdbTipTexture, point: string, relativeTo: table, relativePoint: string, offsetX: number, offsetY: number)
---@field SetWidth fun(self: AmdbTipTexture, width: number)
---@field SetHeight fun(self: AmdbTipTexture, height: number)
---@field Show fun(self: AmdbTipTexture)
---@field Hide fun(self: AmdbTipTexture)

--- The slice of an owner tooltip this file reads.
---@class AmdbTipOwner
---@field GetLeft fun(self: AmdbTipOwner): number
---@field GetRight fun(self: AmdbTipOwner): number
---@field GetEffectiveScale fun(self: AmdbTipOwner): number

--- One sparkline to draw: the row it belongs to, and its bars.
---@class AmdbTipSpark
---@field line integer the row's 1-based index in the frame
---@field bars integer[] one byte a day

-- The frame's name. A named GameTooltip names its font strings
-- `<name>TextLeft<n>`, which is how a sparkline finds its own row.
local FRAME_NAME = "AuctiMoggerDatabaseTip"

-- The sparkline (SPEC 7.6): one bar a day, standing on the bottom edge of its
-- row. A bar is as tall as its day's low is high against the reference.
local MAX_BARS = 30
local BAR_WIDTH = 3
local BAR_PITCH = 4
local BAR_HEIGHT = 10
-- Room left of the bars for the row's caption, so the bars begin at the same
-- place whatever the caption's width.
local BAR_INSET = 60

-- The bars are drawn in the house "muted" color of SPEC 7.5.4.
local BAR_COLOR = { 0.60, 0.63, 0.70 }

--- A layout number a frame's getter answers, or nil when the getter is
--- missing, raises, answers a secret value or answers anything but a finite
--- number. A secret value is screened before it is compared: comparing one
--- raises.
---@param object any the frame or region read
---@param method string its getter
---@return number|nil value
local function read(object, method)
    if type(object) ~= "table" then
        return nil
    end
    local getter = object[method]
    if type(getter) ~= "function" then
        return nil
    end
    local ok, value = pcall(getter, object)
    if not ok then
        return nil
    end
    if issecretvalue and issecretvalue(value) then
        return nil
    end
    if type(value) ~= "number" or value ~= value or value == math.huge or value == -math.huge then
        return nil
    end
    return value
end

--- A layout number in pixels: the getter's answer times the frame's effective
--- scale, so every frame reads in one coordinate space whatever its scale.
---@param object any the frame or region read
---@param method string its getter
---@return number|nil pixels
local function scaled(object, method)
    local value = read(object, method)
    local scale = read(object, "GetEffectiveScale")
    if value == nil or scale == nil or scale <= 0 then
        return nil
    end
    return value * scale
end

--- The attached frame, and whether it is up.
---@class AmdbExtraTip
---@field tip AmdbTipFrame the frame
---@field bars AmdbTipTexture[] created on demand, at most MAX_BARS
---@field shown boolean true between `show` and `hide`
local ExtraTip = {}
ExtraTip.__index = ExtraTip
A.ExtraTip = ExtraTip

--- Creates the frame (SPEC 7.4): a named GameTooltip on the tooltip stratum,
--- clamped to the screen, owned by UIParent with no anchor of its own.
---@return AmdbExtraTip extra
function ExtraTip.new()
    ---@type AmdbTipFrame
    local tip = CreateFrame("GameTooltip", FRAME_NAME, UIParent, "GameTooltipTemplate")
    tip:SetFrameStrata("TOOLTIP")
    tip:SetClampedToScreen(true)
    tip:SetOwner(UIParent, "ANCHOR_NONE")
    return setmetatable({ tip = tip, bars = {}, shown = false }, ExtraTip)
end

---@return boolean shown
function ExtraTip:isShown()
    return self.shown
end

--- Takes the frame down, bars included. Safe to call when it is already down.
function ExtraTip:hide()
    self.tip:Hide()
    self.shown = false
    for index = 1, #self.bars do
        self.bars[index]:Hide()
    end
end

--- The bar of one day, created on first use. Nil when this client cannot draw
--- one: the sparkline is decoration, so it is skipped rather than raised.
---@param index integer 1-based
---@return AmdbTipTexture|nil bar
function ExtraTip:bar(index)
    local bar = self.bars[index]
    if bar == nil then
        local create = self.tip.CreateTexture
        if type(create) ~= "function" then
            return nil
        end
        ---@type AmdbTipTexture
        local texture = create(self.tip, nil, "BACKGROUND", nil)
        if type(texture) ~= "table" or type(texture.SetColorTexture) ~= "function" then
            return nil
        end
        texture:SetColorTexture(BAR_COLOR[1], BAR_COLOR[2], BAR_COLOR[3], 1)
        self.bars[index] = texture
        bar = texture
    end
    return bar
end

--- Draws one sparkline beside its row: on the last `MAX_BARS` days when the
--- series is longer, the most recent day on the right. The bars hang on the
--- row's own font string, so they follow the row wherever the client lays
--- it out, whatever the tooltip's padding.
---@param spark AmdbTipSpark
function ExtraTip:draw(spark)
    local name = self.tip:GetName()
    if type(name) ~= "string" then
        return
    end
    -- The convention Blizzard's own code reads a tooltip's lines by.
    local caption = rawget(_G, name .. "TextLeft" .. spark.line)
    if type(caption) ~= "table" then
        return
    end
    local bars = spark.bars
    local count = math.min(#bars, MAX_BARS)
    local first = #bars - count + 1
    for index = 1, count do
        local bar = self:bar(index)
        if bar == nil then
            return
        end
        local value = bars[first + index - 1]
        bar:ClearAllPoints()
        bar:SetPoint("BOTTOMLEFT", caption, "BOTTOMLEFT", BAR_INSET + (index - 1) * BAR_PITCH, 0)
        bar:SetWidth(BAR_WIDTH)
        bar:SetHeight(math.max(1, math.floor(value / 255 * BAR_HEIGHT + 0.5)))
        bar:Show()
    end
end

--- Puts the frame beside its owner (SPEC 7.5.3): on the side Core/anchor.lua
--- picks from the owner's edges, the frame's width and the screen's right
--- edge, all read in pixels.
---@param owner AmdbTipOwner the tooltip the frame attaches to
function ExtraTip:place(owner)
    local tip = self.tip
    local side = Anchor.side(
        scaled(owner, "GetLeft"),
        scaled(owner, "GetRight"),
        scaled(tip, "GetWidth"),
        scaled(UIParent, "GetRight")
    )
    local point, relativePoint, offsetX, offsetY = Anchor.point(side)
    tip:ClearAllPoints()
    tip:SetPoint(point, owner, relativePoint, offsetX, offsetY)
end

--- Fills the frame with `rows` and shows it beside `owner`. A row with a
--- right-hand text is a double line; one without is a single line; one with
--- bars also draws its sparkline. Whatever was up before comes down first.
---@param owner AmdbTipOwner the tooltip the frame attaches to
---@param rows AmdbTipRow[] the header and the basics at least
function ExtraTip:show(owner, rows)
    self:hide()
    local tip = self.tip
    tip:ClearLines()
    ---@type AmdbTipSpark[]
    local sparklines = {}
    for index = 1, #rows do
        local row = rows[index]
        if row.bars ~= nil then
            sparklines[#sparklines + 1] = { line = tip:NumLines() + 1, bars = row.bars }
        end
        if row.right ~= nil then
            tip:AddDoubleLine(row.left, row.right, 1, 1, 1, 1, 1, 1)
        else
            tip:AddLine(row.left, 1, 1, 1)
        end
    end
    -- Drawn before the frame is shown: the bars hang on rows that already
    -- exist, and follow them when the client lays the frame out.
    for index = 1, #sparklines do
        self:draw(sparklines[index])
    end
    tip:Show()
    self:place(owner)
    self.shown = true
end

return ExtraTip
