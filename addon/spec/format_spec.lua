-- Specs for Core/format.lua: table tests over every output string of the
-- formatter and the house colors (SPEC 7.5.4, 7.6, section 10 "format").

local Loader = require("spec.helpers.loader")

local A = Loader.namespace()
Loader.load(A, "Core/format.lua")
local Format = A.Core.Format

local HOUR = 3600
local DAY = 24 * HOUR

--- Asserts `fn(input)`; a nil expected means the formatter answers nil.
local function behaves(fn, input, expected)
    local result = fn(input)
    if expected == nil then
        assert.is_nil(result)
    else
        assert.equals(expected, result)
    end
end

--- One `it` per `{ input, expected }` row. Each row is captured for its own
--- test: a Lua 5.1 loop variable is shared by every closure made in the loop.
local function tableTest(name, fn, rows)
    describe(name, function()
        for i = 1, #rows do
            local row = rows[i]
            it("answers " .. tostring(row[2]) .. " for " .. tostring(row[1]), function()
                behaves(fn, row[1], row[2])
            end)
        end
    end)
end

local NIL_INPUTS = { { nil }, { "7" }, { true }, { {} }, { 0 / 0 }, { math.huge }, { -math.huge } }

describe("Format", function()
    it("registers into the namespace and returns its table", function()
        local namespace = Loader.namespace()
        local chunk = assert(loadfile(Loader.ROOT .. "/Core/format.lua"))
        local module = chunk(Loader.ADDON, namespace)
        assert.equals(namespace.Core.Format, module)
    end)

    it("carries the house colors as WoW escape strings", function()
        assert.same({
            brand = "|cffff7d0a",
            muted = "|cff99a1b3",
            dim = "|cff666e80",
            good = "|cff3dc773",
            warn = "|cffffd100",
            hot = "|cffff8040",
            bad = "|cffff4040",
        }, Format.COLORS)
        assert.equals("|r", Format.RESET)
        assert.equals("|cffff7d0aAuctiMogger Database|r", Format.PREFIX)
    end)

    it("derives each color channel as round(v * 255) of SPEC 7.5.4", function()
        local function hex(r, g, b)
            return ("|cff%02x%02x%02x"):format(
                math.floor(r * 255 + 0.5),
                math.floor(g * 255 + 0.5),
                math.floor(b * 255 + 0.5)
            )
        end
        assert.equals(hex(1, 0.49, 0.04), Format.COLORS.brand)
        assert.equals(hex(0.60, 0.63, 0.70), Format.COLORS.muted)
        assert.equals(hex(0.40, 0.43, 0.50), Format.COLORS.dim)
        assert.equals(hex(0.24, 0.78, 0.45), Format.COLORS.good)
        assert.equals(hex(1.00, 0.82, 0.00), Format.COLORS.warn)
        assert.equals(hex(1.00, 0.50, 0.25), Format.COLORS.hot)
        assert.equals(hex(1.00, 0.25, 0.25), Format.COLORS.bad)
    end)
end)

describe("Format.colorize", function()
    local rows = {
        { "good", "380g", "|cff3dc773380g|r" },
        { "brand", "AuctiMogger Database", "|cffff7d0aAuctiMogger Database|r" },
        { "muted", 42, "|cff99a1b342|r" },
        { "dim", "", "|cff666e80|r" },
        { "bad", "-5g", "|cffff4040-5g|r" },
        { "hot", "12h", "|cffff804012h|r" },
        { "warn", "80%", "|cffffd10080%|r" },
        { "stale", "2d", "2d" },
        { "nope", "text", "text" },
        { "", "text", "text" },
        { "good", nil, "" },
    }
    for i = 1, #rows do
        local row = rows[i]
        it("answers " .. tostring(row[3]) .. " for " .. tostring(row[1]) .. ", " .. tostring(row[2]), function()
            assert.equals(row[3], Format.colorize(row[1], row[2]))
        end)
    end
end)

tableTest("Format.money", Format.money, {
    { 3805000, "380g 50s" },
    { 2100000, "210g" },
    { 125000, "12g 50s" },
    { 5000, "50s" },
    { 7, "7c" },
    { 0, "0c" },
    { -50000, "-5g" },
    { 1234567, "123g 45s 67c" },
    { 123456700, "12345g 67s" },
    { 100, "1s" },
    { 10000, "1g" },
    { 10001, "1g 1c" },
    { 10100, "1g 1s" },
    { 9999, "99s 99c" },
    { -125000, "-12g 50s" },
    { -7, "-7c" },
    { 59.6, "60c" },
    { 99.5, "1s" },
    { 0.4, "0c" },
    { -0.4, "0c" },
    { 9999999999, "999999g 99s 99c" },
    unpack(NIL_INPUTS),
})

