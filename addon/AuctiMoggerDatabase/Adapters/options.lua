local ADDON, A = ...

-- Adapters/options.lua - the `/amdb` slash command (SPEC 7.7).
--
-- Everything a player can ask of the addon in one place: the master switch,
-- the status, the primary market of this character, the optional tooltip
-- lines and their presets, Shift detail, and a debug dump of one record. The
-- options panel of SPEC 7.8 reuses the same settings tables and arrives with
-- its own batch.
--
-- A command answer reaches chat at once, through the sink the bootstrap hands
-- in. Every change is written back to the SavedVariables as it happens, so a
-- crash after a change loses nothing. A command that raises says so instead
-- of showing a Lua error.

-- Wiring invariant of the TOC order, failed loudly rather than at the first call.
assert(type(A.Compat) == "table", ADDON .. ": A.Compat must be set before Adapters/options.lua loads")

---@type AmdbSettingsModule
local Preferences = A.Core.Settings
---@type AmdbFormatModule
local Format = A.Core.Format
---@type AmdbCompatModule
local Compat = A.Compat
---@type AmdbResolveModule
local Resolve = A.Core.Resolve

-- The spans in wire order, which the decode dump follows.
local SPAN_NAMES = { "d1", "d7", "d14", "d30" }

---@class AmdbOptionsDeps
---@field store AmdbStore
---@field settings AmdbSettings the live account settings
---@field character AmdbCharacterSettings the live per-character settings
---@field tooltip AmdbTooltip|nil nil: the tooltip adapter could not be built
---@field say fun(text: string) the chat sink, one line a call
---@field persist fun() writes both settings tables back to SavedVariables

---@class AmdbOptions
---@field store AmdbStore
---@field settings AmdbSettings
---@field character AmdbCharacterSettings
---@field tooltip AmdbTooltip|nil
---@field chat fun(text: string) the chat sink handed in
---@field persist fun()
local Options = {}
Options.__index = Options
A.Options = Options

--- The sentences of SPEC 7.11 that the login line and `/amdb status` share.
---@class AmdbStatusTexts
---@field NO_DATA string what to do without a feed
---@field PICK_ONE string how to choose among several markets
Options.TEXT = {
    NO_DATA = "no market data — copy the tracker's feed.lua over Market1.lua",
    PICK_ONE = "several markets loaded, pick one: /amdb market <id>",
}

------------------------------------------------------------------- text parts

---@param text string
---@return string bad
local function bad(text)
    return Format.colorize("bad", text)
end

---@param text string
---@return string warn
local function warn(text)
    return Format.colorize("warn", text)
end

---@param text string
---@return string dim
local function dim(text)
    return Format.colorize("dim", text)
end

--- One line of chat text: control characters and color escapes dropped, and
--- cut, so a typed command cannot fake a line of its own.
---@param text string
---@return string clean
local function plain(text)
    local clean = text:gsub("[%c|]", "")
    if #clean > 64 then
        clean = clean:sub(1, 64) .. "..."
    end
    return clean
end

--- Which preset the optional lines amount to: "basic" when every one is off,
--- "standard" when every one is on, "custom" in between.
---@param settings AmdbSettings
---@return string preset
local function presetName(settings)
    local on = 0
    local total = #Preferences.LINE_KEYS
    for index = 1, total do
        if Preferences.isLineOn(settings, Preferences.LINE_KEYS[index]) then
            on = on + 1
        end
    end
    if on == total then
        return "standard"
    end
    if on == 0 then
        return "basic"
    end
    return "custom"
end

--- One market of `/amdb status`: slot, target, records, age, stale, dropped.
---@param stats AmdbMarketStats
---@param primary string|nil the primary market's target
---@return string row
local function marketRow(stats, primary)
    local row = "slot "
        .. stats.slot
        .. "  "
        .. stats.target
        .. "  "
        .. Format.count(stats.records)
        .. " records ("
        .. Format.count(stats.items)
        .. " items · "
        .. Format.count(stats.commodities)
        .. " commodities · "
        .. Format.count(stats.pets)
        .. " pets)"
        .. "  "
        .. Format.colorize(Format.ageColor(stats.ageSeconds), Format.age(stats.ageSeconds) .. " old")
    if stats.target == primary then
        row = row .. Format.colorize("good", "  primary")
    end
    if stats.stale then
        row = row .. bad("  stale")
    end
    if stats.dropped > 0 then
        row = row .. warn("  " .. Format.count(stats.dropped) .. " dropped")
    end
    return row
end

