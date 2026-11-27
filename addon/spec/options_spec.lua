-- Specs for Adapters/options.lua: the `/amdb` command of SPEC 7.7, command by
-- command, and that every change is written back as it happens.
--
-- The real store runs behind the command, with the golden carrier loaded. The
-- game globals go into _G with rawset (a spec file's own globals are private
-- to it): the five the manifest requires are dummies, and every optional one
-- starts absent, so `/amdb status` has something to say about them.

local Loader = require("spec.helpers.loader")
local Encode = require("spec.helpers.encode")

local FILES = {
    "Core/pack.lua",
    "Core/record.lua",
    "Core/market.lua",
    "Core/settings.lua",
    "Core/format.lua",
    "Core/resolve.lua",
    "Core/tooltip_lines.lua",
    "Core/anchor.lua",
    "Adapters/market_files.lua",
    "Adapters/compat.lua",
    "Adapters/options.lua",
}

local NOW = 1795560000
local DAY = 86400
local GOLDEN = Loader.FIXTURES .. "/database_v4.lua"
local TARGET = "forever-eu-pvp-alliance"
local OTHER = "forever-eu-pvp-horde"

local LINK = "|cff1eff00|Hitem:25::::::::120:::::|h[An Item]|h|r"

local TITLE = "|cffff7d0aAuctiMogger Database|r"
local MUTED = "|cff99a1b3"
local DIM = "|cff666e80"
local GOOD = "|cff3dc773"
local WARN = "|cffffd100"
local BAD = "|cffff4040"
local RESET = "|r"

local NO_DATA = "no market data — copy the tracker's feed.lua over Market1.lua"
local PICK_ONE = "several markets loaded, pick one: /amdb market <id>"

local FIRST_LINE = TITLE
    .. ": "
    .. GOOD
    .. "on"
    .. RESET
    .. " · preset standard · shift detail on · tooltip hooks: processor"

--- The manifest's required globals, as dummies an addon can live with.
local REQUIRED = {
    CreateFrame = function() end,
    GameTooltip = {},
    UIParent = {},
    time = function()
        return NOW
    end,
    SlashCmdList = {},
}

--- The manifest's optional globals, each starting absent.
local OPTIONAL = {
    "TooltipDataProcessor",
    "Enum",
    "ItemRefTooltip",
    "issecretvalue",
    "IsShiftKeyDown",
    "Settings",
    "MenuUtil",
    "GetMoneyString",
}

local STUBS = { "CreateFrame", "GameTooltip", "UIParent", "time", "SlashCmdList", "SLASH_AMDB1", "SLASH_AMDB2" }

--- One span block, worth `price` and nothing else.
---@param price integer copper
---@return table block
local function block(price)
    return {
        n = 1,
        min = price,
        p05 = price,
        p25 = price,
        median = price,
        p75 = price,
        p95 = price,
        max = price,
        mad = 0,
        vwap = price,
        qtySum = 1,
        listingCount = 1,
    }
end

