local ADDON, A = ...

-- Adapters/tooltip.lua - the hooks, the lifecycle and Shift (SPEC 7.5).
--
-- Every item tooltip the game shows (bags, bank, mail, chat links, auction
-- house rows) ends in one of two entry points: the TooltipDataProcessor
-- post-call where the client has it, the OnTooltipSetItem script hook
-- otherwise - never both, or the frame would render twice. Both resolve what
-- the tooltip is showing, read the record and schedule the attached frame
-- for the next frame, by which time the owner has its final size.
--
-- The owner is only ever read. Shift rebuilds our own frame at the other
-- detail level; the owner is never refreshed, so nothing of ours re-enters
-- the game's tooltip code. A render that raises is reported once, and a
-- render that is re-entered while it runs is dropped on the spot.
--
-- Taint: the hooks are the client's own (a post-call, HookScript) and the
-- frame is ours and unprotected. Nothing is combat-gated.

-- Wiring invariant of the TOC order, failed loudly rather than at the first call.
assert(
    type(A.ExtraTip) == "table" and type(A.ExtraTip.new) == "function",
    ADDON .. ": A.ExtraTip must be set before Adapters/tooltip.lua loads"
)

---@type AmdbResolveModule
local Resolve = A.Core.Resolve
---@type AmdbTooltipLinesModule
local TooltipLines = A.Core.TooltipLines
---@type AmdbSettingsModule
local Preferences = A.Core.Settings
---@type AmdbExtraTip
local ExtraTip = A.ExtraTip

---@alias AmdbHookPath "processor"|"script"|"none"

--- What a tooltip is showing: an item by id, or a battle pet by species.
---@class AmdbTipTarget
---@field kind AmdbResolveKind
---@field id integer

--- A target's record in the primary market, and where it was read from.
---@class AmdbTipSubject
---@field target AmdbTipTarget
---@field category AmdbCategory the record table: "item", "commodity" or "pet"
---@field record AmdbRecord a fresh decode
---@field market AmdbMarket the primary market it came from

--- One display of the attached frame.
---@class AmdbTipPending
---@field owner AmdbTipOwner the tooltip the frame attaches to
---@field target AmdbTipTarget what the owner shows
---@field rows AmdbTipRow[] built for the `shift` detail level
---@field shift boolean the detail level the rows were built for

---@class AmdbTooltipDeps
---@field store AmdbStore
---@field settings AmdbSettings the live settings, read at call time
---@field say fun(text: string, level: AmdbReportLevel) where a render error is reported

---@class AmdbTooltip
---@field store AmdbStore
---@field settings AmdbSettings
---@field say fun(text: string, level: AmdbReportLevel)
---@field tip AmdbExtraTip the attached frame
---@field path AmdbHookPath the hook path in use; "none" until `register` ran
---@field busy boolean a render is running: a re-entrant display is dropped
---@field reported boolean true once the first error was reported
---@field sequence integer counts requests, so a display scheduled before the latest does nothing
---@field watched boolean true once the owners' hide hooks are installed
---@field shown AmdbTipPending|nil what the shown frame renders; nil while it is hidden
local Tooltip = {}
Tooltip.__index = Tooltip
A.Tooltip = Tooltip

--- Runs `callback` on the next frame where the client has a timer, and at
--- once otherwise: without C_Timer the frame displays before the owner has
--- its final size, as the compat manifest says.
---@param callback fun()
local function nextFrame(callback)
    local timer = C_Timer
    if type(timer) == "table" and type(timer.After) == "function" then
        -- A plain function of the table, as the client's own code calls it.
        if pcall(function()
            timer.After(0, callback)
        end) then
            return
        end
    end
    callback()
end

--- Whether a value is a plain number: screened first, because a secret value
--- must not even be compared.
---@param value any
---@return boolean plain
local function isNumber(value)
    if issecretvalue and issecretvalue(value) then
        return false
    end
    return type(value) == "number"
end

