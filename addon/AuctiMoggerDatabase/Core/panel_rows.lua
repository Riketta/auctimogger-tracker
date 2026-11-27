local _, A = ...
A.Core = A.Core or {}

-- Core/panel_rows.lua - the options panel's rows (pure Lua).
--
-- The panel of SPEC 7.8 is data-driven. This module turns the live settings,
-- the loaded markets and the status line into the rows the options adapter
-- renders, in a fixed order, and answers the height the frame needs for them.
-- The label and the hover help of every row live here too, next to the
-- settings module's line keys and presets, so the panel cannot drift from the
-- `/amdb` command. The adapter builds one control per row and reads a fresh
-- model on every refresh. Nothing here reads a game global or a clock: the
-- status line arrives in the context, already built.

---@type AmdbSettingsModule
local Settings = A.Core.Settings

---@alias AmdbPanelRowKind "check"|"header"|"buttons"|"market"|"status"

--- One button of the preset row.
---@class AmdbPanelButton
---@field preset string the settings module's name of the preset
---@field text string the button's label
---@field help string|nil hover help

--- One row of the options panel (SPEC 7.8).
---@class AmdbPanelRow
---@field kind AmdbPanelRowKind
---@field height number pixels the row takes in the frame
---@field key string|nil check: "enabled", "shiftDetail", "coins" or an optional line's key
---@field text string|nil check, buttons and market: the label; header: the title; status: the line
---@field on boolean|nil check: whether the box is checked
---@field help string|nil check and market: hover help
---@field buttons AmdbPanelButton[]|nil buttons: the preset buttons, left to right
---@field choices string[]|nil market: the loaded markets' target ids, in slot order
---@field target string|nil market: the primary market's target id; nil when no market is loaded

--- What the rows are built against: the live settings, the store's markets and
--- primary market, and the status line of SPEC 7.11.
---@class AmdbPanelContext
---@field settings AmdbSettings the live settings
---@field markets AmdbMarket[] the loaded markets, in slot order
---@field primary string|nil the primary market's target id
---@field status string the status line of SPEC 7.11

---@class AmdbPanelRowsModule
---@field WIDTH number the frame's width, in pixels
---@field TOP number the space above the first row
---@field BOTTOM number the space below the last row
---@field ROW_HEIGHT number the height of a check, buttons or market row
---@field HEADER_HEIGHT number the height of a header row
---@field STATUS_HEIGHT number the height of the status row: room for three wrapped lines
---@field build fun(context: AmdbPanelContext): AmdbPanelRow[] a fresh model, in display order
---@field height fun(rows: AmdbPanelRow[]): number the height the frame needs for `rows`
local PanelRows = {}
A.Core.PanelRows = PanelRows

PanelRows.WIDTH = 600
PanelRows.TOP = 6
PanelRows.BOTTOM = 10
PanelRows.ROW_HEIGHT = 26
PanelRows.HEADER_HEIGHT = 38
PanelRows.STATUS_HEIGHT = 60

-- The master switch and Shift detail.
local ENABLE_TEXT = "Enable tooltips"
local ENABLE_HELP = "The master switch. Off takes the extra tip off every item; same as /amdb off."
local SHIFT_TEXT = "Hold Shift for detail"
local SHIFT_HELP = "While Shift is held, a tooltip shows sold medians and spans instead of its lines."
local COINS_TEXT = "Coin icons for prices"
local COINS_HELP =
    "Prices as the client's own gold, silver and copper icons instead of text, in tooltips and chat. Needs a client with GetMoneyString."

-- The preset row and the two headers.
local PRESET_TEXT = "Preset"
local LINES_TITLE = "Tooltip lines"
local MARKET_TITLE = "Market"
local MARKET_TEXT = "Primary market"
local MARKET_HELP = "The market whose prices the tooltip shows. Other markets show as cross-market lines."

-- The label and the hover help of every optional line, by the settings module's
-- key. A key without an entry would print as its own key; the spec holds the
-- two tables to the settings module, so that never happens.
---@type table<string, string>
local LINE_TEXT = {
    p05 = "Live p05",
    sold = "Sold per day and supply",
    lastSale = "Last sale",
    listed = "Last listed",
    low = "30-day low",
    sparkline = "30-day chart",
    craft = "Crafting",
    deal = "Deal",
    markets = "Other markets",
}