tableTest("Format.moneySigned", Format.moneySigned, {
    { 1900000, "+190g" },
    { -1900000, "-190g" },
    { 0, "0g" },
    { 7, "+7c" },
    { -7, "-7c" },
    { 5000, "+50s" },
    { -125000, "-12g 50s" },
    { 1234567, "+123g 45s 67c" },
    { 0.4, "0g" },
    { -0.4, "0g" },
    unpack(NIL_INPUTS),
})

tableTest("Format.percent", Format.percent, {
    { 0.36, "36%" },
    { 0.3617, "36%" },
    { 0.5, "50%" },
    { 1, "100%" },
    { 0, "0%" },
    { -0.03, "-3%" },
    { 1.5, "150%" },
    { 0.999, "100%" },
    { 0.005, "1%" },
    { 0.004, "0%" },
    { -0.004, "0%" },
    unpack(NIL_INPUTS),
})

tableTest("Format.percentBp", Format.percentBp, {
    { 8000, "80%" },
    { 3800, "38%" },
    { 0, "0%" },
    { 10000, "100%" },
    { 9250, "93%" },
    { 55934, "559%" },
    { -300, "-3%" },
    unpack(NIL_INPUTS),
})

tableTest("Format.signedPercentBp", Format.signedPercentBp, {
    { 1000, "+10%" },
    { -300, "-3%" },
    { 0, "0%" },
    { 8000, "+80%" },
    { -1250, "-13%" },
    { 4, "0%" },
    { -4, "0%" },
    { 1, "0%" },
    unpack(NIL_INPUTS),
})

tableTest("Format.count", Format.count, {
    { 1234, "1,234" },
    { 618, "618" },
    { 0, "0" },
    { 999, "999" },
    { 1000, "1,000" },
    { 1000000, "1,000,000" },
    { 164000000, "164,000,000" },
    { 1234567890, "1,234,567,890" },
    { 1234.6, "1,235" },
    { -1234, "-1,234" },
    unpack(NIL_INPUTS),
})

tableTest("Format.rate", Format.rate, {
    { 2.4, "2.4" },
    { 3, "3" },
    { 3.0, "3" },
    { 2, "2" },
    { 0.5, "0.5" },
    { 9.96, "10" },
    { 0, "0" },
    { 0.04, "0" },
    { -0.04, "0" },
    { 2.26, "2.3" },
    { 0.71, "0.7" },
    unpack(NIL_INPUTS),
})

tableTest("Format.age", Format.age, {
    { 1500, "25m" },
    { 0, "0m" },
    { 59, "0m" },
    { 60, "1m" },
    { 3599, "59m" },
    { 1500.9, "25m" },
    { HOUR, "1h 00m" },
    { 3900, "1h 05m" },
    { 7380, "2h 03m" },
    { DAY - 1, "23h 59m" },
    { DAY, "1d" },
    { 90000, "1d" },
    { 2 * DAY, "2d" },
    { 3 * DAY, "3d" },
    { nil, "0m" },
    { -5, "0m" },
    { -1500, "0m" },
    { "1500", nil },
    { true, nil },
    { {}, nil },
})

tableTest("Format.days", Format.days, {
    { 39, "39d" },
    { 0, "0d" },
    { 1, "1d" },
    { 65534, "65534d" },
    unpack(NIL_INPUTS),
})

tableTest("Format.supplyDays", Format.supplyDays, {
    { nil, "∞" },
    { 65535, "∞" },
    { 39, "39d" },
    { 0, "0d" },
    { 1, "1d" },
    { 65534, "65534d" },
    { "39", nil },
    { true, nil },
    { {}, nil },
    { 0 / 0, nil },
    { math.huge, nil },
})

tableTest("Format.ageTier", Format.ageTier, {
    { 0, "muted" },
    { nil, "muted" },
    { -100, "muted" },
    { 7199, "muted" },
    { 2 * HOUR, "warn" },
    { 12 * HOUR - 1, "warn" },
    { 12 * HOUR, "hot" },
    { DAY - 1, "hot" },
    { DAY, "stale" },
    { 90000, "stale" },
})

tableTest("Format.ageColor", Format.ageColor, {
    { 0, "muted" },
    { nil, "muted" },
    { -100, "muted" },
    { 2 * HOUR, "warn" },
    { 12 * HOUR, "hot" },
    { DAY - 1, "hot" },
    { DAY, "bad" },
    { 90000, "bad" },
})

tableTest("Format.rateTier", Format.rateTier, {
    { 0.5, "good" },
    { 1, "good" },
    { 0.8889, "good" },
    { 0.4999, "warn" },
    { 0.2, "warn" },
    { 0.3617, "warn" },
    { 0.1999, "bad" },
    { 0, "bad" },
    { nil, "bad" },
    { 0 / 0, "bad" },
})

