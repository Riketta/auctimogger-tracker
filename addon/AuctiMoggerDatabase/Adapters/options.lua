local ADDON, A = ...

-- Adapters/options.lua - the `/amdb` slash command (SPEC 7.7) and the options
-- panel (SPEC 7.8).
--
-- Everything a player can change is in one place: the master switch, the
-- primary market of this character, the optional tooltip lines and their
-- presets, Shift detail. The command answers in chat; the panel shows the same
-- settings as controls in the client's Settings UI, over the same row model
-- (Core/panel_rows.lua) and through the same `apply` methods, so the two can
-- never disagree. The status and a debug dump of one record are the command's
-- alone. Every change is written back to the SavedVariables as it happens, so
-- a crash after a change loses nothing. A command that raises says so instead
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
---@type AmdbPanelRowsModule
local PanelRows = A.Core.PanelRows

-- The spans in wire order, which the decode dump follows.
local SPAN_NAMES = { "d1", "d7", "d14", "d30" }

---@class AmdbOptionsDeps
---@field store AmdbStore
---@field settings AmdbSettings the live account settings
---@field character AmdbCharacterSettings the live per-character settings
---@field tooltip AmdbTooltip|nil nil: the tooltip adapter could not be built
---@field say fun(text: string) the chat sink, one line a call
---@field persist fun() writes both settings tables back to SavedVariables
---@field statusLine fun(store: AmdbStore, now: number): string the status line of SPEC 7.11, for the panel

---@class AmdbOptions
---@field store AmdbStore
---@field settings AmdbSettings
---@field character AmdbCharacterSettings
---@field tooltip AmdbTooltip|nil
---@field chat fun(text: string) the chat sink handed in
---@field persist fun()
---@field statusLine fun(store: AmdbStore, now: number): string the status line of SPEC 7.11
---@field panel AmdbOptionsPanel|nil the options panel: nil until it is registered, and on a client without the Settings API
local Options = {}
Options.__index = Options
A.Options = Options

