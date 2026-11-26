local _, A = ...
A.store = A.Core.Market.Store.new()

-- Adapters/market_files.lua - the carrier entry points and the report sink.
--
-- The only file a slot (Market1.lua .. Market6.lua) talks to: a tracker feed
-- opens with `A.BeginMarket(meta)`, fills the builder that comes back and
-- closes with `A.EndMarket()`. This file creates the one Store at load time
-- (the TOC loads it right before the slots) and owns the three seams every
-- other adapter reads: the clock (`A.clock`), the chat sink (`A.chat`) and the
-- report sink (`A.Say`).
--
-- A report is always recorded, for `/amdb status`. It reaches chat at once
-- only in verbose mode; otherwise bootstrap.lua prints the recorded reports
-- once at login, when the player can actually see them.
--
-- A carrier is untrusted input (SPEC 5.9). Nothing here throws on a bad one:
-- the Store validates and this file only reports.

---@type AmdbStore
local store = A.store

---@type AmdbFormatModule
local Format = A.Core.Format

---@alias AmdbReportLevel "info"|"warn"|"bad"

--- One sentence the addon has to say about the data it loaded.
---@class AmdbReport
---@field text string the sentence, without the addon prefix
---@field level AmdbReportLevel

-- The house color (SPEC 7.5.4) each level prints in. "info" is secondary text.
local LEVEL_COLORS = {
    info = "muted",
    warn = "warn",
    bad = "bad",
}

--- Reads the game clock at call time. Swapping `A.clock` is how a spec moves
--- the clock, so nothing else in the addon may read `time`.
---@return number now epoch seconds
function A.clock()
    return time()
end

--- The one chat sink. `print` is looked up at call time: the client always
--- has it, a harness that loads this file without stubs may not, and then the
--- line is dropped rather than raised.
---@param text string
function A.chat(text)
    if type(print) == "function" then
        print(text)
    end
end

--- Verbose mode: reports reach chat as they happen instead of once at login.
A.verbose = false

--- Every report recorded since the last `A.TakeReports()`.
---@type AmdbReport[]
A.reports = {}

--- The chat line of one report: the addon title, then the text in the color
--- of its level. The one place a report is turned into chat text, so the
--- login line and a verbose report can never differ.
---@param text string
---@param level AmdbReportLevel|nil an unknown level prints as "info"
---@return string line
function A.FormatReport(text, level)
    local color = LEVEL_COLORS[level] or "muted"
    return Format.PREFIX .. " " .. Format.colorize(color, text)
end

--- The report level `level` names; an unknown or an absent one is "info".
---@param level AmdbReportLevel|nil
---@return AmdbReportLevel kind
local function levelOf(level)
    return LEVEL_COLORS[level] and level or "info"
end

--- The one report sink. Always records; prints through `A.chat` at once only
--- when `A.verbose` is set.
---@param text string
---@param level AmdbReportLevel|nil defaults to "info"
function A.Say(text, level)
    local message = tostring(text)
    local kind = levelOf(level)
    A.reports[#A.reports + 1] = { text = message, level = kind }
    if A.verbose then
        A.chat(A.FormatReport(message, kind))
    end
end

--- Reports and prints now, whatever the mode: what `A.Say` records, plus the
--- chat line at once unless `A.Say` already printed it (verbose mode). For
--- something that goes wrong after login, when nothing drains the record
--- again; it is in the record for `/amdb status` all the same.
---@param text string
---@param level AmdbReportLevel|nil defaults to "info"
function A.SayNow(text, level)
    local kind = levelOf(level)
    A.Say(text, kind)
    if not A.verbose then
        A.chat(A.FormatReport(tostring(text), kind))
    end
end

--- Hands out the reports recorded so far and clears the record: the login
--- prints them once, and `/amdb status` shows what came after.
---@return AmdbReport[] reports the reports recorded before this call; the caller owns the list
function A.TakeReports()
    local taken = A.reports
    A.reports = {}
    return taken
end

--- A target as plain one-line chat text: control characters and color escapes
--- are dropped and a long id is cut, the way the Store cleans its own reports,
--- so a hostile carrier cannot fake a chat line.
---@param target string
---@return string clean
local function plain(target)
    local clean = target:gsub("[%c|]", "")
    if #clean > 64 then
        clean = clean:sub(1, 64) .. "..."
    end
    return clean
end

--- The report for a builder the next carrier replaces. The slot file that
--- opened it never reached `A.EndMarket`, so its records never registered.
---@param open AmdbMarketBuilder the builder about to be discarded
---@param slot integer the slot number it would have taken, which is its file's
---@return string text
local function unclosedText(open, slot)
    local meta = open.meta
    local target = type(meta) == "table" and rawget(meta, "target") or nil
    if type(target) == "string" then
        return "unclosed market \""
            .. plain(target)
            .. "\" in slot "
            .. slot
            .. " discarded: its file never called EndMarket"
    end
    return "unclosed market in slot " .. slot .. " discarded: its file never called EndMarket"
end

--- Opens the builder the carrier that is running fills. A builder left open by
--- a malformed file is discarded and reported.
---@param meta AmdbRawHeader|nil the header exactly as the carrier wrote it
---@return AmdbMarketBuilder builder the tables the carrier fills: items, commodities, pets
function A.BeginMarket(meta)
    local open = store.builder
    local slot = store.nextSlot
    local builder = store:begin(meta)
    if open ~= nil then
        A.Say(unclosedText(open, slot), "warn")
    end
    return builder
end

--- Closes the carrier that is running: the Store validates the header and
--- registers what it kept. Every report of the commit goes through `A.Say`:
--- "warn" when the market registered (it replaced another, records were
--- dropped), "bad" when the Store refused it. Never throws: a store error is
--- reported too, because a slot file must not fail the login.
---@return AmdbMarket|nil market nil when nothing was open or the market was rejected
function A.EndMarket()
    local ok, market, reports = pcall(function()
        return store:commit(A.clock())
    end)
    if not ok then
        A.Say("market could not be registered: " .. tostring(market), "bad")
        return nil
    end
    for i = 1, #reports do
        A.Say(reports[i], market ~= nil and "warn" or "bad")
    end
    return market
end

--- The global the version 3 carrier assigned. Before the addon had a market
--- loader a tracker feed was a plain table, and a slot file that still is one
--- calls neither `A.BeginMarket` nor `A.EndMarket`: it registers nothing,
--- records nothing and reads as an empty slot. Its global is the only tell.
local LEGACY_FEED_VARIABLE = "AuctiMoggerTrackerFeed"

--- Reports a version 3 feed left in a market slot, in bad. bootstrap.lua
--- calls it once, at its own load: the TOC loads it after every slot. The
--- addon cannot tell which slot holds the file, so the report says how to
--- recognise it. Nothing is cleared: the global is not ours.
---@return boolean found true when an old tracker feed is loaded
function A.ReportLegacyFeed()
    local legacy = rawget(_G, LEGACY_FEED_VARIABLE)
    if type(legacy) ~= "table" then
        return false
    end
    local version = rawget(legacy, "version")
    local what = type(version) == "number" and ("a version " .. tostring(version) .. " tracker feed")
        or "an old tracker feed"
    A.Say(
        what
            .. " is in a market slot and was not read — update the tracker and download the market again (that slot file opens with "
            .. LEGACY_FEED_VARIABLE
            .. ")",
        "bad"
    )
    return true
end

return A
