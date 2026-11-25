local _, A = ...
A.Core = A.Core or {}

-- Core/settings.lua - user settings (pure Lua).
--
-- The shape of the two SavedVariables tables and the only code that reads
-- them. Saved data is untrusted, so coercion is strict: only the literals
-- `true` and `false` count and anything else (a wrong type, a missing key,
-- junk) is the default. Nothing here touches a game global; the bootstrap
-- hands the loaded tables in and writes the result back.

---@class AmdbSettingsModule
---@field LINE_KEYS string[] the optional tooltip lines, in display order
---@field PRESETS string[]
local Settings = {}
A.Core.Settings = Settings

Settings.LINE_KEYS = { "sold", "lastSale", "low", "sparkline", "craft", "deal", "markets" }
Settings.PRESETS = { "basic", "standard" }

--- On/off state of every optional tooltip line (SPEC 7.7). A key that is
--- absent in saved data means on; after coercion every key is a boolean.
---@class AmdbLineSettings
---@field sold boolean
---@field lastSale boolean
---@field low boolean
---@field sparkline boolean
---@field craft boolean
---@field deal boolean
---@field markets boolean

--- Account-wide settings (`AuctiMoggerDatabaseSettings`).
---@class AmdbSettings
---@field enabled boolean master switch
---@field shiftDetail boolean Shift shows the detail level
---@field lines AmdbLineSettings

--- A per-character settings table (`AuctiMoggerDatabaseCharacter`): the
--- chosen target id, when the player picked one.
---@class AmdbCharacterSettings
---@field primaryMarket string|nil the chosen target id

---@param key any
---@return boolean
local function isLineKey(key)
    for i = 1, #Settings.LINE_KEYS do
        if Settings.LINE_KEYS[i] == key then
            return true
        end
    end
    return false
end

--- Only the literals count: a saved value that is neither `true` nor `false`
--- answers the default.
---@param value any
---@param fallback boolean
---@return boolean
local function strictBoolean(value, fallback)
    if value == true then
        return true
    end
    if value == false then
        return false
    end
    return fallback
end

--- The defaults (SPEC 7.7): everything on. A fresh table on every call. The
--- line keys mirror LINE_KEYS; the spec holds the two together.
---@return AmdbSettings
function Settings.defaults()
    return {
        enabled = true,
        shiftDetail = true,
        lines = {
            sold = true,
            lastSale = true,
            low = true,
            sparkline = true,
            craft = true,
            deal = true,
            markets = true,
        },
    }
end

--- Copies a SavedVariables table onto a fresh default table. `saved` is
--- whatever the client loaded, so it is untyped and untrusted: only the
--- literals `true` and `false` count, anything else is the default. `saved` is
--- never mutated; unknown keys are ignored; the result holds every line key.
---@param saved any
---@return AmdbSettings
function Settings.coerce(saved)
    local settings = Settings.defaults()
    if saved == nil or type(saved) ~= "table" then
        return settings
    end
    -- rawget: a metatable is not trusted to answer for saved data.
    settings.enabled = strictBoolean(rawget(saved, "enabled"), settings.enabled)
    settings.shiftDetail = strictBoolean(rawget(saved, "shiftDetail"), settings.shiftDetail)
    local lines = rawget(saved, "lines")
    if type(lines) == "table" then
        for i = 1, #Settings.LINE_KEYS do
            local key = Settings.LINE_KEYS[i]
            settings.lines[key] = strictBoolean(rawget(lines, key), settings.lines[key])
        end
    end
    return settings
end

--- Copies the per-character table; only a non-empty `primaryMarket` string
--- survives. `saved` is untyped and untrusted like the account table.
---@param saved any
---@return AmdbCharacterSettings
function Settings.coerceCharacter(saved)
    ---@type string|nil
    local primaryMarket = nil
    if saved ~= nil and type(saved) == "table" then
        local value = rawget(saved, "primaryMarket")
        if type(value) == "string" and #value > 0 then
            primaryMarket = value
        end
    end
    return { primaryMarket = primaryMarket }
end

--- Whether an optional line shows. Absent means on (SPEC 7.7). A key that
--- names no line answers false: there is nothing to show.
---@param settings AmdbSettings
---@param key string
---@return boolean
function Settings.isLineOn(settings, key)
    if not isLineKey(key) then
        return false
    end
    if type(settings) ~= "table" then
        return true
    end
    local lines = rawget(settings, "lines")
    if type(lines) ~= "table" then
        return true
    end
    return rawget(lines, key) ~= false
end

--- Switches one optional line. Like the saved data, the switch is strict:
--- only the literal `true` switches it on.
---@param settings AmdbSettings
---@param key string
---@param on boolean
---@return boolean ok false when the key names no line
function Settings.setLine(settings, key, on)
    if type(settings) ~= "table" or not isLineKey(key) then
        return false
    end
    local lines = rawget(settings, "lines")
    if type(lines) ~= "table" then
        lines = {}
        settings.lines = lines
    end
    lines[key] = on == true
    return true
end

---@param settings AmdbSettings
---@param on boolean
---@return boolean ok false when settings is not a table
function Settings.setShiftDetail(settings, on)
    if type(settings) ~= "table" then
        return false
    end
    settings.shiftDetail = on == true
    return true
end

---@param settings AmdbSettings
---@param on boolean
---@return boolean ok false when settings is not a table
function Settings.setEnabled(settings, on)
    if type(settings) ~= "table" then
        return false
    end
    settings.enabled = on == true
    return true
end

--- "basic" switches every optional line off, "standard" on. The master
--- switch and Shift detail are not touched.
---@param settings AmdbSettings
---@param name string
---@return boolean ok false for an unknown preset
function Settings.applyPreset(settings, name)
    local on
    if name == "basic" then
        on = false
    elseif name == "standard" then
        on = true
    else
        return false
    end
    if type(settings) ~= "table" then
        return false
    end
    for i = 1, #Settings.LINE_KEYS do
        Settings.setLine(settings, Settings.LINE_KEYS[i], on)
    end
    return true
end

return Settings
