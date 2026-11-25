-- Spec-only module loader. Gives every spec the load mechanics the game
-- client uses: each addon file is run as `loadfile(path)("AuctiMoggerDatabase",
-- namespace)`, so `local _, A = ...` sees exactly what it sees in the
-- client, and every spec starts from a fresh namespace (nothing is cached in
-- package.loaded, so a module that creates state at load time is re-created
-- per spec). Never shipped: it lives under spec/, outside the addon folder.

local source = debug.getinfo(1, "S").source
-- The directory of this file, with a trailing separator, on either platform.
local here = source:match("^@(.*[/\\])[^/\\]+$") or "./"

local Loader = {}

Loader.ADDON = "AuctiMoggerDatabase"
--- The deployable addon folder, relative to this file.
Loader.ROOT = here .. "../../AuctiMoggerDatabase"
--- The tracker's golden fixtures (addon/SPEC.md section 5.11).
Loader.FIXTURES = here .. "../../../fixtures"

---@return table namespace a fresh, empty namespace table
function Loader.namespace()
    return {}
end

--- Loads addon files, in the order given, into the namespace.
---@param namespace table the namespace table passed to every file as `...`'s second value
---@param ... string paths relative to the addon folder, e.g. "Core/pack.lua"
---@return table namespace the same namespace, for chaining
function Loader.load(namespace, ...)
    for i = 1, select("#", ...) do
        local path = select(i, ...)
        local chunk, err = loadfile(Loader.ROOT .. "/" .. path)
        assert(chunk, "cannot load " .. path .. ": " .. tostring(err))
        chunk(Loader.ADDON, namespace)
    end
    return namespace
end

--- Runs one carrier file (a tracker-generated market file) into the namespace.
---@param namespace table the namespace the carrier registers its market into
---@param path string file path of the carrier
---@return any the carrier chunk's return value
function Loader.carrier(namespace, path)
    local chunk, err = loadfile(path)
    assert(chunk, "cannot load carrier " .. path .. ": " .. tostring(err))
    return chunk(Loader.ADDON, namespace)
end

--- A fresh namespace with the nine Core modules loaded, in TOC order.
---@return table namespace
function Loader.core()
    return Loader.load(
        Loader.namespace(),
        "Core/pack.lua",
        "Core/record.lua",
        "Core/market.lua",
        "Core/settings.lua",
        "Core/format.lua",
        "Core/resolve.lua",
        "Core/tooltip_lines.lua",
        "Core/anchor.lua",
        "Core/panel_rows.lua"
    )
end

return Loader