tableTest("Format.trendTier", Format.trendTier, {
    { 1000, "good" },
    { 1, "good" },
    { -300, "bad" },
    { -1, "bad" },
    { 0, "muted" },
    { nil, "muted" },
    { 0 / 0, "muted" },
})

describe("Format on the sample lines of SPEC 7.6", function()
    it("renders the numbers of the tooltip example", function()
        assert.equals("380g 50s", Format.money(3805000))
        assert.equals("1,234", Format.count(1234))
        assert.equals("618", Format.count(618))
        assert.equals("36%", Format.percent(34 / 94))
        assert.equals("31.6", Format.rate(31.6))
        assert.equals("39d", Format.supplyDays(39))
        assert.equals("2d", Format.days(2))
        assert.equals("350g", Format.money(3500000))
        assert.equals("80%", Format.percentBp(8000))
        -- The craft line, in the copper of the JSON twin of SPEC 5.10.
        assert.equals("2100g", Format.money(21000000))
        assert.equals("+1900g", Format.moneySigned(19000000))
        assert.equals("+684g", Format.moneySigned(6840000))
        assert.equals("38%", Format.percentBp(3800))
        assert.equals("0.71", ("%.2f"):format(7100 / 10000))
        assert.equals("-3%", Format.signedPercentBp(-300))
        assert.equals("+10%", Format.signedPercentBp(1000))
        assert.equals("-5%", Format.signedPercentBp(-500))
        -- The ages of the example's header and Snapshots line, 25 minutes after
        -- its auctions stamp.
        assert.equals("58m", Format.age(3500))
        assert.equals("25m", Format.age(25 * 60))
        assert.equals("1h 05m", Format.age(HOUR + 5 * 60))
        assert.equals("∞", Format.supplyDays(nil))
        assert.equals("∞", Format.supplyDays(65535))
    end)

    it("renders the numbers of the login status line of SPEC 7.11", function()
        assert.equals("25m", Format.age(1500))
        assert.equals("2d", Format.age(2 * DAY))
    end)
end)

describe("Format.setCoinIcons", function()
    after_each(function()
        -- Nothing of this leaks into the table tests above or below.
        Format.setCoinIcons(nil)
    end)

    it("is off by default: a price reads as text", function()
        assert.equals("380g 50s", Format.money(3805000))
        assert.equals("+1900g", Format.moneySigned(19000000))
    end)

    it("hands the renderer the whole, non-negative amount and keeps the sign in front", function()
        local seen = {}
        Format.setCoinIcons(function(copper)
            seen[#seen + 1] = copper
            return "<" .. copper .. ">"
        end)
        assert.equals("<3805000>", Format.money(3805000))
        -- Rounded like the text form, to the whole copper.
        assert.equals("<3805001>", Format.money(3805000.6))
        assert.equals("-<50000>", Format.money(-50000))
        assert.same({ 3805000, 3805001, 50000 }, seen)
        -- The signed form: the sign in front, and zero as the renderer draws it.
        assert.equals("+<1900000>", Format.moneySigned(1900000))
        assert.equals("-<1900000>", Format.moneySigned(-1900000))
        assert.equals("<0>", Format.moneySigned(0))
        assert.equals("<0>", Format.moneySigned(0.4))
        assert.equals("<0>", Format.money(0))
        -- Junk is still no price at all.
        assert.is_nil(Format.money(nil))
        assert.is_nil(Format.money("7"))
        assert.is_nil(Format.moneySigned(0 / 0))
    end)

    it("accepts a function only, and nil clears the renderer", function()
        Format.setCoinIcons("junk")
        assert.equals("1g", Format.money(10000))
        Format.setCoinIcons(function()
            return "<x>"
        end)
        assert.equals("<x>", Format.money(10000))
        Format.setCoinIcons(nil)
        assert.equals("1g", Format.money(10000))
    end)

    it("passes over a renderer that raises or answers anything but a string", function()
        Format.setCoinIcons(function(copper)
            if copper == 7 then
                error("no coins for seven", 0)
            end
            return copper == 8 and 42 or nil
        end)
        -- The text form stands in for that one call, and the next call is tried again.
        assert.equals("7c", Format.money(7))
        assert.equals("8c", Format.money(8))
        assert.equals("0c", Format.money(0))
        assert.equals("0g", Format.moneySigned(0))
    end)

    it("never hands a negative amount or a fraction to the renderer", function()
        local smallest = math.huge
        local widest = 0
        Format.setCoinIcons(function(copper)
            smallest = math.min(smallest, copper)
            widest = math.max(widest, copper)
            return tostring(copper)
        end)
        for _, copper in ipairs({ -9999999, -0.4, 0, 0.4, 0.6, 1, 9999999999, 12345.678 }) do
            Format.money(copper)
            Format.moneySigned(copper)
        end
        assert.equals(0, smallest)
        assert.equals(9999999999, widest)
    end)
end)
