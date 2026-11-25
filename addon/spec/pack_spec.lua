-- Core/pack.lua (addon/SPEC.md, section 5.4): the alphabet, the text encoding,
-- and a reader's varints, zigzag values, flags and refusals.
--
-- The vectors are the Rust implementation's own (crates/infra/src/pack.rs), so
-- the two cannot drift apart unnoticed. Byte streams are rendered with the
-- spec's own encoder, never with `Pack.toText`, which is under test here.

local Loader = require("spec.helpers.loader")
local Encode = require("spec.helpers.encode")

local A = Loader.load(Loader.namespace(), "Core/pack.lua")
local Pack = A.Core.Pack

local ALPHABET = Pack.ALPHABET
local MAX_INTEGER = Pack.MAX_INTEGER

local OVERFLOW = "varint is longer than ten bytes or above 2^53 - 1"
local PADDING = "record padding is not at most four zero bits"

---@param actual integer[]
---@param expected integer[]
---@param message string|nil
local function sameBytes(actual, expected, message)
    assert.equals(#actual, #expected, message)
    for index = 1, #expected do
        assert.equals(actual[index], expected[index], message)
    end
end

--- Reads a raw byte stream with a fresh reader, through `read`.
---@param bytes integer[]
---@param read fun(reader: AmdbReader): any
---@return boolean ok false when the reader raised
---@return any value the method's result, or the message raised
local function attempt(bytes, read)
    return pcall(read, Pack.fromBytes(bytes))
end

---@param bytes integer[]
---@param read fun(reader: AmdbReader): any
---@param expected string the message the reader must raise
local function raises(bytes, read, expected)
    local ok, message = attempt(bytes, read)
    assert.is_false(ok)
    assert.equals(expected, message)
end

---@return fun(reader: AmdbReader): integer
local function readVarint()
    return function(reader)
        return reader:varint("test")
    end
end

---@return fun(reader: AmdbReader): integer
local function readZigzag()
    return function(reader)
        return reader:zigzag("test")
    end
end

describe("Pack.ALPHABET", function()
    it("has the 64 documented characters, in symbol order", function()
        assert.equals(64, #ALPHABET)
        assert.equals("0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz+.", ALPHABET)
        for symbol = 0, 63 do
            local char = ALPHABET:sub(symbol + 1, symbol + 1)
            -- Three bytes are four symbols; the first and the last are the easiest to place.
            assert.equals("000" .. char, Encode.toText({ 0, 0, symbol }))
            assert.equals(char .. "000", Encode.toText({ symbol * 4, 0, 0 }))
            sameBytes(Pack.fromText("000" .. char), { 0, 0, symbol })
            sameBytes(Pack.fromText(char .. "000"), { symbol * 4, 0, 0 })
        end
    end)

    it("contains no character a Lua string or a carrier file would have to escape", function()
        for index = 1, #ALPHABET do
            local byte = ALPHABET:byte(index)
            local digit = byte >= 48 and byte <= 57
            local upper = byte >= 65 and byte <= 90
            local lower = byte >= 97 and byte <= 122
            assert.is_true(digit or upper or lower or byte == 43 or byte == 46)
        end
        for _, forbidden in ipairs({ "\"", "\\", "[", "]", " ", "\n", "\t" }) do
            assert.is_nil(ALPHABET:find(forbidden, 1, true))
        end
    end)
end)

describe("Pack.toText and Pack.fromText", function()
    it("match the vectors the Rust implementation is pinned to", function()
        local cases = {
            { bytes = {}, text = "" },
            { bytes = { 0x00 }, text = "00" },
            { bytes = { 0x01 }, text = "0G" },
            { bytes = { 0x80 }, text = "W0" },
            { bytes = { 0xFF, 0x00, 0x01 }, text = ".m01" },
            { bytes = { 0x01, 0x02, 0x03 }, text = "0G83" },
            { bytes = { 0xFF, 0xFF, 0xFF, 0xFF }, text = ".....m" },
            { bytes = { 0x00, 0x00, 0x00 }, text = "0000" },
            { bytes = { 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 }, text = "0G00000000000000" },
            { bytes = { 0x80, 0x80, 0x80, 0x80, 0x10 }, text = "W820W10" },
            { bytes = { 0xAC, 0x02 }, text = "h08" },
        }
        for _, case in ipairs(cases) do
            assert.equals(case.text, Pack.toText(case.bytes))
            sameBytes(Pack.fromText(case.text), case.bytes, case.text)
        end
    end)

    it("are inverses for every length up to 64 bytes", function()
        for length = 0, 64 do
            -- 37 is coprime with 256, so no byte value is favoured.
            local bytes = {}
            for index = 1, length do
                bytes[index] = (index - 1) * 37 % 256
            end
            local text = Pack.toText(bytes)
            assert.equals(math.ceil(length * 8 / 6), #text)
            assert.equals(text, Encode.toText(bytes))
            sameBytes(Pack.fromText(text), bytes)
        end
    end)

    it("use the whole alphabet and nothing else", function()
        local bytes = {}
        for step = 1, 1024 do
            bytes[step] = step * 37 % 256
        end
        local text = Pack.toText(bytes)
        local _, foreign = text:gsub("[^" .. ALPHABET .. "]", "")
        assert.equals(0, foreign)
        for symbol = 0, 63 do
            assert.is_not_nil(text:find(ALPHABET:sub(symbol + 1, symbol + 1), 1, true))
        end
    end)

    it("refuse a character outside the alphabet, naming its byte and offset", function()
        sameBytes(Pack.fromText("0Az9Qm+."), { 0x00, 0xAF, 0x49, 0x6B, 0x0F, 0xBF })
        local cases = {
            { text = "0Az-", message = "character 0x2d at offset 3 is not a record symbol" },
            { text = "a-b", message = "character 0x2d at offset 1 is not a record symbol" },
            { text = "a b", message = "character 0x20 at offset 1 is not a record symbol" },
            { text = "a\"", message = "character 0x22 at offset 1 is not a record symbol" },
            { text = "a\\", message = "character 0x5c at offset 1 is not a record symbol" },
            { text = "a\n", message = "character 0x0a at offset 1 is not a record symbol" },
            { text = "a\t", message = "character 0x09 at offset 1 is not a record symbol" },
            { text = "\127", message = "character 0x7f at offset 0 is not a record symbol" },
            { text = "\0", message = "character 0x00 at offset 0 is not a record symbol" },
            { text = "\255", message = "character 0xff at offset 0 is not a record symbol" },
            -- A complete word followed by a stray character: the offset counts from zero.
            { text = ("0"):rep(27) .. "-", message = "character 0x2d at offset 27 is not a record symbol" },
            -- Multibyte characters: the first byte is already outside the alphabet.
            { text = "a\195\169", message = "character 0xc3 at offset 1 is not a record symbol" },
            { text = "a\240\159\166\160", message = "character 0xf0 at offset 1 is not a record symbol" },
        }
        for _, case in ipairs(cases) do
            local ok, message = pcall(Pack.fromText, case.text)
            assert.is_false(ok)
            assert.equals(case.message, message)
        end
    end)

    it("follow the contract's padding rule", function()
        local refused =
            { "0", "1", "A", ".", "01", "0F", "0Z", "0z", "0+", "001", "003", "007", "00B", "00000", "0G000", "W0000" }
        for _, text in ipairs(refused) do
            local ok, message = pcall(Pack.fromText, text)
            assert.is_false(ok)
            assert.equals(PADDING, message)
        end
        -- Two symbols are one byte plus four padding bits.
        sameBytes(Pack.fromText("00"), { 0x00 })
        sameBytes(Pack.fromText("0G"), { 0x01 })
        -- Three symbols are two bytes plus two padding bits.
        sameBytes(Pack.fromText("000"), { 0x00, 0x00 })
        -- Four symbols are exactly three bytes: nothing to pad.
        sameBytes(Pack.fromText("001."), { 0x00, 0x00, 0x7F })
    end)

    it("read back every padded length and refuse a set padding bit", function()
        for length = 1, 24 do
            local bytes = {}
            for index = 1, length do
                bytes[index] = 0x00
            end
            local text = Pack.toText(bytes)
            local padded = #text * 6 - length * 8
            assert.is_true(padded == 0 or padded == 2 or padded == 4)
            sameBytes(Pack.fromText(text), bytes)
            if padded > 0 then
                -- All-zero bytes end in a zero symbol whose lowest bits are the padding.
                assert.equals("0", text:sub(#text))
                local corrupted = text:sub(1, #text - 1) .. "1"
                local ok, message = pcall(Pack.fromText, corrupted)
                assert.is_false(ok)
                assert.equals(PADDING, message)
            end
        end
    end)

    it("refuse text that is not a string, instead of crashing", function()
        for _, value in ipairs({ 12345, true, {} }) do
            local ok, message = pcall(Pack.fromText, value)
            assert.is_false(ok)
            assert.equals("record text is " .. type(value) .. ", not a string", message)
        end
        local ok, message = pcall(Pack.fromText, nil)
        assert.is_false(ok)
        assert.equals("record text is nil, not a string", message)
    end)
end)

describe("AmdbReader: varint", function()
    it("reads the documented byte layout", function()
        local cases = {
            { bytes = { 0x00 }, value = 0 },
            { bytes = { 0x7F }, value = 127 },
            { bytes = { 0x80, 0x01 }, value = 128 },
            { bytes = { 0xAC, 0x02 }, value = 300 },
            { bytes = { 0xFF, 0xFF, 0xFF, 0xFF, 0x0F }, value = 4294967295 },
        }
        for _, case in ipairs(cases) do
            local ok, value = attempt(case.bytes, readVarint())
            assert.is_true(ok)
            assert.equals(case.value, value)
        end
    end)

    it("round-trips the boundary values", function()
        local values = {
            0,
            1,
            127,
            128,
            16383,
            16384,
            2097152,
            268435456,
            4294967296,
            34359738368,
            4398046511104,
            562949953421312,
            MAX_INTEGER,
        }
        for _, value in ipairs(values) do
            local ok, read = attempt(Encode.varint(value), readVarint())
            assert.is_true(ok)
            assert.equals(value, read)
        end
    end)

    it("refuses a non-minimal varint", function()
        local message = "`test` is a non-minimal varint"
        local cases = {
            { 0x80, 0x00 },
            { 0xFF, 0x00 },
            { 0x80, 0x80, 0x00 },
            { 0x81, 0x80, 0x00 },
            { 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00 },
        }
        for _, bytes in ipairs(cases) do
            raises(bytes, readVarint(), message)
            raises(bytes, readZigzag(), message)
        end
        -- The minimal forms of the same values; a lone zero byte is the minimal zero.
        local minimal = {
            { bytes = { 0x00 }, value = 0 },
            { bytes = { 0x7F }, value = 127 },
            { bytes = { 0x80, 0x01 }, value = 128 },
            { bytes = { 0x81, 0x80, 0x01 }, value = 16385 },
        }
        for _, case in ipairs(minimal) do
            local ok, value = attempt(case.bytes, readVarint())
            assert.is_true(ok)
            assert.equals(case.value, value)
        end
    end)

    it("refuses a varint of more than ten bytes, or a tenth group above one", function()
        local eleven = { 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00 }
        local twelve = { 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01 }
        -- Eleven continuation bytes: longer than any u64 varint.
        raises(eleven, readVarint(), OVERFLOW)
        raises(twelve, readVarint(), OVERFLOW)
        -- The tenth byte holds one bit only.
        raises({ 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x02 }, readVarint(), OVERFLOW)
        raises({ 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F }, readVarint(), OVERFLOW)
        raises({ 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x02 }, readZigzag(), OVERFLOW)
        -- Ten bytes with a tenth group of one are a 10-byte varint (2^63): a
        -- code that large is already above the contract's integer ceiling.
        local ten = { 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01 }
        raises(ten, readVarint(), OVERFLOW)
        raises(ten, readZigzag(), OVERFLOW)
    end)

    it("refuses an integer above 2^53 - 1", function()
        local ok, ceiling = attempt(Encode.varint(MAX_INTEGER), readVarint())
        assert.is_true(ok)
        assert.equals(MAX_INTEGER, ceiling)
        for _, value in ipairs({ MAX_INTEGER + 1, 18014398509481984, 9223372036854775808 }) do
            raises(Encode.varint(value), readVarint(), OVERFLOW)
        end
    end)

    it("reports a stream that ends inside the value", function()
        local message = "record ended inside `test`"
        raises({}, readVarint(), message)
        raises({ 0x80 }, readVarint(), message)
        raises({ 0x80 }, readZigzag(), message)
        -- Ten continuation bytes and nothing more: the eleventh byte is missing.
        raises({ 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80 }, readVarint(), message)
    end)
end)

describe("AmdbReader: zigzag", function()
    it("follows the contract order 0, -1, 1, -2, 2", function()
        -- A code with the sign bit decodes to -(magnitude + 1), never to -magnitude.
        local cases = {
            { value = 0, code = 0 },
            { value = -1, code = 1 },
            { value = 1, code = 2 },
            { value = -2, code = 3 },
            { value = 2, code = 4 },
            { value = -3, code = 5 },
            { value = -64, code = 127 },
            { value = 64, code = 128 },
            { value = -8192, code = 16383 },
        }
        for _, case in ipairs(cases) do
            assert.equals(case.code, Encode.zigzagCode(case.value))
            local ok, read = attempt(Encode.varint(case.code), readZigzag())
            assert.is_true(ok)
            assert.equals(case.value, read)
        end
    end)

    it("round-trips both signs at the edges", function()
        local values = {
            0,
            1,
            -1,
            127,
            -128,
            4294967296,
            -4294967296,
            MAX_INTEGER,
            300,
            -300,
            65535,
            -65536,
            -MAX_INTEGER + 1,
        }
        for _, value in ipairs(values) do
            local ok, read = attempt(Encode.zigzag(value), readZigzag())
            assert.is_true(ok)
            assert.equals(value, read)
        end
        -- The most negative integer, -(2^53 - 1), has the code 2^54 - 3, which a
        -- double cannot hold: its bytes are written out by hand.
        local ok, lowest = attempt({ 0xFD, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x1F }, readZigzag())
        assert.is_true(ok)
        assert.equals(-MAX_INTEGER, lowest)
    end)

    it("refuses an integer beyond 2^53 - 1 in magnitude, either sign", function()
        local beyond = MAX_INTEGER + 1
        for _, value in ipairs({ beyond, -beyond, 2 * beyond, -2 * beyond }) do
            raises(Encode.zigzag(value), readZigzag(), OVERFLOW)
        end
        -- Raw codes at the ceiling: 2^54 - 2 is the largest accepted (it is
        -- +MAX_INTEGER), 2^54 - 3 the most negative one (-(MAX_INTEGER)); both
        -- bytes are written by hand because a double cannot hold the odd code.
        local highest, plus = attempt(Encode.varint(18014398509481982), readZigzag())
        assert.is_true(highest)
        assert.equals(MAX_INTEGER, plus)
        local lowest, minus = attempt({ 0xFD, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x1F }, readZigzag())
        assert.is_true(lowest)
        assert.equals(-MAX_INTEGER, minus)
        -- 2^54 - 1 is -(2^53), one below the contract; 2^54 is +2^53, one above it.
        raises({ 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x1F }, readZigzag(), OVERFLOW)
        raises(Encode.varint(18014398509481984), readZigzag(), OVERFLOW)
        raises(Encode.varint(9223372036854775808), readZigzag(), OVERFLOW)
    end)
end)

describe("AmdbReader: flag, byte, uint and finish", function()
    it("reports unread bytes, then reads them one by one", function()
        -- 32767 fits a u16; the next varint is 65536, which does not.
        local small = Pack.fromBytes({ 0xFF, 0xFF, 0x01 })
        assert.equals(32767, small:uint("small", 65535))
        small:finish()

        local trailing = Pack.fromBytes({ 0xFF, 0xFF, 0x01, 0x80, 0x80, 0x04 })
        assert.equals(32767, trailing:varint("first"))
        local finished, why = pcall(trailing.finish, trailing)
        assert.is_false(finished)
        assert.equals("3 byte(s) follow the last field", why)
        assert.equals(0x80, trailing:byte("late"))
        assert.equals(0x80, trailing:byte("late"))
        assert.equals(0x04, trailing:byte("late"))
        local read, late = pcall(trailing.byte, trailing, "late")
        assert.is_false(read)
        assert.equals("record ended inside `late`", late)

        do
            local counted = Pack.fromBytes({ 0x00, 0x00, 0x00 })
            assert.equals(0, counted:byte("one"))
            local closed, remaining = pcall(counted.finish, counted)
            assert.is_false(closed)
            assert.equals("2 byte(s) follow the last field", remaining)
            assert.equals(0, counted:byte("two"))
            local again, left = pcall(counted.finish, counted)
            assert.is_false(again)
            assert.equals("1 byte(s) follow the last field", left)
            assert.equals(0, counted:byte("three"))
            counted:finish()
        end
    end)

    it("refuses a flag bit with no meaning", function()
        local accepted, flags = attempt({ 0x01 }, function(reader)
            return reader:flag("known", 0x01)
        end)
        assert.is_true(accepted)
        assert.equals(0x01, flags)
        local cases = {
            { bytes = { 0x02 }, field = "unknown", allowed = 0x01, message = "`unknown` sets undefined bits 0x2" },
            { bytes = { 0x01 }, field = "nothing", allowed = 0x00, message = "`nothing` sets undefined bits 0x1" },
            { bytes = { 0x80, 0x01 }, field = "flags", allowed = 0x7F, message = "`flags` sets undefined bits 0x80" },
            { bytes = { 0x10 }, field = "spans", allowed = 0x0F, message = "`spans` sets undefined bits 0x10" },
            {
                bytes = { 0xFF, 0x01 },
                field = "span.sflags",
                allowed = 0x1F,
                message = "`span.sflags` sets undefined bits 0xe0",
            },
            {
                bytes = { 0x0A },
                field = "live.lflags",
                allowed = 0x07,
                message = "`live.lflags` sets undefined bits 0x8",
            },
            {
                bytes = Encode.varint(4503599627370496),
                field = "flags",
                allowed = 0x7F,
                message = "`flags` sets undefined bits 0x10000000000000",
            },
            {
                bytes = Encode.varint(MAX_INTEGER),
                field = "flags",
                allowed = 0x7F,
                message = "`flags` sets undefined bits 0x1fffffffffff80",
            },
        }
        for _, case in ipairs(cases) do
            local ok, message = attempt(case.bytes, function(reader)
                return reader:flag(case.field, case.allowed)
            end)
            assert.is_false(ok)
            assert.equals(case.message, message)
        end
    end)

    it("refuses a value above its field", function()
        local cases = {
            {
                value = 65536,
                field = "evidence.supplyDays",
                maximum = 65535,
                message = "`evidence.supplyDays` holds 65536, out of range",
            },
            {
                value = 1099511627776,
                field = "position.lows",
                maximum = 30,
                message = "`position.lows` holds 1099511627776, out of range",
            },
            {
                value = MAX_INTEGER,
                field = "value.value7",
                maximum = 30,
                message = "`value.value7` holds 9007199254740991, out of range",
            },
        }
        for _, case in ipairs(cases) do
            local ok, message = attempt(Encode.varint(case.value), function(reader)
                return reader:uint(case.field, case.maximum)
            end)
            assert.is_false(ok)
            assert.equals(case.message, message)
        end
        local ok, value = attempt(Encode.varint(65535), function(reader)
            return reader:uint("evidence.supplyDays", 65535)
        end)
        assert.is_true(ok)
        assert.equals(65535, value)
    end)

    it("reads the empty text as an empty stream", function()
        sameBytes(Pack.fromText(""), {})
        raises({}, readVarint(), "record ended inside `test`")
        Pack.new(""):finish()
    end)
end)

describe("Pack.hasBit", function()
    it("tests one power-of-two bit of a flag word", function()
        assert.is_false(Pack.hasBit(0x00, 0x01))
        assert.is_true(Pack.hasBit(0x01, 0x01))
        assert.is_false(Pack.hasBit(0x02, 0x01))
        assert.is_true(Pack.hasBit(0x7F, 0x40))
        assert.is_false(Pack.hasBit(0x3F, 0x40))
        assert.is_true(Pack.hasBit(MAX_INTEGER, 1))
        assert.is_true(Pack.hasBit(MAX_INTEGER, 4503599627370496))
        assert.is_false(Pack.hasBit(MAX_INTEGER, 9007199254740992))
    end)
end)

describe("refusal messages", function()
    it("are exactly the Rust decoder's words", function()
        local function refusal(fn, ...)
            local ok, message = pcall(fn, ...)
            assert.is_false(ok)
            return message
        end

        assert.equals("character 0x2d at offset 3 is not a record symbol", refusal(Pack.fromText, "0Az-"))
        assert.equals(
            "record ended inside `live.floor`",
            refusal(function()
                return Pack.new(""):varint("live.floor")
            end)
        )
        assert.equals(PADDING, refusal(Pack.fromText, "0"))
        assert.equals(
            "1 byte(s) follow the last field",
            refusal(function()
                return Pack.fromBytes({ 0x00 }):finish()
            end)
        )
        assert.equals(
            OVERFLOW,
            refusal(function()
                return Pack.fromBytes(Encode.varint(MAX_INTEGER + 1)):varint("vendor.vendorPrice")
            end)
        )
        assert.equals(
            "`live.floor` is a non-minimal varint",
            refusal(function()
                return Pack.fromBytes({ 0x80, 0x00 }):varint("live.floor")
            end)
        )
        assert.equals(
            "`flags` sets undefined bits 0x80",
            refusal(function()
                return Pack.fromBytes({ 0x80, 0x01 }):flag("flags", 0x7F)
            end)
        )
        assert.equals(
            "`supplyDays` holds 65536, out of range",
            refusal(function()
                return Pack.fromBytes(Encode.varint(65536)):uint("supplyDays", 65535)
            end)
        )
        assert.equals("`live.lflags` sets undefined bits 0x2", Pack.unknownFlagMessage("live.lflags", 0x02))
    end)
end)