--- The sentences of SPEC 7.11 that the login line and `/amdb status` share,
--- and the note of SPEC 7.8 for a client without an options panel.
---@class AmdbStatusTexts
---@field NO_DATA string what to do without a feed
---@field PICK_ONE string how to choose among several markets
---@field NO_PANEL string what to do on a client without the Settings API
Options.TEXT = {
    NO_DATA = "no market data — copy the tracker's feed.lua over Market1.lua",
    PICK_ONE = "several markets loaded, pick one: /amdb market <id>",
    NO_PANEL = "no options panel on this client — the /amdb command covers everything",
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
    self.statusLine = deps.statusLine
    self.panel = nil
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

--- Applies the primary market of this character, remembers it and writes it
--- back at once. Shared by the command and the panel.
---@param target string a loaded market's target id
---@return boolean ok false when the store refused it
---@return string|nil message the store's reason
function Options:applyMarket(target)
    local ok, message = self.store:setPrimary(target)
    if not ok then
        return false, message
    end
    self.character.primaryMarket = target
    self.persist()
    self:refreshPanel()
    return true
end

--- The command: chooses this character's primary market.
---@param target string a loaded market's target id
function Options:chooseMarket(target)
    local ok, message = self:applyMarket(target)
    if not ok then
        self:say(warn(message or ("market \"" .. plain(target) .. "\" is not loaded")))
        return
    end
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

--- Switches one optional line and writes it back at once. Shared by the
--- command and the panel.
---@param key string the line's key
---@param on boolean
---@return boolean ok false when the key names no line
function Options:applyLine(key, on)
    if not Preferences.setLine(self.settings, key, on) then
        return false
    end
    self.persist()
    self:refreshPanel()
    return true
end

--- The command: switches one optional line.
---@param key string the line's key
---@param on boolean
function Options:setLine(key, on)
    if not self:applyLine(key, on) then
        self:say(warn("no optional line is named \"" .. plain(key) .. "\""))
        return
    end
    self:reply(key .. " line " .. (on and "on" or "off"))
end

--- Applies a preset and writes it back at once: "basic" switches every
--- optional line off, "standard" on. Shared by the command and the panel.
---@param name string the preset's name
---@return boolean ok false for an unknown preset
function Options:applyPreset(name)
    if not Preferences.applyPreset(self.settings, name) then
        return false
    end
    self.persist()
    self:refreshPanel()
    return true
end

--- The command: applies a preset.
---@param name string the preset's name
function Options:preset(name)
    if not self:applyPreset(name) then
        self:say(warn("no preset is named \"" .. plain(name) .. "\""))
        return
    end
    self:reply("preset " .. name .. ": every optional line " .. (name == "standard" and "on" or "off"))
end

--- Applies the master switch and writes it back at once; a shown frame comes
--- down at once when it goes off. Shared by the command and the panel.
---@param on boolean
function Options:applyEnabled(on)
    Preferences.setEnabled(self.settings, on)
    if self.tooltip ~= nil then
        self.tooltip:setEnabled(on)
    end
    self.persist()
    self:refreshPanel()
end

--- The command: the master switch.
---@param on boolean
function Options:setEnabled(on)
    self:applyEnabled(on)
    self:reply("tooltips " .. (on and "on" or "off"))
end

--- Applies whether Shift shows the detail level, and writes it back at once.
--- Shared by the command and the panel.
---@param on boolean
function Options:applyShift(on)
    Preferences.setShiftDetail(self.settings, on)
    self.persist()
    self:refreshPanel()
end

--- The command: whether Shift shows the detail level.
---@param on boolean
function Options:setShift(on)
    self:applyShift(on)
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

---------------------------------------------------------------- the panel
-- SPEC 7.8. A canvas category of our own in the client's Settings UI. The two
-- calls that register it are all this file needs from that API, and every
-- control inside is a frame this file creates, on templates the sniper runs
-- on Forever as well.
--
-- Nothing of ours runs while the game opens or closes its Settings window:
-- the frame has no OnShow, OnHide, OnRefresh, OnCommit or OnDefault. Mastercraft
-- ships its canvas the same way on Forever, where addon code on that path got
-- Log Out and Exit blocked. A shown frame refreshes itself on its own
-- OnUpdate instead, at most once a second, and every `apply` refreshes it at
-- once. A hidden frame is left alone: a hidden frame's OnUpdate does not run.
--
-- Taint: the frame and its controls are ours and unprotected, and the Settings
-- UI only shows, hides and reparents the frame.

--- The slice of the client's Settings API the panel registers through.
---@class AmdbSettingsApi
---@field RegisterCanvasLayoutCategory fun(frame: any, name: string): any, any
---@field RegisterAddOnCategory fun(category: any)

--- The layout a canvas category is registered with: where the Settings UI
--- anchors the frame in its canvas.
---@class AmdbCanvasLayout
---@field AddAnchorPoint fun(self: AmdbCanvasLayout, point: string, x: number, y: number)

--- The slice of the client's Frame the panel is built with. The client's frame
--- types are declared nowhere in this workspace, so the methods are listed.
---@class AmdbPanelFrame
---@field SetSize fun(self: AmdbPanelFrame, width: number, height: number)
---@field SetHeight fun(self: AmdbPanelFrame, height: number)
---@field Hide fun(self: AmdbPanelFrame)
---@field SetScript fun(self: AmdbPanelFrame, scriptType: string, handler: fun(owner: AmdbPanelFrame)|nil)
---@field CreateFontString fun(self: AmdbPanelFrame, name: nil, layer: string, template: string): AmdbPanelText

--- The slice of a font string the panel writes to.
---@class AmdbPanelText
---@field SetText fun(self: AmdbPanelText, text: string)
---@field SetTextColor fun(self: AmdbPanelText, r: number, g: number, b: number)
---@field SetPoint fun(self: AmdbPanelText, point: string, relativeTo: any, relativePoint: string, offsetX: number, offsetY: number)
---@field SetJustifyH fun(self: AmdbPanelText, justify: string)
---@field SetJustifyV fun(self: AmdbPanelText, justify: string)
---@field SetWidth fun(self: AmdbPanelText, width: number)
---@field Hide fun(self: AmdbPanelText)
---@field SetWordWrap (fun(self: AmdbPanelText, wrap: boolean))|nil a client too old to wrap has none

--- The slice of a checkbox or a button the panel creates.
---@class AmdbPanelControl
---@field SetPoint fun(self: AmdbPanelControl, point: string, relativeTo: any, relativePoint: string, offsetX: number, offsetY: number)
---@field SetScript fun(self: AmdbPanelControl, scriptType: string, handler: fun(owner: AmdbPanelControl)|nil)
---@field SetText fun(self: AmdbPanelControl, text: string)
---@field SetSize fun(self: AmdbPanelControl, width: number, height: number)
---@field SetEnabled fun(self: AmdbPanelControl, enabled: boolean)
---@field SetChecked fun(self: AmdbPanelControl, checked: boolean)
---@field GetChecked fun(self: AmdbPanelControl): any a boolean on a current client, nil or true on an old one
---@field SetHitRectInsets fun(self: AmdbPanelControl, left: number, right: number, top: number, bottom: number)
---@field Hide fun(self: AmdbPanelControl)

--- The slice of the client's GameTooltip the hover help shows in.
---@class AmdbPanelTooltip
---@field SetOwner fun(self: AmdbPanelTooltip, owner: any, anchor: string)
---@field AddLine fun(self: AmdbPanelTooltip, text: string, r: number, g: number, b: number)
---@field Show fun(self: AmdbPanelTooltip)
---@field Hide fun(self: AmdbPanelTooltip)

--- One row's controls, and what updates them. The client offers no way to
--- destroy a frame, so a replaced row hides its controls.
---@class AmdbPanelRegion
---@field slot string the row's kind and key: a row of another slot at this index replaces the region
---@field row AmdbPanelRow the model row applied last
---@field hide fun() takes every control of the row down
---@field update fun(row: AmdbPanelRow) brings the controls in line with a fresh model row

--- The options panel: the frame the Settings UI shows, and one region per row
--- of the model, by row index.
---@class AmdbOptionsPanel
---@field frame AmdbPanelFrame
---@field regions (AmdbPanelRegion|nil)[]

-- The category's name in the AddOns list: the addon's title, as the TOC has it.
local CATEGORY = "AuctiMogger Database"
-- The frame's place in the canvas: its top-left corner, this far in. Only that
-- corner is anchored, so the height the rows need is the height the frame has.
local MARGIN = 16
-- The brand color of SPEC 7.5.4, #ff7d0a, on the two titles.
local BRAND = { 1, 125 / 255, 10 / 255 }
-- The label column: a row's control starts right of it.
local LABEL_WIDTH = 150
-- How far a checkbox's clickable area reaches over its label.
local HIT_LABEL = 280
local BUTTON_WIDTH = 110
local BUTTON_HEIGHT = 22
local BUTTON_GAP = 8
local PICKER_WIDTH = 260
local PICKER_HEIGHT = 24
local STATUS_WIDTH = 560

-- The client's Settings API where it can register a canvas; nil otherwise.
-- The global is read at call time: nothing of it is held onto.
---@return AmdbSettingsApi|nil api
local function settingsApi()
    local api = rawget(_G, "Settings")
    if
        type(api) == "table"
        and type(api.RegisterCanvasLayoutCategory) == "function"
        and type(api.RegisterAddOnCategory) == "function"
    then
        return api
    end
    return nil
end

--- The slot of a row: its kind, and the key a checkbox switches. A region
--- keeps the slot it was built for, and a refresh replaces it when the row at
--- its index answers another.
---@param row AmdbPanelRow
---@return string slot
local function regionSlot(row)
    return row.kind .. "/" .. tostring(row.key)
end

--- Applies a fresh model row to a region.
---@param region AmdbPanelRegion
---@param row AmdbPanelRow
local function applyRow(region, row)
    region.row = row
    region.update(row)
end

--- A line of text on the panel: left-aligned, and not anchored yet.
---@param frame AmdbPanelFrame
---@param font string a font object's name
---@return AmdbPanelText text
local function textLine(frame, font)
    ---@type AmdbPanelText
    local text = frame:CreateFontString(nil, "OVERLAY", font)
    text:SetJustifyH("LEFT")
    return text
end

--- The client's tooltip where it can show help, nil otherwise. Help is
--- decoration: a client without it gets none and raises nothing.
---@return AmdbPanelTooltip|nil tooltip
local function helpTooltip()
    local tooltip = rawget(_G, "GameTooltip")
    if
        type(tooltip) == "table"
        and type(tooltip.SetOwner) == "function"
        and type(tooltip.AddLine) == "function"
        and type(tooltip.Hide) == "function"
    then
        return tooltip
    end
    return nil
end

--- Hover help for a control: the client's tooltip at the cursor, a title in
--- the brand color and one line of body text.
---@param control AmdbPanelControl
---@param title string
---@param help string
local function attachHelp(control, title, help)
    control:SetScript("OnEnter", function(owner)
        local tooltip = helpTooltip()
        if tooltip == nil then
            return
        end
        tooltip:SetOwner(owner, "ANCHOR_CURSOR")
        tooltip:AddLine(title, BRAND[1], BRAND[2], BRAND[3])
        tooltip:AddLine(help, 1, 1, 1)
        tooltip:Show()
    end)
    control:SetScript("OnLeave", function()
        local tooltip = helpTooltip()
        if tooltip ~= nil then
            tooltip:Hide()
        end
    end)
end

--- A checkbox row: the box, its label to the right of it, and help on hover.
---@param options AmdbOptions
---@param frame AmdbPanelFrame
---@param row AmdbPanelRow
---@param top number the row's offset from the frame's top edge, in pixels
---@return AmdbPanelRegion region
local function newCheck(options, frame, row, top)
    ---@type AmdbPanelControl
    local box = CreateFrame("CheckButton", nil, frame, "UICheckButtonTemplate")
    box:SetPoint("TOPLEFT", frame, "TOPLEFT", 0, -top)
    box:SetHitRectInsets(0, -HIT_LABEL, 0, 0)
    local text = textLine(frame, "GameFontHighlight")
    text:SetPoint("LEFT", box, "RIGHT", 6, 0)

    local key = row.key
    -- The template toggles nothing by itself: the new state is read before the
    -- box is set, so it comes out right on every client.
    box:SetScript("OnClick", function(control)
        local on = not control:GetChecked()
        control:SetChecked(on)
        options:onPanelToggle(key, on)
    end)
    if row.help ~= nil then
        attachHelp(box, row.text or "", row.help)
    end

    local function hide()
        box:Hide()
        text:Hide()
    end

    ---@param fresh AmdbPanelRow
    local function update(fresh)
        text:SetText(fresh.text or "")
        box:SetChecked(fresh.on == true)
    end
    return { slot = regionSlot(row), row = row, hide = hide, update = update }
end

--- A title row, in the brand color.
---@param _ AmdbOptions unused
---@param frame AmdbPanelFrame
---@param row AmdbPanelRow
---@param top number the row's offset from the frame's top edge, in pixels
---@return AmdbPanelRegion region
local function newHeader(_, frame, row, top)
    local text = textLine(frame, "GameFontHighlight")
    text:SetPoint("TOPLEFT", frame, "TOPLEFT", 0, -(top + 12))
    text:SetTextColor(BRAND[1], BRAND[2], BRAND[3])

    local function hide()
        text:Hide()
    end

    ---@param fresh AmdbPanelRow
    local function update(fresh)
        text:SetText(fresh.text or "")
    end
    return { slot = regionSlot(row), row = row, hide = hide, update = update }
end

--- The preset row: its label, and one button per preset of the model. A
--- button appears the first time the model asks for it.
---@param options AmdbOptions
---@param frame AmdbPanelFrame
---@param row AmdbPanelRow
---@param top number the row's offset from the frame's top edge, in pixels
---@return AmdbPanelRegion region
local function newButtons(options, frame, row, top)
    local text = textLine(frame, "GameFontHighlight")
    text:SetPoint("TOPLEFT", frame, "TOPLEFT", 0, -(top + 5))
    ---@type AmdbPanelControl[]
    local buttons = {}

    local function hide()
        text:Hide()
        for index = 1, #buttons do
            buttons[index]:Hide()
        end
    end

    ---@param fresh AmdbPanelRow
    local function update(fresh)
        text:SetText(fresh.text or "")
        local list = fresh.buttons or {}
        for index = 1, #list do
            local button = buttons[index]
            if button == nil then
                local entry = list[index]
                ---@type AmdbPanelControl
                local control = CreateFrame("Button", nil, frame, "UIPanelButtonTemplate")
                control:SetSize(BUTTON_WIDTH, BUTTON_HEIGHT)
                control:SetPoint(
                    "TOPLEFT",
                    frame,
                    "TOPLEFT",
                    LABEL_WIDTH + (index - 1) * (BUTTON_WIDTH + BUTTON_GAP),
                    -(top + 2)
                )
                local preset = entry.preset
                control:SetScript("OnClick", function()
                    options:applyPreset(preset)
                end)
                if entry.help ~= nil then
                    attachHelp(control, entry.text, entry.help)
                end
                buttons[index] = control
                button = control
            end
            button:SetText(list[index].text)
        end
        for index = #list + 1, #buttons do
            buttons[index]:Hide()
        end
    end
    return { slot = regionSlot(row), row = row, hide = hide, update = update }
end

--- The primary-market row: its label, and the picker button that names the
--- primary market and opens the menu of the loaded ones.
---@param options AmdbOptions
---@param frame AmdbPanelFrame
---@param row AmdbPanelRow
---@param top number the row's offset from the frame's top edge, in pixels
---@return AmdbPanelRegion region
local function newMarket(options, frame, row, top)
    local text = textLine(frame, "GameFontHighlight")
    text:SetPoint("TOPLEFT", frame, "TOPLEFT", 0, -(top + 5))
    ---@type AmdbPanelControl
    local picker = CreateFrame("Button", nil, frame, "UIPanelButtonTemplate")
    picker:SetSize(PICKER_WIDTH, PICKER_HEIGHT)
    picker:SetPoint("TOPLEFT", frame, "TOPLEFT", LABEL_WIDTH, -(top + 1))
    local title = row.text or ""
    picker:SetScript("OnClick", function(control)
        options:openMarketMenu(control, title)
    end)
    if row.help ~= nil then
        attachHelp(picker, title, row.help)
    end

    local function hide()
        text:Hide()
        picker:Hide()
    end

    ---@param fresh AmdbPanelRow
    local function update(fresh)
        text:SetText(fresh.text or "")
        local choices = fresh.choices or {}
        picker:SetEnabled(#choices > 0)
        picker:SetText(fresh.target or (#choices > 0 and "choose a market" or "none loaded"))
    end
    return { slot = regionSlot(row), row = row, hide = hide, update = update }
end

--- The status row: the login line of SPEC 7.11, wrapped.
---@param _ AmdbOptions unused
---@param frame AmdbPanelFrame
---@param row AmdbPanelRow
---@param top number the row's offset from the frame's top edge, in pixels
---@return AmdbPanelRegion region
local function newStatus(_, frame, row, top)
    local text = textLine(frame, "GameFontNormal")
    text:SetPoint("TOPLEFT", frame, "TOPLEFT", 0, -top)
    text:SetWidth(STATUS_WIDTH)
    text:SetJustifyV("TOP")
    -- A client too old to wrap shows the line unwrapped; the canvas has the room.
    if text.SetWordWrap ~= nil then
        text:SetWordWrap(true)
    end

    local function hide()
        text:Hide()
    end

    ---@param fresh AmdbPanelRow
    local function update(fresh)
        text:SetText(fresh.text or "")
    end
    return { slot = regionSlot(row), row = row, hide = hide, update = update }
end

--- The controls each kind of row is built from.
---@type table<AmdbPanelRowKind, fun(options: AmdbOptions, frame: AmdbPanelFrame, row: AmdbPanelRow, top: number): AmdbPanelRegion>
local BUILDERS = {
    check = newCheck,
    header = newHeader,
    buttons = newButtons,
    market = newMarket,
    status = newStatus,
}

--- Registers the panel with the client's Settings API (SPEC 7.8): under
--- Interface, AddOns, the addon's title. The frame and its controls are built
--- here, once, and registered only once every one of them exists. A control
--- this client cannot create raises: the bootstrap reports it, and nothing is
--- left behind.
---@return boolean registered false when this client has no Settings API
function Options:registerPanel()
    if self.panel ~= nil then
        return true
    end
    local api = settingsApi()
    if api == nil then
        return false
    end

    ---@type AmdbPanelFrame
    local frame = CreateFrame("Frame", nil, UIParent)
    -- Ours until the Settings UI parents it to its canvas: it must not stand
    -- on the screen until then.
    frame:Hide()
    frame:SetSize(PanelRows.WIDTH, 0)
    ---@type AmdbOptionsPanel
    local panel = { frame = frame, regions = {} }
    self.panel = panel
    local built, err = pcall(self.renderPanel, self, panel)
    if not built then
        self.panel = nil
        error(err, 0)
    end

    -- The tick: a refresh at most once a second, and only while the frame is
    -- shown. It keeps the status line's age current, and follows a change made
    -- through the command while the panel is open.
    local last = A.clock()
    frame:SetScript("OnUpdate", function(owner)
        local now = A.clock()
        if now == last then
            return
        end
        last = now
        if not self:refreshPanel() then
            owner:SetScript("OnUpdate", nil)
        end
    end)

    local category, layout = api.RegisterCanvasLayoutCategory(frame, CATEGORY)
    if type(layout) == "table" and type(layout.AddAnchorPoint) == "function" then
        layout:AddAnchorPoint("TOPLEFT", MARGIN, -MARGIN)
    end
    api.RegisterAddOnCategory(category)
    return true
end

--- What the rows are built against: the live settings, the store's markets and
--- primary market, and the status line as the clock has it now.
---@return AmdbPanelContext context
function Options:panelContext()
    return {
        settings = self.settings,
        markets = self.store:markets(),
        primary = self.store:primaryTarget(),
        status = self.statusLine(self.store, A.clock()),
    }
end

--- Renders the model into the panel: a region per row, each in line with its
--- row, and the frame as tall as the rows need. A row whose slot changed at
--- an index replaces the region that was there, and a model with fewer rows
--- takes the rest down. Raises on a client that cannot create a control; its
--- two callers decide what that means.
---@param panel AmdbOptionsPanel
function Options:renderPanel(panel)
    local rows = PanelRows.build(self:panelContext())
    local top = PanelRows.TOP
    for index = 1, #rows do
        local row = rows[index]
        local region = panel.regions[index]
        local slot = regionSlot(row)
        if region == nil or region.slot ~= slot then
            if region ~= nil then
                region.hide()
            end
            region = BUILDERS[row.kind](self, panel.frame, row, top)
            panel.regions[index] = region
        end
        applyRow(region, row)
        top = top + row.height
    end
    for index = #rows + 1, #panel.regions do
        local stale = panel.regions[index]
        if stale ~= nil then
            stale.hide()
        end
        panel.regions[index] = nil
    end
    panel.frame:SetHeight(PanelRows.height(rows))
end

--- Refreshes the panel from the settings and the store: after every change,
--- and by the tick. The first error takes the panel out of service and is
--- reported once, so a broken panel never breaks a command.
---@return boolean live false once the panel was taken out of service
function Options:refreshPanel()
    local panel = self.panel
    if panel == nil then
        return true
    end
    local ok, err = pcall(self.renderPanel, self, panel)
    if ok then
        return true
    end
    self:dropPanel()
    A.SayNow("the options panel stopped refreshing: " .. tostring(err), "bad")
    return false
end

--- Takes the panel out of service: its tick stops, and nothing refreshes it
--- again.
function Options:dropPanel()
    local panel = self.panel
    if panel == nil then
        return
    end
    self.panel = nil
    panel.frame:SetScript("OnUpdate", nil)
end

--- A checkbox of the panel was clicked: the key names the master switch, Shift
--- detail or an optional line.
---@param key string|nil
---@param on boolean
function Options:onPanelToggle(key, on)
    if key == "enabled" then
        self:applyEnabled(on)
    elseif key == "shiftDetail" then
        self:applyShift(on)
    elseif key ~= nil then
        self:applyLine(key, on)
    end
end

--- The primary-market picker (SPEC 7.8): where the client has MenuUtil, its
--- own menu with one radio entry per loaded market; a click that moves on to
--- the next market otherwise, the sniper's pattern. Both end in `applyMarket`.
---@param button AmdbPanelControl the picker the menu opens at
---@param title string the picker's label
function Options:openMarketMenu(button, title)
    ---@type string[]
    local targets = {}
    local markets = self.store:markets()
    for index = 1, #markets do
        targets[index] = markets[index].target
    end
    if #targets == 0 then
        return
    end

    local menu = rawget(_G, "MenuUtil")
    if type(menu) == "table" and type(menu.CreateContextMenu) == "function" then
        -- A menu that raises on opening is a menu this client does not have.
        local opened = pcall(function()
            menu.CreateContextMenu(button, function(_, rootDescription)
                rootDescription:CreateTitle(title)
                for index = 1, #targets do
                    local target = targets[index]
                    rootDescription:CreateRadio(target, function()
                        return self.store:primaryTarget() == target
                    end, function()
                        self:applyMarket(target)
                    end)
                end
            end)
        end)
        if opened then
            return
        end
    end

    -- No menu to open: the next market in slot order, round to the first.
    local current = self.store:primaryTarget()
    local chosen = targets[1]
    for index = 1, #targets do
        if targets[index] == current then
            chosen = targets[index % #targets + 1]
            break
        end
    end
    self:applyMarket(chosen)
end

return Options