--- Whether a frame's boolean getter answers true. A missing getter, one that
--- raises and a secret answer all read as false.
---@param object any the frame read
---@param method string its getter
---@return boolean yes
local function answersTrue(object, method)
    if type(object) ~= "table" or type(object[method]) ~= "function" then
        return false
    end
    local ok, value = pcall(object[method], object)
    if not ok or issecretvalue and issecretvalue(value) then
        return false
    end
    return value == true
end

---@return boolean down
local function shiftDown()
    if type(IsShiftKeyDown) ~= "function" then
        return false
    end
    local ok, down = pcall(IsShiftKeyDown)
    return ok and down == true
end

--- Whether `owner` is one of the two tooltips we attach to (SPEC 7.5.1). The
--- globals are read at call time: comparison tooltips and every other frame
--- stay untouched.
---@param owner any
---@return boolean attached
local function isAttached(owner)
    return owner == rawget(_G, "GameTooltip") or owner == rawget(_G, "ItemRefTooltip")
end

--- Whether a processor payload describes a battle pet.
---@param data table the processor's payload
---@return boolean pet
local function isPetData(data)
    if type(Enum) ~= "table" then
        return false
    end
    local types = rawget(Enum, "TooltipDataType")
    if type(types) ~= "table" or types.BattlePet == nil then
        return false
    end
    local kind = rawget(data, "type")
    if issecretvalue and issecretvalue(kind) then
        return false
    end
    return kind == types.BattlePet
end

--- What the owner's own GetItem names: the link is read under pcall and
--- screened, because a secret link is no link.
---@param owner any
---@return AmdbTipTarget|nil target
local function linkTarget(owner)
    if type(owner) ~= "table" or type(owner.GetItem) ~= "function" then
        return nil
    end
    local ok, _, link = pcall(owner.GetItem, owner)
    if not ok or issecretvalue and issecretvalue(link) then
        return nil
    end
    local kind, id = Resolve.resolve(link)
    if kind == nil or id == nil then
        return nil
    end
    return { kind = kind, id = id }
end

--- What a tooltip is showing (SPEC 7.5.1): the processor payload's `id`, else
--- the owner's own item link.
---@param owner any
---@param data any the processor's payload, or nil on the script path
---@return AmdbTipTarget|nil target
local function resolve(owner, data)
    if type(data) == "table" then
        local id = rawget(data, "id")
        if isNumber(id) then
            local pet = isPetData(data)
            local found = pet and Resolve.species(id) or Resolve.item(id)
            if found ~= nil then
                return { kind = pet and "pet" or "item", id = found }
            end
        end
    end
    return linkTarget(owner)
end

--- The record of a target in the primary market: items first, then
--- commodities (SPEC 5.3). Nil when no market is loaded or it lacks the item.
---@param store AmdbStore
---@param target AmdbTipTarget
---@return AmdbTipSubject|nil subject
local function locate(store, target)
    local market = store:primary()
    if market == nil then
        return nil
    end
    if target.kind == "pet" then
        local record = market:petRecord(target.id)
        if record ~= nil then
            return { target = target, category = "pet", record = record, market = market }
        end
        return nil
    end
    local category = "item"
    local record = market:record(target.id, "item")
    if record == nil then
        category = "commodity"
        record = market:record(target.id, "commodity")
    end
    if record == nil then
        return nil
    end
    return { target = target, category = category, record = record, market = market }
end