---@type table<string, string>
local LINE_HELP = {
    p05 = "The price under which the cheapest 5% of the listings in the newest snapshot sit: the sniping level, right under the min buyout.",
    sold = "Units sold per day over the last 14 days, and how long the listed stock lasts.",
    lastSale = "How many days ago the item last sold.",
    listed = "How many days ago the item was last seen listed on the auction house.",
    low = "The lowest buyout of the last 30 days, and how many of those days were higher.",
    sparkline = "A bar a day: the lowest ask of each of the last 30 days.",
    craft = "What the reagents cost, and the profit of crafting and selling the item.",
    deal = "The tracker's deal rank: how far under its value the item is listed.",
    markets = "One line per other loaded market, with that market's price.",
}

-- The label and the hover help of each preset, by the settings module's name.
---@type table<string, string>
local PRESET_LABEL = {
    basic = "Basic",
    standard = "Standard",
}

---@type table<string, string>
local PRESET_HELP = {
    basic = "Only the three basic lines: min buyout, market value and sale rate.",
    standard = "Every line.",
}

--- The target ids of the loaded markets, in slot order. Anything that is not
--- a market with a target is left out: the picker offers what can be chosen.
---@param markets any
---@return string[] targets
local function targets(markets)
    local list = {}
    if type(markets) ~= "table" then
        return list
    end
    for index = 1, #markets do
        local market = markets[index]
        if type(market) == "table" and type(market.target) == "string" then
            list[#list + 1] = market.target
        end
    end
    return list
end

--- A checkbox row.
---@param key string
---@param text string the label
---@param on boolean
---@param help string|nil
---@return AmdbPanelRow row
local function check(key, text, on, help)
    return { kind = "check", height = PanelRows.ROW_HEIGHT, key = key, text = text, on = on, help = help }
end

--- A title row.
---@param text string
---@return AmdbPanelRow row
local function header(text)
    return { kind = "header", height = PanelRows.HEADER_HEIGHT, text = text }
end

--- The model of the panel, in display order: the master switch, the presets,
--- the optional lines with Shift detail and the coin icons under one title, the
--- primary market under another, and the status line last. A fresh table on
--- every call, so the caller owns it.
---@param context AmdbPanelContext
---@return AmdbPanelRow[] rows
function PanelRows.build(context)
    local settings = context.settings
    local rows = {}

    -- The master switch reads as the tooltip adapter reads it: on unless it is
    -- literally switched off.
    local enabled = type(settings) == "table" and settings.enabled ~= false
    rows[#rows + 1] = check("enabled", ENABLE_TEXT, enabled, ENABLE_HELP)

    ---@type AmdbPanelButton[]
    local buttons = {}
    for index = 1, #Settings.PRESETS do
        local preset = Settings.PRESETS[index]
        buttons[index] = { preset = preset, text = PRESET_LABEL[preset] or preset, help = PRESET_HELP[preset] }
    end
    rows[#rows + 1] = { kind = "buttons", height = PanelRows.ROW_HEIGHT, text = PRESET_TEXT, buttons = buttons }

    rows[#rows + 1] = header(LINES_TITLE)
    for index = 1, #Settings.LINE_KEYS do
        local key = Settings.LINE_KEYS[index]
        rows[#rows + 1] = check(key, LINE_TEXT[key] or key, Settings.isLineOn(settings, key), LINE_HELP[key])
    end
    -- Shift detail and the coin icons are off unless literally switched on, as
    -- the adapters read them.
    local settingsTable = type(settings) == "table" and settings or {}
    rows[#rows + 1] = check("shiftDetail", SHIFT_TEXT, settingsTable.shiftDetail == true, SHIFT_HELP)
    rows[#rows + 1] = check("coins", COINS_TEXT, settingsTable.coins == true, COINS_HELP)

    rows[#rows + 1] = header(MARKET_TITLE)
    rows[#rows + 1] = {
        kind = "market",
        height = PanelRows.ROW_HEIGHT,
        text = MARKET_TEXT,
        help = MARKET_HELP,
        choices = targets(context.markets),
        target = context.primary,
    }

    rows[#rows + 1] = { kind = "status", height = PanelRows.STATUS_HEIGHT, text = context.status or "" }
    return rows
end

--- The height the frame needs: the padding and every row.
---@param rows AmdbPanelRow[]
---@return number height
function PanelRows.height(rows)
    local total = PanelRows.TOP + PanelRows.BOTTOM
    for index = 1, #rows do
        total = total + rows[index].height
    end
    return total
end

return PanelRows
