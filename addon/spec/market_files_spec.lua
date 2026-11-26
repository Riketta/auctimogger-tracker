-- Specs for Adapters/market_files.lua: the carrier entry points of SPEC 7.4
-- and section 10 "market files".
--
-- The golden carrier runs against the real Store and the real decoder, the way
-- the client runs a slot file, so the whole chain is exercised: carrier ->
-- BeginMarket / EndMarket -> lookups. A header is damaged by rewriting the
-- carrier's own text, never by a hand-written one.

local Loader = require("spec.helpers.loader")

local NOW = 1795560000
local DAY = 86400
local GOLDEN = Loader.FIXTURES .. "/database_v4.lua"
local TARGET = "forever-eu-pvp-alliance"
local GENERATED_AT = 1795559034

local FILES = {
    "Core/pack.lua",
    "Core/record.lua",
    "Core/market.lua",
    "Core/format.lua",
    "Adapters/market_files.lua",
}

--- The golden carrier's header and record words, captured through a namespace
--- that only records what the carrier hands over.
local captured = { items = {}, commodities = {}, pets = {} }
do
    local ns = {
        BeginMarket = function(meta)
            captured.meta = meta
            return captured
        end,
        EndMarket = function() end,
    }
    Loader.carrier(ns, GOLDEN)
end

--- The golden carrier's text.
---@return string text
local function goldenText()
    local file = assert(io.open(GOLDEN, "rb"))
    local text = file:read("*a")
    file:close()
    return text
end

--- Compiles carrier text and runs it the way the client runs a slot file.
---@param A table the namespace the carrier registers into
---@param text string the carrier's Lua
local function runText(A, text)
    local chunk = assert(loadstring(text, "=carrier"))
    return chunk(Loader.ADDON, A)
end

--- The golden carrier with its text rewritten by `patch`.
---@param A table the namespace
---@param patch function(text: string): string
local function runPatched(A, patch)
    return runText(A, patch(goldenText()))
end

--- A damage to the carrier's text: `from` (a pattern) must occur exactly once.
---@param from string a Lua pattern over the carrier's text
---@param to string its replacement
---@return function patch
local function damaged(from, to)
    return function(text)
        local patched, count = text:gsub(from, to)
        assert.equals(1, count, "the carrier text was not rewritten as intended")
        return patched
    end
end

--- A valid header for a synthetic carrier.
---@param target any
---@param generatedAt number epoch seconds
---@return table meta
local function header(target, generatedAt)
    return { version = 4, trackerVersion = "0.1.0", target = target, generatedAt = generatedAt }
end

