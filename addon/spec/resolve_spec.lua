-- Specs for Core/resolve.lua: table tests over every accepted and refused
-- identity (section 10 "resolve").

local Loader = require("spec.helpers.loader")

local A = Loader.namespace()
Loader.load(A, "Core/resolve.lua")
local Resolve = A.Core.Resolve

local ITEM_LINK = "|cff1eff00|Hitem:194784:::::::|h[Arcanist's Sunstone]|h|r"
local PET_LINK = "|cff71d5ff|Hbattlepet:2546:1589:13:329:19:12:0000000000000000|h[Anubisath Idol]|h|r"

--- One `it` per row. Each row is captured for its own test: a Lua 5.1 loop
--- variable is shared by every closure made in the loop.
local function tableTest(name, fn, rows)
    describe(name, function()
        for i = 1, #rows do
            local row = rows[i]
            it("answers " .. tostring(row[2]) .. " for " .. tostring(row[1]), function()
                assert.equals(row[2], fn(row[1]))
            end)
        end
    end)
end

local JUNK = {
    { nil },
    { true },
    { false },
    { {} },
    { "" },
    { "abc" },
    { "item:" },
    { "item:abc" },
    { 0 },
    { -5 },
    { 1.5 },
    { 0 / 0 },
    { math.huge },
}

describe("Resolve", function()
    it("registers into the namespace and returns its table", function()
        local namespace = Loader.namespace()
        local chunk = assert(loadfile(Loader.ROOT .. "/Core/resolve.lua"))
        local module = chunk(Loader.ADDON, namespace)
        assert.equals(namespace.Core.Resolve, module)
    end)
end)

tableTest("Resolve.item", Resolve.item, {
    { 194784, 194784 },
    { 194784.0, 194784 },
    { "194784", 194784 },
    { "0194784", 194784 },
    { "item:194784", 194784 },
    { "item:194784:::::::", 194784 },
    { ITEM_LINK, 194784 },
    { "buy item:25 now", 25 },
    { "|cff1eff00|Hitem:25:0:0:0:0:0:0:0:0:0|h[Soot]|h|r", 25 },
    { "item:0", nil },
    { "item:007", 7 },
    { "0", nil },
    { "000", nil },
    { "1e5", nil },
    { " 25", nil },
    { "25 ", nil },
    { "25abc", nil },
    { PET_LINK, nil },
    { "battlepet:2546", nil },
    unpack(JUNK),
})

tableTest("Resolve.species", Resolve.species, {
    { 2546, 2546 },
    { 2546.0, 2546 },
    { "battlepet:2546", 2546 },
    { "battlepet:2546:1589:13", 2546 },
    { PET_LINK, 2546 },
    { "|cff71d5ff|Hbattlepet:293:1:1:1:1:1:0000000000000000|h[Chicken]|h|r", 293 },
    -- A digit string is an item id to the caller, never a species.
    { "2546", nil },
    { "02546", nil },
    { ITEM_LINK, nil },
    { "item:194784", nil },
    { "battlepet:", nil },
    { "battlepet:abc", nil },
    { "battlepet:0", nil },
    unpack(JUNK),
})

local function resolved(value)
    local kind, id = Resolve.resolve(value)
    if kind == nil then
        return nil
    end
    return kind .. ":" .. id
end

tableTest("Resolve.resolve", resolved, {
    { 194784, "item:194784" },
    { "194784", "item:194784" },
    { "item:194784", "item:194784" },
    { ITEM_LINK, "item:194784" },
    { "battlepet:2546", "pet:2546" },
    { PET_LINK, "pet:2546" },
    -- A pet link that does not parse is not an item either.
    { "battlepet:", nil },
    { "battlepet:abc", nil },
    { "item:abc", nil },
    { "abc", nil },
    { "", nil },
    { nil, nil },
    { 0, nil },
    { -5, nil },
    { 1.5, nil },
    { true, nil },
    { {}, nil },
})

describe("Resolve.resolve: both return values", function()
    it("answers the kind and the id", function()
        local kind, id = Resolve.resolve(ITEM_LINK)
        assert.equals("item", kind)
        assert.equals(194784, id)
        kind, id = Resolve.resolve(PET_LINK)
        assert.equals("pet", kind)
        assert.equals(2546, id)
    end)

    it("answers nil, nil for everything else", function()
        local values = { nil, "abc", 0, -1, 1.5, true, {}, "item:", "battlepet:" }
        for i = 1, #values + 1 do
            local kind, id = Resolve.resolve(values[i])
            assert.is_nil(kind)
            assert.is_nil(id)
        end
    end)
end)
