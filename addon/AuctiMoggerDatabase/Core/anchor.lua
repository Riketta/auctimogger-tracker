local _, A = ...
A.Core = A.Core or {}

-- Core/anchor.lua - where the attached frame goes (pure Lua).
--
-- The frame stands beside the owner tooltip (SPEC 7.5.3): its top-left at the
-- owner's top-right, a gap apart, or mirrored to the owner's left when the
-- right side would cross the screen's right edge. The side is decided here
-- from four numbers already in one coordinate space; the adapter converts
-- with GetEffectiveScale, because a frame's rectangle and a screen edge are
-- only comparable once both are pixels. Nothing here reads a frame, a layout
-- value or a game global.

---@alias AmdbAnchorSide "right"|"left"

---@class AmdbAnchorModule
---@field GAP number horizontal gap between the owner and the frame, in pixels
---@field TOP_OFFSET number the frame's top sits this far below the owner's top, in pixels
---@field side fun(ownerLeft: any, ownerRight: any, frameWidth: any, screenRight: any): AmdbAnchorSide
---@field point fun(side: AmdbAnchorSide): string, string, number, number
local Anchor = {}
A.Core.Anchor = Anchor

Anchor.GAP = 8
Anchor.TOP_OFFSET = 10

---@param value any
---@return boolean finite
local function isFinite(value)
    return type(value) == "number" and value == value and value ~= math.huge and value ~= -math.huge
end

--- The side the frame goes on (SPEC 7.5.3): the owner's right, unless the
--- frame placed there with the gap would cross the screen's right edge.
--- Anything that is not four finite numbers answers the default side: the
--- frame is clamped to the screen anyway, and a default is safer than a guess
--- made of a value that could not be read.
---@param ownerLeft any the owner's left edge
---@param ownerRight any the owner's right edge
---@param frameWidth any the frame's width
---@param screenRight any the screen's right edge
---@return AmdbAnchorSide side
function Anchor.side(ownerLeft, ownerRight, frameWidth, screenRight)
    if not (isFinite(ownerLeft) and isFinite(ownerRight) and isFinite(frameWidth) and isFinite(screenRight)) then
        return "right"
    end
    if ownerRight + Anchor.GAP + frameWidth > screenRight then
        return "left"
    end
    return "right"
end

--- The SetPoint arguments that put the frame on `side` of its owner. The
--- frame's top is the owner's top minus TOP_OFFSET on both sides.
---@param side AmdbAnchorSide
---@return string point the frame's own point
---@return string relativePoint the owner's point it attaches to
---@return number offsetX in pixels, positive to the right
---@return number offsetY in pixels, positive upwards
function Anchor.point(side)
    if side == "left" then
        return "TOPRIGHT", "TOPLEFT", -Anchor.GAP, -Anchor.TOP_OFFSET
    end
    return "TOPLEFT", "TOPRIGHT", Anchor.GAP, -Anchor.TOP_OFFSET
end

return Anchor
