local _, A = ...
A.Core = A.Core or {}

-- Core/format.lua - display text (pure Lua).
--
-- money, percent, count, rate and age text, the age / rate / trend color
-- tiers, and the house colors as WoW escape strings. Every formatter answers
-- nil for an absent value, so callers render a dash (or skip the line) instead
-- of printing "nil"; junk that is not a finite number answers nil as well,
-- never an error. Nothing here reads a game global.

---@alias AmdbColorName "brand"|"muted"|"dim"|"good"|"warn"|"hot"|"bad"
---@alias AmdbAgeTier "muted"|"warn"|"hot"|"stale"
---@alias AmdbRateTier "good"|"warn"|"bad"
---@alias AmdbTrendTier "good"|"bad"|"muted"

---@class AmdbFormatModule
---@field COLORS table<AmdbColorName, string> the house colors (SPEC 7.5.4)
---@field ageColor fun(seconds: number|nil): AmdbColorName the house color of an age's tier
---@field RESET string ends a color run
---@field PREFIX string the addon title in the brand color
local Format = {}
A.Core.Format = Format

local HOUR = 3600
local DAY = 24 * HOUR

--- SPEC 7.5.4: each channel is round(v * 255), as a two-digit hex value.
Format.COLORS = {
    brand = "|cffff7d0a",
    muted = "|cff99a1b3",
    dim = "|cff666e80",
    good = "|cff3dc773",
    warn = "|cffffd100",
    hot = "|cffff8040",
    bad = "|cffff4040",
}
Format.RESET = "|r"
Format.PREFIX = Format.COLORS.brand .. "AuctiMogger Database" .. Format.RESET

--- The value itself when it is a finite number, else nil. NaN, the infinities
--- and every other type are junk, and junk formats as nil rather than as zero.
---@param value any
---@return number|nil
local function finite(value)
    if type(value) == "number" and value == value and value ~= math.huge and value ~= -math.huge then
        return value
    end
    return nil
end

--- Lua switches to exponent notation from 1e14; this keeps every digit.
---@param value number an integer-valued, non-negative number
---@return string
local function digits(value)
    return ("%.0f"):format(value)
end

--- Whole number with a separator every three digits: "1,234", "-1,234".
---@param value number an integer-valued number
---@return string
local function grouped(value)
    local sign = value < 0 and "-" or ""
    local reversed = digits(math.abs(value)):reverse()
    -- Separators are inserted from the right, so the leading group is never cut.
    local withSeparators = reversed:gsub("(%d%d%d)", "%1,")
    local trimmed = withSeparators:reverse():gsub("^,", "")
    return sign .. trimmed
end

--- A duration clamped at zero. Absent, negative (a clock a little ahead) and
--- junk all read as zero: an age is never negative.
---@param value any
---@return number
local function duration(value)
    local number = finite(value)
    if number == nil or number < 0 then
        return 0
    end
    return number
end

--- Wraps text in a house color. An unknown name adds nothing (no stray reset);
--- nil text is empty text.
---@param name AmdbColorName|string
---@param text string|number|nil
---@return string
function Format.colorize(name, text)
    if text == nil then
        return ""
    end
    local body = tostring(text)
    local color = Format.COLORS[name]
    if color == nil then
        return body
    end
    return color .. body .. Format.RESET
end