--- The record of the same item in every other loaded market, in slot order.
---@param store AmdbStore
---@param subject AmdbTipSubject the primary market's record
---@return AmdbTipOther[] others
local function otherMarkets(store, subject)
    local others = {}
    local markets = store:markets()
    for index = 1, #markets do
        local market = markets[index]
        if market ~= subject.market then
            local record
            if subject.target.kind == "pet" then
                record = market:petRecord(subject.target.id)
            else
                record = market:record(subject.target.id, subject.category)
            end
            if record ~= nil then
                others[#others + 1] = { target = market.target, record = record }
            end
        end
    end
    return others
end

---@param deps AmdbTooltipDeps
---@return AmdbTooltip tooltip
function Tooltip.new(deps)
    local self = setmetatable({}, Tooltip)
    self.store = deps.store
    self.settings = deps.settings
    self.say = deps.say
    self.tip = ExtraTip.new()
    self.path = "none"
    self.busy = false
    self.reported = false
    self.sequence = 0
    self.watched = false
    self.shown = nil
    return self
end

--- The master switch, read from the live settings.
---@return boolean enabled
function Tooltip:isEnabled()
    return type(self.settings) == "table" and self.settings.enabled ~= false
end

--- The master switch in the other direction: switching off takes a shown
--- frame down at once, and drops what was scheduled.
---@param on boolean
function Tooltip:setEnabled(on)
    Preferences.setEnabled(self.settings, on)
    if not self:isEnabled() then
        self:hide()
    end
end

--- Whether the attached frame is up.
---@return boolean shown
function Tooltip:isShown()
    return self.shown ~= nil
end

--- Takes the frame down and voids the display scheduled before this call.
function Tooltip:hide()
    self.tip:hide()
    self.shown = nil
    self.sequence = self.sequence + 1
end

---@param err any what a render raised
function Tooltip:report(err)
    if self.reported then
        return
    end
    self.reported = true
    -- The sink must not turn a report into a second error.
    pcall(self.say, "tooltip error (reported once): " .. tostring(err), "bad")
end

--- The detail level to build for: Shift held, the settings allowing it, and
--- the owner not collapsed (a collapsed tooltip gets the basics only).
---@param owner any
---@return boolean detail
function Tooltip:detailWanted(owner)
    return self.settings.shiftDetail == true and shiftDown() and not answersTrue(owner, "IsTooltipCollapsed")
end

--- Everything the row builder reads besides the record.
---@param subject AmdbTipSubject
---@param owner any
---@param shift boolean the detail level to build for
---@return AmdbTipContext context
function Tooltip:context(subject, owner, shift)
    local market = subject.market
    local now = A.clock()
    return {
        itemID = subject.target.kind == "item" and subject.target.id or nil,
        speciesID = subject.target.kind == "pet" and subject.target.id or nil,
        category = subject.category,
        target = market.target,
        marketAge = market:ageSeconds(now),
        auctionsAge = market:snapshotAge("auctions", now),
        commoditiesAge = market:snapshotAge("commodities", now),
        feedVersion = market.meta.version,
        others = otherMarkets(self.store, subject),
        collapsed = answersTrue(owner, "IsTooltipCollapsed"),
        shift = shift,
        settings = self.settings,
    }
end

--- Builds the rows for a target the primary market has, and schedules the
--- display for the next frame (SPEC 7.5.2). A newer request voids an older
--- schedule, so only the latest tooltip is ever shown.
---@param owner any
---@param target AmdbTipTarget
function Tooltip:prepare(owner, target)
    local subject = locate(self.store, target)
    if subject == nil then
        self:hide()
        return
    end
    local shift = self:detailWanted(owner)
    local rows = TooltipLines.build(subject.record, self:context(subject, owner, shift))
    self.sequence = self.sequence + 1
    local sequence = self.sequence
    local pending = { owner = owner, target = target, rows = rows, shift = shift }
    nextFrame(function()
        self:display(pending, sequence)
    end)
end

--- The display scheduled by `prepare`: nothing once it was superseded, and
--- nothing when the owner is no longer shown or the switch went off.
---@param pending AmdbTipPending
---@param sequence integer the request the display belongs to
function Tooltip:display(pending, sequence)
    if sequence ~= self.sequence or self.busy then
        return
    end
    if not self:isEnabled() or not answersTrue(pending.owner, "IsShown") then
        return
    end
    self.busy = true
    local ok, err = pcall(self.tip.show, self.tip, pending.owner, pending.rows)
    self.busy = false
    if ok then
        self.shown = pending
    else
        -- Whatever a failed render left up comes down.
        pcall(self.tip.hide, self.tip)
        self:report(err)
    end
end

--- The hook entry, for both paths: resolve what the owner shows, then show
--- the frame for it or take the frame down.
---@param owner any the tooltip the post-call or the hook names
---@param data any the processor's payload; nil on the script path
function Tooltip:onTooltip(owner, data)
    local ok, err = pcall(function()
        if not self:isEnabled() or not isAttached(owner) then
            self:hide()
            return
        end
        local target = resolve(owner, data)
        if target == nil then
            self:hide()
            return
        end
        self:prepare(owner, target)
    end)
    if not ok then
        self:report(err)
    end
end

--- MODIFIER_STATE_CHANGED: a Shift press or release rebuilds a shown frame
--- at the other detail level (SPEC 7.5.2). The owner is not touched.
---@param key any the modifier the event names ("LSHIFT" or "RSHIFT")
function Tooltip:onModifier(key)
    if type(key) ~= "string" or key:find("SHIFT", 1, true) == nil then
        return
    end
    local shown = self.shown
    if shown == nil or self.busy then
        return
    end
    if self:detailWanted(shown.owner) == shown.shift then
        return
    end
    local ok, err = pcall(self.prepare, self, shown.owner, shown.target)
    if not ok then
        self:report(err)
    end
end

--- Installs the hide hooks of one owner tooltip: the frame comes down with
--- its owner, whether the game clears it or hides it.
---@param frame any a GameTooltip or an ItemRefTooltip
function Tooltip:watchOwner(frame)
    for _, script in ipairs({ "OnTooltipCleared", "OnHide" }) do
        pcall(frame.HookScript, frame, script, function()
            self:hide()
        end)
    end
end

--- The preferred path (SPEC 7.5.1): one post-call for items and, where the
--- client names the type, the same for battle pets.
---@return boolean installed
function Tooltip:registerProcessor()
    local processor = TooltipDataProcessor
    if type(processor) ~= "table" or type(processor.AddTooltipPostCall) ~= "function" then
        return false
    end
    local types = type(Enum) == "table" and rawget(Enum, "TooltipDataType") or nil
    local itemType = type(types) == "table" and rawget(types, "Item") or nil
    if itemType == nil then
        return false
    end
    -- The item type is the path; a failure here falls through to the scripts.
    local ok = pcall(processor.AddTooltipPostCall, itemType, function(tooltip, data)
        self:onTooltip(tooltip, data)
    end)
    if not ok then
        return false
    end
    self.path = "processor"
    -- The pet type is optional: without it, battle-pet tooltips get no frame.
    local petType = rawget(types, "BattlePet")
    if petType ~= nil then
        pcall(processor.AddTooltipPostCall, petType, function(tooltip, data)
            self:onTooltip(tooltip, data)
        end)
    end
    return true
end

--- The fallback path (SPEC 7.5.1): OnTooltipSetItem on the two frames.
function Tooltip:registerScripts()
    for _, name in ipairs({ "GameTooltip", "ItemRefTooltip" }) do
        local frame = rawget(_G, name)
        if type(frame) == "table" and type(frame.HookScript) == "function" then
            local ok = pcall(frame.HookScript, frame, "OnTooltipSetItem", function(tip)
                self:onTooltip(tip, nil)
            end)
            if ok then
                self.path = "script"
            end
        end
    end
end

--- Installs the hooks, once: the processor path where the client has it, the
--- script path otherwise, and the owner's hide hooks on both. Idempotent.
---@return AmdbHookPath path
function Tooltip:register()
    if self.path == "none" and not self:registerProcessor() then
        self:registerScripts()
    end
    if not self.watched then
        self.watched = true
        for _, name in ipairs({ "GameTooltip", "ItemRefTooltip" }) do
            local frame = rawget(_G, name)
            if type(frame) == "table" and type(frame.HookScript) == "function" then
                self:watchOwner(frame)
            end
        end
    end
    return self.path
end

return Tooltip