--- A fresh namespace with Core and market_files.lua loaded, the chat recorded
--- and the clock pinned to NOW.
---@return table A namespace
---@return string[] lines what reached chat
local function fresh()
    local lines = {}
    local A = Loader.load(Loader.namespace(), unpack(FILES))
    A.chat = function(text)
        lines[#lines + 1] = text
    end
    A.clock = function()
        return NOW
    end
    return A, lines
end

--- True when `text` contains `part` as plain text.
---@param text string
---@param part string
local function mentions(text, part)
    return text:find(part, 1, true) ~= nil
end

--- The text of a report, asserted to exist first.
---@param A table the namespace
---@param index integer|nil 1-based, default 1
---@return string text
local function reportText(A, index)
    local report = A.reports[index or 1]
    assert.is_not_nil(report, "no report was recorded")
    return report.text
end

describe("market_files: loading", function()
    it("creates the store while the module loads", function()
        local A = fresh()
        assert.is_table(A.store)
        assert.equals("function", type(A.store.begin))
        assert.equals("function", type(A.store.commit))
        assert.equals(0, #A.store:markets())
        -- One store per namespace: a second load is a second store.
        local other = fresh()
        assert.not_equals(A.store, other.store)
    end)

    it("registers into the namespace and returns it", function()
        local namespace = Loader.load(Loader.namespace(), unpack(FILES))
        local chunk = assert(loadfile(Loader.ROOT .. "/Adapters/market_files.lua"))
        local module = chunk(Loader.ADDON, namespace)
        assert.equals(namespace, module)
        assert.is_table(namespace.store)
        assert.equals("function", type(namespace.BeginMarket))
        assert.equals("function", type(namespace.EndMarket))
        assert.equals("function", type(namespace.clock))
        assert.equals("function", type(namespace.chat))
        assert.equals("function", type(namespace.Say))
        assert.equals("function", type(namespace.SayNow))
        assert.equals("function", type(namespace.FormatReport))
        assert.equals("function", type(namespace.TakeReports))
        assert.equals("function", type(namespace.ReportLegacyFeed))
    end)

    it("starts silent, with no report", function()
        local A, lines = fresh()
        assert.is_false(A.verbose)
        assert.same({}, A.reports)
        assert.same({}, lines)
        assert.same({}, A.TakeReports())
    end)
end)

describe("market_files: the golden carrier", function()
    it("registers one market of nine records", function()
        local A, lines = fresh()
        Loader.carrier(A, GOLDEN)
        local markets = A.store:markets()
        assert.equals(1, #markets)
        assert.equals(1, markets[1].slot)
        assert.equals(TARGET, markets[1].target)
        assert.equals(9, markets[1]:count())
        assert.equals(58, A.store:record(25, "item").spans.d1.n)
        assert.equals(2450000, A.store:petRecord(2546):reference())
        assert.equals(0, #A.reports)
        assert.same({}, lines)
    end)

    it("commits against A.clock, read once per EndMarket", function()
        local A = fresh()
        local reads = 0
        A.clock = function()
            reads = reads + 1
            return GENERATED_AT
        end
        Loader.carrier(A, GOLDEN)
        assert.equals(1, reads)
        assert.equals(0, A.store:primary():ageSeconds(A.clock()))
    end)

    it("reads the game clock at call time through A.clock", function()
        -- Not fresh(): that pins A.clock, and this reads the module's own.
        local A = Loader.load(Loader.namespace(), unpack(FILES))
        -- A stub must land in _G: this file's own globals are private to it.
        local saved = rawget(_G, "time")
        rawset(_G, "time", function()
            return 1234
        end)
        local now = A.clock()
        rawset(_G, "time", saved)
        assert.equals(1234, now)
    end)

    it("is stale once A.clock is a day past its generatedAt", function()
        local A = fresh()
        Loader.carrier(A, GOLDEN)
        assert.equals(NOW - GENERATED_AT, A.store:primary():ageSeconds(A.clock()))
        assert.is_false(A.store:primary():isStale(A.clock()))
        A.clock = function()
            return GENERATED_AT + DAY + 60
        end
        assert.is_true(A.store:primary():isStale(A.clock()))
    end)

    it("is rejected when A.clock says the generator diverged", function()
        local A = fresh()
        A.clock = function()
            return GENERATED_AT - 600
        end
        Loader.carrier(A, GOLDEN)
        assert.equals(0, #A.store:markets())
        assert.equals(1, #A.reports)
        assert.equals("bad", A.reports[1].level)
        assert.is_true(mentions(reportText(A), "in the future"))
    end)

    it("is reported in warn when a later slot replaces the same target", function()
        local A, lines = fresh()
        Loader.carrier(A, GOLDEN)
        Loader.carrier(A, GOLDEN)
        local markets = A.store:markets()
        assert.equals(1, #markets)
        assert.equals(2, markets[1].slot)
        assert.equals(1, #A.reports)
        assert.equals("warn", A.reports[1].level)
        assert.is_true(mentions(reportText(A), "from slot 2 replaces slot 1"))
        assert.same({}, lines)
    end)
end)

describe("market_files: BeginMarket and EndMarket", function()
    it("hands the carrier builder tables named items, commodities and pets", function()
        local A = fresh()
        local meta = header(TARGET, NOW)
        local builder = A.BeginMarket(meta)
        assert.same({}, builder.items)
        assert.same({}, builder.commodities)
        assert.same({}, builder.pets)
        assert.not_equals(builder.items, builder.commodities)
        assert.not_equals(builder.commodities, builder.pets)
        -- The header reaches the store untouched, and the tables reach it through the commit.
        assert.equals(meta, A.store.builder.meta)
        builder.items[25] = captured.items[25]
        builder.commodities[194784] = captured.commodities[194784]
        builder.pets[2546] = captured.pets[2546]
        local market = A.EndMarket()
        assert.is_not_nil(market)
        assert.equals(3, market:count())
        assert.equals(58, A.store:record(25, "item").spans.d1.n)
        assert.equals(0, #A.reports)
    end)

    it("rejects a wrong feed version: reported in bad and remembered by the store", function()
        local A, lines = fresh()
        local market = runPatched(A, damaged("version = 4,", "version = 3,"))
        assert.is_nil(market)
        assert.equals(0, #A.store:markets())
        assert.equals("nil", type(A.store.builder))
        assert.equals(1, #A.reports)
        assert.equals("bad", A.reports[1].level)
        assert.is_true(mentions(reportText(A), "feed version 3 is not supported"))
        -- The report is the store's own reason, behind the store's lead.
        local rejected = A.store:rejected()
        assert.equals(1, #rejected)
        assert.equals(1, rejected[1].slot)
        assert.equals(TARGET, rejected[1].target)
        assert.equals("market \"" .. TARGET .. "\" rejected: " .. rejected[1].reason, reportText(A))
        assert.same({}, lines)
    end)

    it("rejects a missing target without naming one", function()
        local A = fresh()
        local market = runPatched(A, damaged("    target = \"forever%-eu%-pvp%-alliance\",", ""))
        assert.is_nil(market)
        assert.equals(1, #A.reports)
        assert.equals("bad", A.reports[1].level)
        assert.is_true(mentions(reportText(A), "header field target is missing or malformed"))
        local rejected = A.store:rejected()
        assert.equals(1, #rejected)
        assert.is_nil(rejected[1].target)
        assert.equals("header field target is missing or malformed", rejected[1].reason)
    end)

    it("rejects a header that is not a table at all", function()
        local A = fresh()
        A.BeginMarket(nil)
        assert.is_nil(A.EndMarket())
        A.BeginMarket("junk")
        assert.is_nil(A.EndMarket())
        assert.equals(0, #A.store:markets())
        assert.equals(2, #A.reports)
        assert.equals("bad", A.reports[1].level)
        assert.equals("bad", A.reports[2].level)
    end)

    it("replaces an unclosed builder and reports it in warn", function()
        local A, lines = fresh()
        local abandoned = A.BeginMarket(header(TARGET, NOW))
        abandoned.items[25] = captured.items[25]
        local builder = A.BeginMarket(header("other", NOW))
        assert.equals(1, #A.reports)
        assert.equals("warn", A.reports[1].level)
        assert.is_true(mentions(reportText(A), "unclosed market \"" .. TARGET .. "\" in slot 1 discarded"))
        assert.equals("other", A.store.builder.meta.target)
        builder.items[117] = captured.items[117]
        local market = A.EndMarket()
        assert.is_not_nil(market)
        assert.equals("other", market.target)
        assert.equals(1, market:count())
        -- The abandoned one never registered, and was not a rejection either.
        assert.equals(1, #A.store:markets())
        assert.equals(0, #A.store:rejected())
        assert.equals(1, #A.reports)
        assert.same({}, lines)
    end)

    it("reports an unclosed builder without a usable target just as a market", function()
        local A = fresh()
        A.BeginMarket(header(42, NOW))
        A.BeginMarket(header(TARGET, NOW))
        A.EndMarket()
        assert.equals(1, #A.reports)
        assert.equals("warn", A.reports[1].level)
        assert.is_true(mentions(reportText(A), "unclosed market in slot 1 discarded"))
        assert.is_nil(reportText(A):find("\"", 1, true))
    end)

    it("prints a hostile target as plain one-line text", function()
        local A = fresh()
        A.BeginMarket(header("evil|cffff0000\nforged", NOW))
        A.BeginMarket(header(TARGET, NOW))
        A.EndMarket()
        local text = reportText(A)
        assert.is_nil(text:find("|", 1, true))
        assert.is_nil(text:find("\n", 1, true))
        assert.is_true(mentions(text, "evilcffff0000forged"))
    end)

    it("reports EndMarket with nothing open, in bad, and does not throw", function()
        local A, lines = fresh()
        local returned = A.EndMarket()
        assert.is_nil(returned)
        assert.equals(0, #A.store:markets())
        assert.equals(1, #A.reports)
        assert.equals("bad", A.reports[1].level)
        assert.is_true(mentions(reportText(A), "no open market"))
        assert.same({}, lines)
    end)

    it("drops an undecodable record and reports it once for its market", function()
        local A, lines = fresh()
        runPatched(
            A,
            damaged(
                "A%.EndMarket%(%)",
                [[
m.items[26] = "not a record!"
m.items[27] = 7
m.items[28] = "7"
A.EndMarket()]]
            )
        )
        local market = A.store:get(TARGET)
        assert.is_not_nil(market)
        assert.equals(9, market:count())
        assert.equals(3, market.dropped)
        assert.is_nil(A.store:record(26, "item"))
        assert.equals(58, A.store:record(25, "item").spans.d1.n)
        assert.equals(1, #A.reports)
        assert.equals("warn", A.reports[1].level)
        assert.is_true(mentions(reportText(A), "3 record(s) dropped"))
        assert.same({}, lines)
    end)

    it("reports a store error instead of raising it", function()
        local A, lines = fresh()
        A.clock = function()
            return nil
        end
        A.BeginMarket(header(TARGET, NOW))
        local market = A.EndMarket()
        assert.is_nil(market)
        assert.equals(0, #A.store:markets())
        assert.equals(1, #A.reports)
        assert.equals("bad", A.reports[1].level)
        assert.is_true(mentions(reportText(A), "could not be registered"))
        assert.same({}, lines)
    end)
end)

describe("market_files: a version 3 feed left in a slot", function()
    --- Runs `A.ReportLegacyFeed` with the old carrier's global set to `value`.
    --- The global is put back as it was, whatever the call does.
    ---@param A table namespace
    ---@param value any
    ---@return boolean found
    local function reportWith(A, value)
        local previous = rawget(_G, "AuctiMoggerTrackerFeed")
        rawset(_G, "AuctiMoggerTrackerFeed", value)
        local ok, found = pcall(A.ReportLegacyFeed)
        rawset(_G, "AuctiMoggerTrackerFeed", previous)
        assert.is_true(ok, found)
        return found
    end

    it("reports nothing while no old feed is loaded", function()
        local A, lines = fresh()
        assert.is_false(A.ReportLegacyFeed())
        assert.same({}, A.reports)
        assert.same({}, lines)
    end)

    it("ignores a global that holds no table", function()
        local A = fresh()
        assert.is_false(reportWith(A, nil))
        assert.is_false(reportWith(A, "AuctiMoggerTrackerFeed"))
        assert.is_false(reportWith(A, false))
        assert.same({}, A.reports)
    end)

    it("reports the old feed in bad, with its version, and leaves the global alone", function()
        local A, lines = fresh()
        local old = { version = 3, generatedAt = 1795648943 }
        local previous = rawget(_G, "AuctiMoggerTrackerFeed")
        rawset(_G, "AuctiMoggerTrackerFeed", old)
        local ok, found = pcall(A.ReportLegacyFeed)
        local kept = rawget(_G, "AuctiMoggerTrackerFeed")
        rawset(_G, "AuctiMoggerTrackerFeed", previous)
        assert.is_true(ok, found)
        assert.is_true(found)
        assert.equals(old, kept, "the global is not ours to clear")
        assert.equals("bad", A.reports[1].level)
        local text = reportText(A)
        assert.is_true(mentions(text, "a version 3 tracker feed is in a market slot and was not read"))
        assert.is_true(mentions(text, "update the tracker and download the market again"))
        assert.is_true(mentions(text, "AuctiMoggerTrackerFeed"))
        -- Recorded, not printed: the login says it.
        assert.same({}, lines)
    end)

    it("names no version the old feed does not carry", function()
        local A = fresh()
        assert.is_true(reportWith(A, { version = "three" }))
        local text = reportText(A)
        assert.is_true(mentions(text, "an old tracker feed is in a market slot and was not read"))
        assert.is_false(mentions(text, "a version"))
    end)
end)

describe("market_files: reports", function()
    it("drains through TakeReports", function()
        local A = fresh()
        A.Say("first", "bad")
        A.Say("second")
        local taken = A.TakeReports()
        assert.equals(2, #taken)
        assert.equals("first", taken[1].text)
        assert.equals("bad", taken[1].level)
        assert.equals("second", taken[2].text)
        assert.equals("info", taken[2].level)
        assert.same({}, A.TakeReports())
        A.Say("third", "warn")
        assert.equals(1, #A.reports)
        assert.equals("third", A.reports[1].text)
        -- The list handed out is the caller's: later reports never land in it.
        assert.equals(2, #taken)
    end)

    it("records every level and defaults a missing or unknown one to info", function()
        local A = fresh()
        A.Say("a")
        A.Say("b", nil)
        A.Say("c", "info")
        A.Say("d", "warn")
        A.Say("e", "bad")
        A.Say("f", "nonsense")
        A.Say(42)
        local levels = {}
        for i, report in ipairs(A.reports) do
            levels[i] = report.level
        end
        assert.same({ "info", "info", "info", "warn", "bad", "info", "info" }, levels)
        -- Anything is recorded as its text.
        assert.equals("42", A.reports[7].text)
    end)

    it("stays silent unless verbose, then prints at once through A.chat", function()
        local A, lines = fresh()
        local Format = A.Core.Format
        A.Say("quiet", "bad")
        assert.equals(1, #A.reports)
        assert.same({}, lines)
        A.verbose = true
        A.Say("loud", "warn")
        A.Say("noted", "info")
        A.Say("broken", "bad")
        assert.equals(3, #lines)
        assert.equals(Format.PREFIX .. " " .. Format.COLORS.warn .. "loud|r", lines[1])
        assert.equals(Format.PREFIX .. " " .. Format.COLORS.muted .. "noted|r", lines[2])
        assert.equals(Format.PREFIX .. " " .. Format.COLORS.bad .. "broken|r", lines[3])
        assert.equals(4, #A.reports)
    end)

    it("prints a carrier's own report at once in verbose mode", function()
        local A, lines = fresh()
        local Format = A.Core.Format
        A.verbose = true
        A.BeginMarket(header(TARGET, NOW))
        A.BeginMarket(header(TARGET, NOW))
        A.EndMarket()
        -- Only the abandoned builder had something to say; the commit was clean.
        assert.equals(1, #lines)
        assert.equals(1, #A.reports)
        assert.equals(Format.PREFIX .. " " .. Format.COLORS.warn .. reportText(A, 1) .. "|r", lines[1])
    end)

    it("formats one report line: the title, then the text in its level's color", function()
        local A = fresh()
        local Format = A.Core.Format
        local title = Format.PREFIX .. " "
        assert.equals(title .. Format.COLORS.muted .. "note|r", A.FormatReport("note", "info"))
        assert.equals(title .. Format.COLORS.warn .. "note|r", A.FormatReport("note", "warn"))
        assert.equals(title .. Format.COLORS.bad .. "note|r", A.FormatReport("note", "bad"))
        assert.equals(title .. Format.COLORS.muted .. "note|r", A.FormatReport("note", nil))
        -- A verbose Say prints exactly this line.
        local printed
        A.verbose = true
        A.chat = function(text)
            printed = text
        end
        A.Say("same", "warn")
        assert.equals(A.FormatReport("same", "warn"), printed)
    end)

    it("SayNow records, and prints at once, exactly once in either mode", function()
        local A, lines = fresh()
        local Format = A.Core.Format
        A.SayNow("after login", "bad")
        -- Recorded like any report, and printed although the mode is quiet.
        assert.equals(1, #A.reports)
        assert.same({ Format.PREFIX .. " " .. Format.COLORS.bad .. "after login|r" }, lines)
        A.verbose = true
        A.SayNow("still once", "warn")
        assert.equals(2, #A.reports)
        assert.equals(2, #lines)
        -- An unknown level is "info", as for Say.
        A.SayNow("plain", "nonsense")
        assert.equals("info", A.reports[3].level)
        assert.equals(Format.PREFIX .. " " .. Format.COLORS.muted .. "plain|r", lines[3])
    end)

    it("chats through print where it exists, and is silent where it does not", function()
        -- Not fresh(): that replaces A.chat, and this reads the module's own.
        local A = Loader.load(Loader.namespace(), unpack(FILES))
        -- Stubs must land in _G: this file's own globals are private to it.
        local saved = rawget(_G, "print")
        local seen = {}
        rawset(_G, "print", function(text)
            seen[#seen + 1] = text
        end)
        local ok, err = pcall(A.chat, "hello")
        rawset(_G, "print", false)
        local silent, err2 = pcall(A.chat, "hidden")
        rawset(_G, "print", saved)
        assert.is_true(ok, err)
        assert.is_true(silent, err2)
        assert.same({ "hello" }, seen)
    end)
end)