--- One line of the decode dump: `name value` pairs, an absent field as "-".
---@param fields { name: string, value: any }[] in display order
---@return string line
local function fieldLine(fields)
    local parts = {}
    for index = 1, #fields do
        local field = fields[index]
        parts[#parts + 1] = field.name .. " " .. (field.value ~= nil and tostring(field.value) or "-")
    end
    return table.concat(parts, " · ")
end

--- The commands of SPEC 7.7, as `help` lists them. The line keys are read
--- from the settings module, so the list cannot drift from it.
---@type string[]
local HELP = {
    "/amdb on|off - the tooltip's master switch",
    "/amdb status - markets, primary market, switch, preset",
    "/amdb market - the loaded markets",
    "/amdb market <id> - this character's primary market",
    "/amdb lines - the optional tooltip lines",
    "/amdb basic|standard - every optional line off / on",
    "/amdb <line> on|off - one optional line: " .. table.concat(Preferences.LINE_KEYS, ", "),
    "/amdb shift on|off - Shift shows the detail level",
    "/amdb decode <item> - every decoded field of the item",
    "/amdb help - this list",
}

----------------------------------------------------------------------- setup

---@param deps AmdbOptionsDeps
---@return AmdbOptions options
function Options.new(deps)
    local self = setmetatable({}, Options)
    self.store = deps.store
    self.settings = deps.settings
    self.character = deps.character
    self.tooltip = deps.tooltip
    self.chat = deps.say
    self.persist = deps.persist
    return self
end

--- Registers `/amdb` and its alias (SPEC 7.7).
function Options:register()
    rawset(_G, "SLASH_AMDB1", "/amdb")
    rawset(_G, "SLASH_AMDB2", "/auctimoggerdb")
    SlashCmdList.AMDB = function(msg)
        self:run(msg)
    end
end

--------------------------------------------------------------------- output

--- A data row: printed as it is.
---@param text string
function Options:say(text)
    self.chat(text)
end

--- The answer to a command: the addon title, then the text.
---@param text string
function Options:reply(text)
    self:say(Format.PREFIX .. " " .. text)
end

--- How a command should have been written.
---@param text string the command's shape, without `/amdb`
function Options:usage(text)
    self:say(warn("usage: /amdb " .. text))
end

-------------------------------------------------------------------- dispatch

--- The slash command's entry: a command that raises says so, and shows no
--- Lua error to the player.
---@param msg string|nil what the player typed after the slash command
function Options:run(msg)
    local ok, err = pcall(self.handle, self, msg)
    if not ok then
        self:say(Format.PREFIX .. " " .. bad("command failed: " .. tostring(err)))
    end
end

--- One command of SPEC 7.7.
---@param msg string|nil what the player typed after the slash command
function Options:handle(msg)
    local text = type(msg) == "string" and msg or ""
    local command, argument = text:match("^%s*(%S*)%s*(.-)%s*$")

    if command == "" or command == "help" then
        self:help()
        return
    end
    if command == "on" or command == "off" then
        self:setEnabled(command == "on")
        return
    end
    if command == "status" then
        self:status()
        return
    end
    if command == "market" then
        if argument == "" then
            self:markets()
        else
            self:chooseMarket(argument)
        end
        return
    end
    if command == "lines" then
        self:lines()
        return
    end
    if command == "basic" or command == "standard" then
        self:preset(command)
        return
    end
    if command == "shift" then
        if argument == "on" or argument == "off" then
            self:setShift(argument == "on")
        else
            self:usage("shift on|off")
        end
        return
    end
    if command == "decode" then
        self:decode(argument)
        return
    end
    if argument == "on" or argument == "off" then
        for index = 1, #Preferences.LINE_KEYS do
            if Preferences.LINE_KEYS[index] == command then
                self:setLine(command, argument == "on")
                return
            end
        end
    end
    self:say(warn("unknown command \"" .. plain(command) .. "\""))
    self:help()
end

-------------------------------------------------------------------- commands

--- The help list.
function Options:help()
    for index = 1, #HELP do
        self:say(HELP[index])
    end
end

--- Markets (slot, target, records, age, stale, dropped), the primary market,
--- the switch, the preset, what the client holds for the addon, and what was
--- reported since the login line.
function Options:status()
    local now = A.clock()
    local state = {
        self.settings.enabled and Format.colorize("good", "on") or bad("off"),
        "preset " .. presetName(self.settings),
        "shift detail " .. (self.settings.shiftDetail and "on" or "off"),
    }
    if self.tooltip ~= nil then
        state[#state + 1] = "tooltip hooks: " .. self.tooltip.path
    end
    -- Shaped like the login status line of SPEC 7.11.
    self:say(Format.PREFIX .. ": " .. table.concat(state, " · "))

    local primary = self.store:primaryTarget()
    local stats = self.store:stats(now)
    if #stats == 0 then
        self:say(bad(Options.TEXT.NO_DATA))
    end
    for index = 1, #stats do
        self:say(marketRow(stats[index], primary))
    end

    local rejections = self.store:rejected()
    for index = 1, #rejections do
        local rejection = rejections[index]
        local who = rejection.target ~= nil and ("market \"" .. plain(rejection.target) .. "\"") or "a market"
        self:say(bad("slot " .. rejection.slot .. "  " .. who .. " rejected: " .. plain(rejection.reason)))
    end

    if self.store:primaryIsAmbiguous() then
        self:say(warn(Options.TEXT.PICK_ONE))
    elseif primary ~= nil then
        local chosen = self.store:explicitPrimary()
        if chosen ~= nil and self.store:get(chosen) == nil then
            self:say(warn("chosen market \"" .. plain(chosen) .. "\" is not loaded; using " .. primary))
        end
    end

    -- What the client holds: a missing required global is broken, a missing
    -- optional one only costs the feature its sentence names.
    local compat = Compat.check()
    for index = 1, #compat.missing do
        self:say(bad("missing: " .. compat.missing[index]))
    end
    for index = 1, #compat.absentOptional do
        local name = compat.absentOptional[index]
        self:say(dim("without " .. name .. ": " .. (Compat.fallbacks[name] or "no fallback described")))
    end

    -- The reports recorded since the login line drained the ones before it.
    local reports = A.TakeReports()
    for index = 1, #reports do
        self:say(A.FormatReport(reports[index].text, reports[index].level))
    end
end

--- The loaded markets, the primary one marked.
function Options:markets()
    local primary = self.store:primaryTarget()
    local markets = self.store:markets()
    if #markets == 0 then
        self:say(bad(Options.TEXT.NO_DATA))
        return
    end
    self:reply(#markets .. (#markets == 1 and " market" or " markets") .. " loaded")
    for index = 1, #markets do
        local market = markets[index]
        local text = "  " .. market.target
        if market.target == primary then
            text = text .. Format.colorize("good", "  primary")
        end
        self:say(text .. dim("  slot " .. market.slot))
    end
    self:say(dim("choose this character's primary market: /amdb market <id>"))
end

--- Chooses this character's primary market; it is remembered per character.
---@param target string a loaded market's target id
function Options:chooseMarket(target)
    local ok, message = self.store:setPrimary(target)
    if not ok then
        self:say(warn(message or ("market \"" .. plain(target) .. "\" is not loaded")))
        return
    end
    self.character.primaryMarket = target
    self.persist()
    self:reply("primary market: " .. target)
end

--- The optional lines with their state.
function Options:lines()
    self:reply("optional tooltip lines (" .. presetName(self.settings) .. ")")
    for index = 1, #Preferences.LINE_KEYS do
        local key = Preferences.LINE_KEYS[index]
        local on = Preferences.isLineOn(self.settings, key)
        self:say("  " .. key .. (on and Format.colorize("good", "  on") or bad("  off")))
    end
    self:say(dim("toggle: /amdb <line> on|off · presets: /amdb basic, /amdb standard"))
end

--- Switches one optional line.
---@param key string the line's key
---@param on boolean
function Options:setLine(key, on)
    if not Preferences.setLine(self.settings, key, on) then
        self:say(warn("no optional line is named \"" .. plain(key) .. "\""))
        return
    end
    self.persist()
    self:reply(key .. " line " .. (on and "on" or "off"))
end

--- Applies a preset: "basic" switches every optional line off, "standard" on.
---@param name string the preset's name
function Options:preset(name)
    if not Preferences.applyPreset(self.settings, name) then
        self:say(warn("no preset is named \"" .. plain(name) .. "\""))
        return
    end
    self.persist()
    self:reply("preset " .. name .. ": every optional line " .. (name == "standard" and "on" or "off"))
end

--- The master switch; a shown frame comes down at once when it goes off.
---@param on boolean
function Options:setEnabled(on)
    Preferences.setEnabled(self.settings, on)
    if self.tooltip ~= nil then
        self.tooltip:setEnabled(on)
    end
    self.persist()
    self:reply("tooltips " .. (on and "on" or "off"))
end

--- Whether Shift shows the detail level.
---@param on boolean
function Options:setShift(on)
    Preferences.setShiftDetail(self.settings, on)
    self.persist()
    self:reply("shift detail " .. (on and "on" or "off"))
end

--- Prints every decoded field of one record: the raw numbers, as the wire
--- carries them.
---@param argument string an item id, link or `item:` string; a `battlepet:` string
function Options:decode(argument)
    if argument == "" then
        self:usage("decode <item>")
        return
    end
    local kind, id = Resolve.resolve(argument)
    if kind == nil or id == nil then
        self:say(warn("not an item or a battle pet: " .. plain(argument)))
        return
    end
    local market = self.store:primary()
    if market == nil then
        self:say(bad(Options.TEXT.NO_DATA))
        return
    end

    ---@type AmdbCategory
    local category = "pet"
    local record = market:petRecord(id)
    if kind == "item" then
        category = "item"
        record = market:record(id, "item")
        if record == nil then
            category = "commodity"
            record = market:record(id, "commodity")
        end
    end
    if record == nil then
        self:reply(warn("no record for " .. tostring(id) .. " in " .. market.target))
        return
    end

    local live = record.live
    local craft = record.craft
    local deal = record.deal
    self:reply(
        tostring(id)
            .. " · "
            .. category
            .. " · "
            .. market.target
            .. dim(
                " · feed v"
                    .. tostring(market.meta.version)
                    .. " · "
                    .. Format.age(market:ageSeconds(A.clock()))
                    .. " old"
            )
    )
    for index = 1, #SPAN_NAMES do
        local name = SPAN_NAMES[index]
        local block = record.spans[name]
        if block ~= nil then
            self:say(name .. " " .. fieldLine({
                { name = "n", value = block.n },
                { name = "min", value = block.min },
                { name = "p05", value = block.p05 },
                { name = "p25", value = block.p25 },
                { name = "median", value = block.median },
                { name = "p75", value = block.p75 },
                { name = "p95", value = block.p95 },
                { name = "max", value = block.max },
                { name = "mad", value = block.mad },
                { name = "vwap", value = block.vwap },
                { name = "qtySum", value = block.qtySum },
                { name = "listingCount", value = block.listingCount },
                { name = "saleRate", value = block.saleRate },
                { name = "saleRate180d", value = block.saleRate180d },
                { name = "daysSinceLastSale", value = block.daysSinceLastSale },
                { name = "closuresSinceLastSale", value = block.closuresSinceLastSale },
                { name = "qtySold", value = block.qtySold },
            }))
        end
    end
    self:say("live " .. fieldLine({
        { name = "floor", value = live ~= nil and live.floor or nil },
        { name = "floorQty", value = live ~= nil and live.floorQty or nil },
        { name = "listed", value = live ~= nil and live.listed or nil },
        { name = "qtySum", value = live ~= nil and live.qtySum or nil },
        { name = "seenFloor", value = live ~= nil and live.seenFloor or nil },
        { name = "seenQty", value = live ~= nil and live.seenQty or nil },
    }))
    self:say(fieldLine({
        { name = "value7", value = record.value7 },
        { name = "soldMedian14", value = record.soldMedian14 },
        { name = "samples7", value = record.samples7 },
        { name = "trendBp", value = record.trendBp },
        { name = "soldCount", value = record.soldCount },
        { name = "expiredCount", value = record.expiredCount },
        { name = "supplyDays", value = record.supplyDays },
        { name = "low30", value = record.low30 },
        { name = "lowRank30d", value = record.lowRank30d },
        { name = "vendorPrice", value = record.vendorPrice },
    }))
    local lows = record.lows
    self:say("lows " .. (lows ~= nil and (#lows .. ": " .. table.concat(lows, " ")) or "-"))
    self:say("craft " .. fieldLine({
        { name = "cost", value = craft ~= nil and craft.cost or nil },
        { name = "profit", value = craft ~= nil and craft.profit or nil },
        { name = "expectedProfit", value = craft ~= nil and craft.expectedProfit or nil },
    }))
    self:say("deal " .. fieldLine({
        { name = "rank", value = deal ~= nil and deal.rank or nil },
        { name = "scoreBp", value = deal ~= nil and deal.scoreBp or nil },
        { name = "discountBp", value = deal ~= nil and deal.discountBp or nil },
    }))
    self:say("derived " .. fieldLine({
        { name = "reference", value = record:reference() },
        { name = "valueSource", value = record:valueSource() },
        { name = "saleRate14", value = record:saleRate14() },
        { name = "saleRate180d", value = record:saleRate180d() },
        { name = "daysSinceLastSale", value = record:daysSinceLastSale() },
        { name = "closuresSinceLastSale", value = record:closuresSinceLastSale() },
        { name = "soldPerDay", value = record:soldPerDay() },
        { name = "atVendorFloor", value = tostring(record:atVendorFloor()) },
        { name = "supplyUnbounded", value = tostring(record:supplyUnbounded()) },
    }))
end

return Options