--- Copper as "380g 50s": gold, silver and copper parts only while above zero,
--- except that an amount of zero reads "0c". A negative amount gets its sign
--- in front. Same shape as the sniper's money text.
---@param copper number|nil
---@return string|nil
function Format.money(copper)
    local amount = finite(copper)
    if amount == nil then
        return nil
    end
    local total = math.floor(math.abs(amount) + 0.5)
    -- The sign only when something is left: a negative fraction of a copper is zero.
    local sign = amount < 0 and total > 0 and "-" or ""
    local gold = math.floor(total / 10000)
    local silver = math.floor((total % 10000) / 100)
    local rem = total % 100
    local parts = {}
    if gold > 0 then
        parts[#parts + 1] = gold .. "g"
    end
    if silver > 0 then
        parts[#parts + 1] = silver .. "s"
    end
    if rem > 0 or #parts == 0 then
        parts[#parts + 1] = rem .. "c"
    end
    return sign .. table.concat(parts, " ")
end

--- `money` with a sign in front: "+190g" for a gain, "-190g" for a loss and
--- "0g" when nothing is left after rounding.
---@param copper number|nil
---@return string|nil
function Format.moneySigned(copper)
    local amount = finite(copper)
    if amount == nil then
        return nil
    end
    local text = Format.money(amount)
    if text == "0c" then
        return "0g"
    end
    if amount < 0 then
        return text
    end
    return "+" .. text
end

--- A fraction as a whole percent, rounded: 0.3617 reads "36%".
---@param fraction number|nil
---@return string|nil
function Format.percent(fraction)
    local value = finite(fraction)
    if value == nil then
        return nil
    end
    return digits(math.floor(value * 100 + 0.5)) .. "%"
end

--- Basis points as a whole percent, rounded: 8000 reads "80%".
---@param bp number|nil
---@return string|nil
function Format.percentBp(bp)
    local value = finite(bp)
    if value == nil then
        return nil
    end
    return digits(math.floor(value / 100 + 0.5)) .. "%"
end

--- Basis points as a signed whole percent: "+10%", "-3%", "0%" (no sign on zero).
---@param bp number|nil
---@return string|nil
function Format.signedPercentBp(bp)
    local value = finite(bp)
    if value == nil then
        return nil
    end
    local percent = math.floor(math.abs(value) / 100 + 0.5)
    if percent == 0 then
        return "0%"
    end
    local sign = value < 0 and "-" or "+"
    return sign .. digits(percent) .. "%"
end

--- A count with thousands separators, rounded to a whole unit: "1,234".
---@param n number|nil
---@return string|nil
function Format.count(n)
    local value = finite(n)
    if value == nil then
        return nil
    end
    return grouped(math.floor(value + 0.5))
end

--- One decimal, the trailing ".0" dropped: "2.4", "3".
---@param x number|nil
---@return string|nil
function Format.rate(x)
    local value = finite(x)
    if value == nil then
        return nil
    end
    local text = ("%.1f"):format(value)
    if text:sub(-2) == ".0" then
        text = text:sub(1, -3)
        -- A value that rounds to zero keeps a bare sign ("-0.0" strips to "-0"): drop it.
        if text == "-0" or text == "-" or text == "" then
            text = "0"
        end
    end
    return text
end

--- An age in seconds, floored: "25m" under an hour, "1h 05m" under a day,
--- "2d" from a day on. Absent and negative read as 0 ("0m").
---@param seconds number|nil
---@return string|nil nil only for input that is neither a number nor nil
function Format.age(seconds)
    if seconds ~= nil and type(seconds) ~= "number" then
        return nil
    end
    local age = duration(seconds)
    if age < HOUR then
        return tostring(math.floor(age / 60)) .. "m"
    end
    if age < DAY then
        return ("%dh %02dm"):format(math.floor(age / HOUR), math.floor((age % HOUR) / 60))
    end
    return tostring(math.floor(age / DAY)) .. "d"
end

--- A number of days as "39d", rounded to a whole day.
---@param n number|nil
---@return string|nil
function Format.days(n)
    local value = finite(n)
    if value == nil then
        return nil
    end
    return digits(math.floor(value + 0.5)) .. "d"
end

--- Stock that never clears reads as the single infinity sign: an absent
--- `supplyDays` (nothing sold) and the saturated 65535 alike (SPEC 5.7).
---@param days number|nil
---@return string|nil
function Format.supplyDays(days)
    if days == nil or days == 65535 then
        return "∞"
    end
    return Format.days(days)
end

--- Color tier of an age badge (SPEC 7.5.4): under 2 h muted, under 12 h warn,
--- under 24 h hot, from 24 h stale. Absent reads as fresh.
---@param seconds number|nil
---@return AmdbAgeTier
function Format.ageTier(seconds)
    local age = duration(seconds)
    if age < 2 * HOUR then
        return "muted"
    end
    if age < 12 * HOUR then
        return "warn"
    end
    if age < DAY then
        return "hot"
    end
    return "stale"
end

-- The house color of each age tier (SPEC 7.5.4). The formatter's "stale" tier
-- is the red the color table keeps under "bad".
---@type table<AmdbAgeTier, AmdbColorName>
local AGE_COLORS = {
    muted = "muted",
    warn = "warn",
    hot = "hot",
    stale = "bad",
}

--- The house color of an age badge (SPEC 7.5.4): the color of its tier, so
--- data past 24 hours reads in bad. One table answers for the login line, the
--- tooltip header and `/amdb status`, which cannot drift apart.
---@param seconds number|nil
---@return AmdbColorName color
function Format.ageColor(seconds)
    return AGE_COLORS[Format.ageTier(seconds)]
end

--- Color tier of a sale rate, the sniper's thresholds: 0.5 and above good,
--- 0.2 and above warn, else bad. An absent rate is bad: nothing sold as far
--- as we know.
---@param fraction number|nil
---@return AmdbRateTier
function Format.rateTier(fraction)
    local rate = duration(fraction)
    if rate >= 0.5 then
        return "good"
    end
    if rate >= 0.2 then
        return "warn"
    end
    return "bad"
end

--- Color tier of a trend in basis points: above zero good, below zero bad,
--- zero muted.
---@param bp number|nil
---@return AmdbTrendTier
function Format.trendTier(bp)
    local trend = finite(bp)
    if trend == nil then
        return "muted"
    end
    if trend > 0 then
        return "good"
    end
    if trend < 0 then
        return "bad"
    end
    return "muted"
end

return Format
