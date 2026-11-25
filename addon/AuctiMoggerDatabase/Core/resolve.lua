local _, A = ...
A.Core = A.Core or {}

-- Core/resolve.lua - item identity (pure Lua).
--
-- Turns what a tooltip hook or a public-API caller hands over (an item id, an
-- item link, an `item:` string, a battle-pet link) into the id the store is
-- keyed by. Pure string and number handling: screening secret values is the
-- adapter's job, and nothing comes back that is not a positive integer.

---@class AmdbResolveModule
local Resolve = {}
A.Core.Resolve = Resolve

---@alias AmdbResolveKind "item"|"pet"

---@param value any
---@return boolean
local function isPositiveInteger(value)
    return type(value) == "number" and value > 0 and value < math.huge and math.floor(value) == value
end

--- The id after `prefix:` inside a link or a bare `prefix:<id>` string.
---@param value string
---@param prefix string plain text, no pattern magic
---@return integer|nil id
local function idAfter(value, prefix)
    local text = value:match(prefix .. ":(%d+)")
    if text == nil then
        return nil
    end
    local id = tonumber(text)
    if isPositiveInteger(id) then
        return id
    end
    return nil
end

--- The item a value names: an item id, a digit-only string, an `item:` string
--- or any string containing `item:<digits>`, a full hyperlink included. An
--- id that is not a positive integer, and every other value, answers nil.
---@param value any
---@return integer|nil itemID
function Resolve.item(value)
    local kind = type(value)
    if kind == "number" then
        return isPositiveInteger(value) and value or nil
    end
    if kind ~= "string" then
        return nil
    end
    if value:match("^%d+$") then
        local id = tonumber(value)
        if isPositiveInteger(id) then
            return id
        end
        return nil
    end
    return idAfter(value, "item")
end

--- The battle-pet species a value names: a `battlepet:` link or string, or a
--- species id passed as a number on purpose (a digit string is not accepted:
--- it would be indistinguishable from an item id).
---@param value any
---@return integer|nil speciesID
function Resolve.species(value)
    local kind = type(value)
    if kind == "number" then
        return isPositiveInteger(value) and value or nil
    end
    if kind ~= "string" then
        return nil
    end
    return idAfter(value, "battlepet")
end

--- What a value names: "pet" and the species id for a `battlepet:` string,
--- "item" and the item id for an item link, an `item:` string, a digit string
--- or a number; nil, nil when it names nothing.
---@param value any
---@return AmdbResolveKind|nil kind
---@return integer|nil id
function Resolve.resolve(value)
    if type(value) == "string" and value:find("battlepet:", 1, true) then
        -- A pet link that does not parse is still a pet link, never an item.
        local speciesID = Resolve.species(value)
        if speciesID == nil then
            return nil, nil
        end
        return "pet", speciesID
    end
    local itemID = Resolve.item(value)
    if itemID == nil then
        return nil, nil
    end
    return "item", itemID
end

return Resolve
