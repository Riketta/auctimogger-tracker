-- Specs for Core/anchor.lua: the side decision of SPEC 7.5.3 and the SetPoint
-- arguments that follow from it (section 10 "anchor").

local Loader = require("spec.helpers.loader")

local A = Loader.load(Loader.namespace(), "Core/anchor.lua")
local Anchor = A.Core.Anchor

describe("Anchor", function()
    it("registers into the namespace and returns its table", function()
        local namespace = Loader.namespace()
        local chunk = assert(loadfile(Loader.ROOT .. "/Core/anchor.lua"))
        local module = chunk(Loader.ADDON, namespace)
        assert.equals(namespace.Core.Anchor, module)
    end)

    it("keeps the gap and the vertical offset of SPEC 7.5.3", function()
        assert.equals(8, Anchor.GAP)
        assert.equals(10, Anchor.TOP_OFFSET)
    end)
end)

describe("Anchor.side", function()
    it("goes right while the frame fits between the owner and the screen edge", function()
        -- 600 + 8 + 200 = 808, well inside 1920.
        assert.equals("right", Anchor.side(400, 600, 200, 1920))
    end)

    it("stays right when the frame ends exactly on the screen's right edge", function()
        -- 600 + 8 + 312 = 920.
        assert.equals("right", Anchor.side(400, 600, 312, 920))
    end)

    it("mirrors to the left once the frame would cross the screen's right edge", function()
        -- One pixel over: 600 + 8 + 313 = 921.
        assert.equals("left", Anchor.side(400, 600, 313, 920))
        assert.equals("left", Anchor.side(1700, 1900, 200, 1920))
    end)

    it("counts the gap, so a frame that would only touch the edge still mirrors", function()
        -- 600 + 320 = 920 fits without the gap; with it, the frame crosses.
        assert.equals("left", Anchor.side(400, 600, 320, 920))
    end)

    it("reads the edges, not the owner's width, so an owner at the origin decides the same", function()
        assert.equals("right", Anchor.side(0, 300, 200, 1920))
        assert.equals("left", Anchor.side(0, 1800, 200, 1920))
    end)

    it("answers the default side for anything that is not four finite numbers", function()
        assert.equals("right", Anchor.side(nil, 600, 200, 1920))
        assert.equals("right", Anchor.side(400, nil, 200, 1920))
        assert.equals("right", Anchor.side(400, 600, nil, 1920))
        assert.equals("right", Anchor.side(400, 600, 200, nil))
        assert.equals("right", Anchor.side(400, 600, 0 / 0, 1920))
        assert.equals("right", Anchor.side(400, 600, math.huge, 1920))
        assert.equals("right", Anchor.side("400", 600, 200, 1920))
    end)
end)

describe("Anchor.point", function()
    it("puts the frame's top-left at the owner's top-right, a gap and the offset apart", function()
        assert.same({ "TOPLEFT", "TOPRIGHT", 8, -10 }, { Anchor.point("right") })
    end)

    it("mirrors: the frame's top-right at the owner's top-left, the same height", function()
        assert.same({ "TOPRIGHT", "TOPLEFT", -8, -10 }, { Anchor.point("left") })
    end)

    it("treats a side it does not know as the right one", function()
        ---@type any
        local side = "sideways"
        assert.same({ "TOPLEFT", "TOPRIGHT", 8, -10 }, { Anchor.point(side) })
    end)
end)