--- A carrier of one market: item 25 worth `price` when there is one, and
--- item 117 as `junk`, a record word that will not decode.
---@param target string
---@param generatedAt number epoch seconds
---@param price integer|nil copper; nil: no record at all
---@param junk string|nil
---@param version integer|nil the feed version to write; 4 by default
---@return string text the carrier's Lua
local function carrier(target, generatedAt, price, junk, version)
    local lines = {
        "local _, A = ...",
        ("local m = A.BeginMarket({ version = %s, trackerVersion = '0.1.0', target = %s, generatedAt = %s })"):format(
            tostring(version or 4),
            string.format("%q", target),
            tostring(generatedAt)
        ),
    }
    if price ~= nil then
        lines[#lines + 1] = "m.items[25] = '" .. Encode.record({ spans = { d30 = block(price) } }) .. "'"
    end
    if junk ~= nil then
        lines[#lines + 1] = "m.items[117] = '" .. junk .. "'"
    end
    lines[#lines + 1] = "A.EndMarket()"
    return table.concat(lines, "\n")
end

--- Runs carrier text the way the client runs a slot file.
---@param A table namespace
---@param text string the carrier's Lua
local function runCarrier(A, text)
    local chunk = assert(loadstring(text, "=carrier"))
    chunk(Loader.ADDON, A)
end

--- Whether some line equals `text`.
---@param lines string[]
---@param text string
---@return boolean present
local function hasLine(lines, text)
    for index = 1, #lines do
        if lines[index] == text then
            return true
        end
    end
    return false
end

--- Whether some line carries `part` as plain text.
---@param lines string[]
---@param part string
---@return boolean mentioned
local function mentions(lines, part)
    for index = 1, #lines do
        if lines[index]:find(part, 1, true) ~= nil then
            return true
        end
    end
    return false
end

--- The `name value` fields of a decode line, without its lead: the line is
--- fields joined by " · ", an absent field being `-`.
---@param line string
---@param lead string the label the line starts with, with its space
---@return string[] fields
local function fieldsOf(line, lead)
    assert.equals(lead, line:sub(1, #lead))
    local fields = {}
    for field in (line:sub(#lead + 1) .. " · "):gmatch("(.-) · ") do
        fields[#fields + 1] = field
    end
    return fields
end

describe("the /amdb command", function()
    local A
    local options
    local lines
    local persisted
    local tooltip
    local saved

    --- The options over a namespace, the chat recorded and the clock pinned.
    ---@param namespace table
    ---@return table options
    local function build(namespace)
        namespace.clock = function()
            return NOW
        end
        return namespace.Options.new({
            store = namespace.store,
            settings = namespace.Core.Settings.defaults(),
            character = { primaryMarket = nil },
            tooltip = tooltip,
            say = function(text)
                lines[#lines + 1] = text
            end,
            persist = function()
                persisted = persisted + 1
            end,
            statusLine = function()
                return "status line"
            end,
        })
    end

    --- What the command answered to `input`.
    ---@param input string|nil what the player typed
    ---@return string[] answer
    local function run(input)
        lines = {}
        options:run(input)
        return lines
    end

    --- The options over an addon that loaded no carrier at all.
    local function withoutFeed()
        options = build(Loader.load(Loader.namespace(), unpack(FILES)))
        return options
    end

    --- What a decode reply names: the id, and the record table it read.
    ---@param input string what follows `decode`
    ---@return string id
    ---@return string category
    local function decoded(input)
        local answer = run("decode " .. input)
        local prefix = TITLE .. " "
        assert.equals(prefix, answer[1]:sub(1, #prefix))
        local id, category = answer[1]:sub(#prefix + 1):match("^(%S+) · (%S+) · ")
        assert.is_not_nil(id, answer[1])
        return id, category
    end

    before_each(function()
        saved = {}
        for _, name in ipairs(STUBS) do
            saved[name] = rawget(_G, name)
        end
        for _, name in ipairs(OPTIONAL) do
            saved[name] = rawget(_G, name)
        end
        for name, value in pairs(REQUIRED) do
            rawset(_G, name, value)
        end
        for _, name in ipairs(OPTIONAL) do
            rawset(_G, name, nil)
        end
        rawset(_G, "SLASH_AMDB1", nil)
        rawset(_G, "SLASH_AMDB2", nil)

        lines = {}
        persisted = 0
        tooltip = { path = "processor", switches = {} }
        function tooltip:setEnabled(on)
            self.switches[#self.switches + 1] = on
        end
        A = Loader.load(Loader.namespace(), unpack(FILES))
        Loader.carrier(A, GOLDEN)
        options = build(A)
    end)

    after_each(function()
        for _, name in ipairs(STUBS) do
            rawset(_G, name, saved[name])
        end
        for _, name in ipairs(OPTIONAL) do
            rawset(_G, name, saved[name])
        end
    end)

    it("registers into the namespace and returns its table", function()
        local namespace = Loader.load(Loader.namespace(), unpack(FILES, 1, #FILES - 1))
        local chunk = assert(loadfile(Loader.ROOT .. "/Adapters/options.lua"))
        local module = chunk(Loader.ADDON, namespace)
        assert.equals(namespace.Options, module)
    end)

    it("shares the sentences of SPEC 7.11 with the login line", function()
        assert.equals(NO_DATA, A.Options.TEXT.NO_DATA)
        assert.equals(PICK_ONE, A.Options.TEXT.PICK_ONE)
    end)

    ------------------------------------------------------------ registration

    describe("registration", function()
        it("registers /amdb and its alias, and routes both to the command", function()
            options:register()
            assert.equals("/amdb", rawget(_G, "SLASH_AMDB1"))
            assert.equals("/auctimoggerdb", rawget(_G, "SLASH_AMDB2"))
            assert.equals("function", type(REQUIRED.SlashCmdList.AMDB))
            REQUIRED.SlashCmdList.AMDB("off")
            assert.is_false(options.settings.enabled)
        end)

        it("says a command that raised, instead of showing a Lua error", function()
            options.handle = function()
                error("boom", 0)
            end
            assert.same({ TITLE .. " " .. BAD .. "command failed: boom" .. RESET }, run("anything"))
        end)
    end)

    -------------------------------------------------------------------- help

    describe("help", function()
        it("lists the commands of SPEC 7.7 for a bare /amdb and for help", function()
            local bare = run("")
            local help = run("help")
            assert.equals(11, #bare)
            assert.same(help, bare)
            assert.equals("/amdb on|off - the tooltip's master switch", bare[1])
            assert.equals("/amdb status - markets, primary market, switch, preset", bare[2])
            assert.equals("/amdb market <id> - this character's primary market", bare[4])
            assert.equals("/amdb basic|standard - every optional line off / on", bare[6])
            assert.equals("/amdb shift on|off - Shift shows the detail level", bare[8])
            assert.equals("/amdb coins on|off - prices as coin icons instead of text", bare[9])
            assert.equals("/amdb decode <item> - every decoded field of the item", bare[10])
        end)

        it("names every optional line in the help, from the settings module", function()
            assert.equals(
                "/amdb <line> on|off - one optional line: " .. table.concat(A.Core.Settings.LINE_KEYS, ", "),
                run("help")[7]
            )
        end)

        it("answers an unknown command with a warning and the list", function()
            local answer = run("frobnicate")
            assert.equals(WARN .. "unknown command \"frobnicate\"" .. RESET, answer[1])
            assert.equals(12, #answer)
            assert.equals("/amdb help - this list", answer[12])
        end)

        it("drops color codes and control characters from what it echoes", function()
            -- Neither a color escape nor a line break can be smuggled into chat.
            assert.equals(WARN .. "unknown command \"cff000000evilr\"" .. RESET, run("|cff000000evil|r")[1])
            assert.equals(WARN .. "unknown command \"one\"" .. RESET, run("one\ntwo")[1])
        end)
    end)

    ------------------------------------------------------------------ on / off

    describe("on and off", function()
        it("switches the tooltips off, tells the tooltip adapter, and writes it back at once", function()
            assert.same({ TITLE .. " tooltips off" }, run("off"))
            assert.is_false(options.settings.enabled)
            assert.same({ false }, tooltip.switches)
            assert.equals(1, persisted)
        end)

        it("switches them back on", function()
            run("off")
            assert.same({ TITLE .. " tooltips on" }, run("on"))
            assert.is_true(options.settings.enabled)
            assert.same({ false, true }, tooltip.switches)
            assert.equals(2, persisted)
        end)

        it("changes nothing that needs saving when the command is only read", function()
            run("status")
            run("lines")
            run("market")
            assert.equals(0, persisted)
        end)
    end)

    ------------------------------------------------------------------- status

    describe("status", function()
        it("starts with the switch, the preset, Shift detail, and the tooltip's hook path", function()
            assert.same({ FIRST_LINE }, { run("status")[1] })
        end)

        it("reads the hook path from the tooltip adapter, and says nothing of hooks without one", function()
            tooltip.path = "script"
            assert.is_true(mentions(run("status"), "tooltip hooks: script"))
            options.tooltip = nil
            assert.is_false(mentions(run("status"), "tooltip hooks"))
        end)

        it("reads the switch, the preset and Shift detail from the live settings", function()
            A.Core.Settings.applyPreset(options.settings, "basic")
            A.Core.Settings.setShiftDetail(options.settings, false)
            A.Core.Settings.setEnabled(options.settings, false)
            assert.equals(
                TITLE
                    .. ": "
                    .. BAD
                    .. "off"
                    .. RESET
                    .. " · preset basic · shift detail off · tooltip hooks: processor",
                run("status")[1]
            )
        end)

        it("names a preset of mixed lines custom", function()
            A.Core.Settings.setLine(options.settings, "deal", false)
            assert.is_true(mentions(run("status"), "preset custom"))
        end)

        it("prints one row per market: slot, target, records, age, and the primary mark", function()
            local answer = run("status")
            assert.equals(
                "slot 1  "
                    .. TARGET
                    .. "  9 records (4 items · 3 commodities · 2 pets)  "
                    .. MUTED
                    .. "16m old"
                    .. RESET
                    .. GOOD
                    .. "  primary"
                    .. RESET,
                answer[2]
            )
        end)

        it("marks a stale market and its dropped records", function()
            runCarrier(A, carrier(OTHER, NOW - 2 * DAY, 100, "not-a-record"))
            assert.equals(
                "slot 2  "
                    .. OTHER
                    .. "  1 records (1 items · 0 commodities · 0 pets)  "
                    .. BAD
                    .. "2d old"
                    .. RESET
                    .. BAD
                    .. "  stale"
                    .. RESET
                    .. WARN
                    .. "  1 dropped"
                    .. RESET,
                run("status")[3]
            )
        end)

        it("asks for a choice with several markets and none made", function()
            runCarrier(A, carrier(OTHER, NOW - 600, 100))
            assert.is_true(hasLine(run("status"), WARN .. PICK_ONE .. RESET))
        end)

        it("says when the chosen market is not loaded", function()
            options.character.primaryMarket = "not-loaded"
            A.store:setExplicitPrimary("not-loaded")
            assert.is_true(
                hasLine(run("status"), WARN .. "chosen market \"not-loaded\" is not loaded; using " .. TARGET .. RESET)
            )
        end)

        it("has nothing to ask once the chosen market is loaded", function()
            runCarrier(A, carrier(OTHER, NOW - 600, 100))
            assert.is_true(A.store:setPrimary(OTHER))
            local answer = run("status")
            assert.is_false(mentions(answer, "chosen market"))
            assert.is_false(hasLine(answer, WARN .. PICK_ONE .. RESET))
            -- The mark moved to the chosen market's row.
            assert.is_true(mentions({ answer[3] }, GOOD .. "  primary" .. RESET))
            assert.is_false(mentions({ answer[2] }, "primary"))
        end)

        it("lists the markets a slot file could not register", function()
            runCarrier(A, carrier("old-v3", NOW - 600, 100, nil, 3))
            assert.is_true(
                hasLine(
                    run("status"),
                    BAD .. "slot 2  market \"old-v3\" rejected: feed version 3 is not supported (expected 4)" .. RESET
                )
            )
        end)

        it("says what to do without a feed", function()
            withoutFeed()
            local answer = run("status")
            assert.same({ FIRST_LINE, BAD .. NO_DATA .. RESET }, { answer[1], answer[2] })
        end)

        it("reports a required global the client lacks, in bad", function()
            rawset(_G, "GameTooltip", nil)
            assert.is_true(hasLine(run("status"), BAD .. "missing: GameTooltip" .. RESET))
        end)

        it("names each absent optional surface with its fallback", function()
            local withoutPanel = DIM
                .. "without Settings: the options panel is skipped with a one-time note; the slash command covers everything"
                .. RESET
            local withoutScreening = DIM
                .. "without issecretvalue: values are used unscreened; a client without it exposes no secret values"
                .. RESET
            local withoutMenu = DIM
                .. "without MenuUtil: the primary-market picker of the options panel moves to the next market on each click instead of opening a menu"
                .. RESET
            local withoutCoins = DIM
                .. "without GetMoneyString: prices stay in text: /amdb coins on refuses and says so, and the panel's box stays off"
                .. RESET
            local answer = run("status")
            assert.is_true(hasLine(answer, withoutPanel))
            assert.is_true(hasLine(answer, withoutScreening))
            assert.is_true(hasLine(answer, withoutMenu))
            assert.is_true(hasLine(answer, withoutCoins))
            -- The login line, one market row, then one fallback line per
            -- optional surface that is absent here.
            assert.equals(2 + #OPTIONAL, #answer)
            rawset(_G, "Settings", {})
            rawset(_G, "issecretvalue", function() end)
            answer = run("status")
            assert.is_false(hasLine(answer, withoutPanel))
            assert.is_false(hasLine(answer, withoutScreening))
            -- The menu is still missing.
            assert.is_true(hasLine(answer, withoutMenu))
        end)

        it("prints the reports recorded since the login line, once", function()
            A.Say("market \"x\" from slot 2 replaces slot 1", "warn")
            A.Say("3 record(s) dropped (undecodable)", "bad")
            local first = run("status")
            assert.is_true(hasLine(first, TITLE .. " " .. WARN .. "market \"x\" from slot 2 replaces slot 1" .. RESET))
            assert.is_true(hasLine(first, TITLE .. " " .. BAD .. "3 record(s) dropped (undecodable)" .. RESET))
            assert.same({}, A.TakeReports())
            assert.is_false(mentions(run("status"), "3 record(s) dropped"))
        end)
    end)

    ------------------------------------------------------------------ market

    describe("market", function()
        it("lists the loaded markets, the primary one marked, and how to choose", function()
            runCarrier(A, carrier(OTHER, NOW - 600, 100))
            assert.same({
                TITLE .. " 2 markets loaded",
                "  " .. TARGET .. GOOD .. "  primary" .. RESET .. DIM .. "  slot 1" .. RESET,
                "  " .. OTHER .. DIM .. "  slot 2" .. RESET,
                DIM .. "choose this character's primary market: /amdb market <id>" .. RESET,
            }, run("market"))
        end)

        it("reads one market as one market", function()
            assert.equals(TITLE .. " 1 market loaded", run("market")[1])
        end)

        it("chooses this character's primary market, and writes it back at once", function()
            runCarrier(A, carrier(OTHER, NOW - 600, 100))
            assert.same({ TITLE .. " primary market: " .. OTHER }, run("market " .. OTHER))
            assert.equals(OTHER, A.store:primaryTarget())
            assert.equals(OTHER, options.character.primaryMarket)
            assert.equals(1, persisted)
            -- The status row follows.
            assert.is_true(
                hasLine(
                    run("status"),
                    "slot 2  "
                        .. OTHER
                        .. "  1 records (1 items · 0 commodities · 0 pets)  "
                        .. MUTED
                        .. "10m old"
                        .. RESET
                        .. GOOD
                        .. "  primary"
                        .. RESET
                )
            )
        end)

        it("refuses a market that is not loaded, with the store's reason", function()
            assert.same({ WARN .. "market \"nowhere\" is not loaded" .. RESET }, run("market nowhere"))
            assert.is_nil(options.character.primaryMarket)
            assert.equals(0, persisted)
        end)

        it("says what to do without a feed", function()
            withoutFeed()
            assert.same({ BAD .. NO_DATA .. RESET }, run("market"))
        end)
    end)

    ------------------------------------------------------------------- lines

    describe("lines", function()
        it("lists every optional line with its state", function()
            local answer = run("lines")
            assert.equals(TITLE .. " optional tooltip lines (standard)", answer[1])
            assert.equals("  sold" .. GOOD .. "  on" .. RESET, answer[3])
            assert.equals("  p05" .. GOOD .. "  on" .. RESET, answer[2])
            assert.equals("  listed" .. GOOD .. "  on" .. RESET, answer[5])
            assert.equals("  markets" .. GOOD .. "  on" .. RESET, answer[10])
            assert.equals(11, #answer)
        end)

        it("switches one line, and writes it back at once", function()
            assert.same({ TITLE .. " deal line off" }, run("deal off"))
            assert.is_false(options.settings.lines.deal)
            assert.is_true(options.settings.lines.sold)
            assert.equals(1, persisted)
            assert.same({ TITLE .. " deal line on" }, run("deal on"))
            assert.is_true(options.settings.lines.deal)
        end)

        it("lists a switched line as off, and the lines as custom", function()
            run("sparkline off")
            local answer = run("lines")
            assert.equals(TITLE .. " optional tooltip lines (custom)", answer[1])
            assert.equals("  sparkline" .. BAD .. "  off" .. RESET, answer[7])
        end)

        it("answers every key of the settings module", function()
            for _, key in ipairs(A.Core.Settings.LINE_KEYS) do
                assert.same({ TITLE .. " " .. key .. " line off" }, run(key .. " off"))
                assert.is_false(options.settings.lines[key])
            end
        end)

        it("answers a line it does not know as an unknown command", function()
            assert.equals(WARN .. "unknown command \"nonsense\"" .. RESET, run("nonsense off")[1])
            assert.equals(0, persisted)
        end)
    end)

    ----------------------------------------------------------------- presets

    describe("presets", function()
        it("basic switches every optional line off", function()
            assert.same({ TITLE .. " preset basic: every optional line off" }, run("basic"))
            for _, key in ipairs(A.Core.Settings.LINE_KEYS) do
                assert.is_false(options.settings.lines[key])
            end
            assert.equals(1, persisted)
        end)

        it("standard switches every optional line back on", function()
            run("basic")
            assert.same({ TITLE .. " preset standard: every optional line on" }, run("standard"))
            for _, key in ipairs(A.Core.Settings.LINE_KEYS) do
                assert.is_true(options.settings.lines[key])
            end
        end)

        it("leaves the master switch and Shift detail alone", function()
            run("off")
            run("shift off")
            run("basic")
            assert.is_false(options.settings.enabled)
            assert.is_false(options.settings.shiftDetail)
        end)
    end)

    ------------------------------------------------------------------- shift

    describe("shift", function()
        it("switches the detail level, and writes it back at once", function()
            assert.same({ TITLE .. " shift detail off" }, run("shift off"))
            assert.is_false(options.settings.shiftDetail)
            assert.equals(1, persisted)
            assert.same({ TITLE .. " shift detail on" }, run("shift on"))
            assert.is_true(options.settings.shiftDetail)
        end)

        it("asks for on or off", function()
            assert.same({ WARN .. "usage: /amdb shift on|off" .. RESET }, run("shift"))
            assert.same({ WARN .. "usage: /amdb shift on|off" .. RESET }, run("shift sideways"))
            assert.equals(0, persisted)
        end)
    end)

    ------------------------------------------------------------------- coins

    describe("coins", function()
        --- A stand-in for the client's money string: it writes its arguments, so
        --- the test sees exactly what the renderer hands it.
        local function fakeCoins(copper, separateThousands, checkGoldThreshold, showZeroAsGold)
            return ("<%d|%s|%s|%s>"):format(
                copper,
                tostring(separateThousands),
                tostring(checkGoldThreshold),
                tostring(showZeroAsGold)
            )
        end

        it("asks for on or off", function()
            assert.same({ WARN .. "usage: /amdb coins on|off" .. RESET }, run("coins"))
            assert.same({ WARN .. "usage: /amdb coins on|off" .. RESET }, run("coins maybe"))
            assert.equals(0, persisted)
        end)

        it("refuses to switch them on without GetMoneyString, and changes nothing", function()
            local answer = run("coins on")
            assert.same({
                WARN .. "this client has no GetMoneyString: prices stay in text" .. RESET,
            }, answer)
            assert.is_false(options.settings.coins)
            assert.equals(0, persisted)
            assert.equals("1g 23s 45c", A.Core.Format.money(12345))
        end)

        it("switches them off without the API too: there is nothing to undo", function()
            assert.same({ TITLE .. " coin icons off" }, run("coins off"))
            assert.is_false(options.settings.coins)
        end)

        it("switches the prices to the client's money string, and writes it back at once", function()
            rawset(_G, "GetMoneyString", fakeCoins)
            assert.same({ TITLE .. " coin icons on" }, run("coins on"))
            assert.is_true(options.settings.coins)
            assert.equals(1, persisted)
            local Format = A.Core.Format
            -- The whole, non-negative amount; no thousands separators; copper shown
            -- whenever there is any; zero as the gold coin alone.
            assert.equals("<12345|false|false|true>", Format.money(12345))
            assert.equals("<12346|false|false|true>", Format.money(12345.6))
            -- The sign stays in front, and zero keeps its own form.
            assert.equals("-<500|false|false|true>", Format.money(-500))
            assert.equals("+<500|false|false|true>", Format.moneySigned(500))
            assert.equals("-<500|false|false|true>", Format.moneySigned(-500))
            assert.equals("<0|false|false|true>", Format.moneySigned(0))
            assert.is_nil(Format.money(nil))
            assert.same({ TITLE .. " coin icons off" }, run("coins off"))
            assert.is_false(options.settings.coins)
            assert.equals("1g 23s 45c", Format.money(12345))
            assert.equals("0g", Format.moneySigned(0))
        end)

        it("names the coin icons in the status line only while they are on", function()
            assert.is_false(mentions(run("status"), "coin icons"))
            rawset(_G, "GetMoneyString", fakeCoins)
            run("coins on")
            assert.equals(
                TITLE
                    .. ": "
                    .. GOOD
                    .. "on"
                    .. RESET
                    .. " · preset standard · shift detail on · coin icons on · tooltip hooks: processor",
                run("status")[1]
            )
            run("coins off")
            assert.is_false(mentions(run("status"), "coin icons"))
        end)

        it("follows the loaded settings through syncCoins, the way the bootstrap applies them", function()
            options.settings.coins = true
            options:syncCoins()
            -- The setting says on, but the client has nothing to draw coins with.
            assert.equals("12s 50c", A.Core.Format.money(1250))
            rawset(_G, "GetMoneyString", fakeCoins)
            options:syncCoins()
            assert.equals("<1250|false|false|true>", A.Core.Format.money(1250))
            options.settings.coins = false
            options:syncCoins()
            assert.equals("12s 50c", A.Core.Format.money(1250))
        end)

        it("are left alone by the presets, the lines, Shift detail and the master switch", function()
            rawset(_G, "GetMoneyString", fakeCoins)
            run("coins on")
            run("basic")
            run("shift off")
            run("off")
            assert.is_true(options.settings.coins)
            assert.equals("<1250|false|false|true>", A.Core.Format.money(1250))
        end)
    end)

    ------------------------------------------------------------------- decode

    describe("decode", function()
        it("prints every decoded field of the item, by id", function()
            local answer = run("decode 25")
            assert.equals(
                TITLE .. " " .. "25 · item · " .. TARGET .. DIM .. " · feed v4 · 16m old" .. RESET,
                answer[1]
            )
            -- The first span, in wire order, with its ledger fields.
            assert.same({
                "n 58",
                "min 575",
                "p05 600",
                "p25 1575",
                "median 15000",
                "p75 19985",
                "p95 50100",
                "max 50100",
                "mad 5100",
                "vwap 15912",
                "qtySum 58",
                "listingCount 58",
                "saleRate 0.8889",
                "saleRate180d 0.8889",
                "daysSinceLastSale 0",
                "closuresSinceLastSale 0",
                "qtySold 9",
            }, fieldsOf(answer[2], "d1 "))
            assert.same({
                "floor 15000",
                "floorQty 1",
                "listed 58",
                "qtySum 58",
                "p05 15900",
                "seenFloor -",
                "seenQty -",
                "daysSinceListed 0",
            }, fieldsOf(answer[6], "live "))
            -- The value, evidence and position groups, flat as the record holds them.
            assert.same({
                "value7 7030",
                "soldMedian14 -",
                "samples7 188",
                "trendBp 55934",
                "soldCount 32",
                "expiredCount 4",
                "supplyDays 25",
                "low30 100",
                "lowRank30d 8000",
                "vendorPrice 1250",
            }, fieldsOf(answer[7], ""))
            assert.equals("lows 12: 2 1 3 36 120 128 131 127 130 126 133 129", answer[8])
            assert.same({ "cost -", "profit -", "expectedProfit -" }, fieldsOf(answer[9], "craft "))
            assert.same({ "rank -", "scoreBp -", "discountBp -" }, fieldsOf(answer[10], "deal "))
            -- Four spans, five record lines, the derived values: eleven lines.
            assert.equals(11, #answer)
            assert.same({
                "reference 7030",
                "valueSource listed",
                "saleRate14 0.8889",
                "saleRate180d 0.8889",
                "daysSinceLastSale 0",
                "closuresSinceLastSale 0",
                "soldPerDay 2.2857142857143",
                "atVendorFloor false",
                "supplyUnbounded false",
            }, fieldsOf(answer[11], "derived "))
        end)

        it("accepts an item link and an item string", function()
            assert.same({ "25", "item" }, { decoded(LINK) })
            assert.same({ "25", "item" }, { decoded("item:25") })
        end)

        it("reads the item record before the commodity one for an id in both tables", function()
            -- The item record first, though the commodity's is the fuller one: its
            -- live group holds the date of its last listing and nothing else.
            assert.same({ "194784", "item" }, { decoded("194784") })
            assert.equals(
                "live floor - · floorQty - · listed - · qtySum - · p05 - · seenFloor - · seenQty - · daysSinceListed 3",
                lines[3]
            )
        end)

        it("reads a battle pet by species", function()
            assert.same({ "2546", "pet" }, { decoded("battlepet:2546") })
            assert.equals(
                "live floor 2500000 · floorQty 1 · listed 2 · qtySum 2 · p05 - · seenFloor - · seenQty - · daysSinceListed 0",
                lines[4]
            )
        end)

        it("says when the primary market has no record for it", function()
            assert.same({ TITLE .. " " .. WARN .. "no record for 123456 in " .. TARGET .. RESET }, run("decode 123456"))
        end)

        it("asks for an item, and refuses what is not one", function()
            assert.same({ WARN .. "usage: /amdb decode <item>" .. RESET }, run("decode"))
            assert.same({ WARN .. "not an item or a battle pet: banana" .. RESET }, run("decode banana"))
        end)

        it("says what to do without a feed", function()
            withoutFeed()
            assert.same({ BAD .. NO_DATA .. RESET }, run("decode 25"))
        end)
    end)
end)
