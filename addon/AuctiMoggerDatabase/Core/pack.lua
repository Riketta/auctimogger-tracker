-- Packing primitives of the database feed's records (addon/SPEC.md, section 5.4).
--
-- A record is a byte stream of unsigned LEB128 varints (7 bits per byte, low
-- group first, continuation bit 0x80), with signed values zigzagged. The stream
-- travels as text: it is read as a big-endian bit string and cut into 6-bit
-- symbols, each one character of ALPHABET, the last symbol zero-padded on the
-- right. No record character needs escaping in a Lua string, so a record is one
-- printable word.
--
-- Contract of this module:
--   * `fromText(text)` and `toText(bytes)` are inverses on everything an
--     encoder writes; anything else is refused.
--   * `new(text)` hands out a reader over one record. Its methods read one
--     field at a time and raise `error(message, 0)` on the first malformed
--     field: a value is never wrapped and never partly read.
--   * The messages are the Rust reference decoder's word for word
--     (crates/infra/src/pack.rs, `PackError`), so both decoders log the same
--     line for the same bad record.
--   * Every integer the contract carries is below 2^53 in magnitude, where a
--     Lua 5.1 double is exact; anything larger is refused, never rounded.
--
-- Pure Lua: standard library only, no game global.

local _, A = ...
A.Core = A.Core or {}

---@class AmdbPack
---@field ALPHABET string the 64 record characters, in symbol order
---@field MAX_INTEGER integer the largest integer the contract carries (2^53 - 1)
local Pack = {}
A.Core.Pack = Pack

Pack.ALPHABET = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz+."
Pack.MAX_INTEGER = 9007199254740991

local ALPHABET = Pack.ALPHABET
local MAX_INTEGER = Pack.MAX_INTEGER

local OVERFLOW = "varint is longer than ten bytes or above 2^53 - 1"
local PADDING = "record padding is not at most four zero bits"

local HEX_DIGITS = { "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "a", "b", "c", "d", "e", "f" }
local DEC_DIGITS = { "0", "1", "2", "3", "4", "5", "6", "7", "8", "9" }

-- `POW2[n + 1]` is 2^n. Every accumulator below stays under 2^14, so the
-- divisions and remainders on it are exact.
local POW2 = { 1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192 }

-- SYMBOLS[byte] is the 6-bit symbol of one record character (nil for any other
-- byte); CHARS[symbol + 1] is the character of one symbol.
local SYMBOLS = {}
local CHARS = {}
for index = 1, #ALPHABET do
    local byte = string.byte(ALPHABET, index)
    SYMBOLS[byte] = index - 1
    CHARS[index] = string.char(byte)
end

local byteAt = string.byte

-- Lua 5.1 renders numbers with "%.14g", which turns an integer of 15 or more
-- digits into an exponent, and its "%d" is only as wide as the platform's
-- `long`. Rust prints the whole value, so integers in messages are formatted
-- by hand.

---@param value integer a non-negative integer below 2^53
---@param base integer how many distinct digits there are
---@param symbols string[] the digits, lowest first
---@return string text the digits, without a prefix and without leading zeros
local function digits(value, base, symbols)
    local out = {}
    local count = 0
    repeat
        local low = value % base
        count = count + 1
        out[count] = symbols[low + 1]
        value = math.floor(value / base)
    until value == 0
    -- The digits came out lowest first.
    for index = 1, math.floor(count / 2) do
        local mirror = count - index + 1
        out[index], out[mirror] = out[mirror], out[index]
    end
    return table.concat(out)
end

---@param field string the field being read
---@return string message
local function truncated(field)
    return "record ended inside `" .. field .. "`"
end

---@param field string the field being read
---@return string message
local function nonMinimal(field)
    return "`" .. field .. "` is a non-minimal varint"
end

---@param field string the field being read
---@param value integer the value that was read
---@return string message
local function outOfRange(field, value)
    return "`" .. field .. "` holds " .. digits(value, 10, DEC_DIGITS) .. ", out of range"
end

---@param offset integer the offending character's offset, from zero
---@param byte integer the offending byte
---@return string message
local function notASymbol(offset, byte)
    return "character 0x"
        .. HEX_DIGITS[math.floor(byte / 16) + 1]
        .. HEX_DIGITS[byte % 16 + 1]
        .. " at offset "
        .. offset
        .. " is not a record symbol"
end

---@class AmdbReader
---@field package bytes integer[] the record's byte stream, 1-based
---@field package length integer how many bytes the stream holds
---@field package pos integer index of the next unread byte
local Reader = {}
Reader.__index = Reader

----------------------------------------------------------------------------- text

---@param text string one record's text
---@return integer[] bytes the byte stream the text carries, 1-based
function Pack.fromText(text)
    if type(text) ~= "string" then
        error("record text is " .. type(text) .. ", not a string", 0)
    end
    local out = {}
    local count = 0
    -- `acc` holds the bits not yet turned into bytes: fewer than eight between
    -- symbols.
    local acc, nbits = 0, 0
    for offset = 0, #text - 1 do
        local byte = byteAt(text, offset + 1)
        local symbol = SYMBOLS[byte]
        if symbol == nil then
            error(notASymbol(offset, byte), 0)
        end
        acc = acc * 64 + symbol
        nbits = nbits + 6
        if nbits >= 8 then
            nbits = nbits - 8
            count = count + 1
            out[count] = math.floor(acc / POW2[nbits + 1])
            acc = acc % POW2[nbits + 1]
        end
    end
    -- The encoder pads a byte stream with at most four zero bits (8n bits
    -- become 6 * ceil(8n / 6) symbols), so a longer or non-zero tail was not
    -- written by an encoder: whatever decodes is exactly what the encoder
    -- would emit.
    if nbits > 4 or acc ~= 0 then
        error(PADDING, 0)
    end
    return out
end

---@param bytes integer[] a byte stream whose values are 0 to 255
---@return string text the record text, the last symbol zero-padded on the right
function Pack.toText(bytes)
    local out = {}
    local acc, nbits = 0, 0
    for index = 1, #bytes do
        acc = acc * 256 + bytes[index]
        nbits = nbits + 8
        while nbits >= 6 do
            nbits = nbits - 6
            out[#out + 1] = CHARS[math.floor(acc / POW2[nbits + 1]) % 64 + 1]
            acc = acc % POW2[nbits + 1]
        end
    end
    if nbits > 0 then
        out[#out + 1] = CHARS[acc * POW2[7 - nbits] + 1]
    end
    return table.concat(out)
end

-------------------------------------------------------------------------- bits

---@param flags integer a flag word
---@param bit integer the bit to test, a power of two
---@return boolean whether the bit is set
function Pack.hasBit(flags, bit)
    return math.floor(flags / bit) % 2 == 1
end

--- The message for a flag word that sets bits with no meaning. Also for a bit
--- that only means something together with another one, as `live`'s `qtySum`
--- does with `listed`.
---@param field string the flag word's field
---@param bits integer the bits without meaning
---@return string message
function Pack.unknownFlagMessage(field, bits)
    return "`" .. field .. "` sets undefined bits 0x" .. digits(bits, 16, HEX_DIGITS)
end

------------------------------------------------------------------------- reader

---@param bytes integer[] a record's byte stream, as `fromText` returns it
---@return AmdbReader reader positioned at the start of the stream
function Pack.fromBytes(bytes)
    return setmetatable({ bytes = bytes, length = #bytes, pos = 1 }, Reader)
end

---@param text string one record's text
---@return AmdbReader reader positioned at the start of the record
function Pack.new(text)
    return Pack.fromBytes(Pack.fromText(text))
end

---@param field string the field being read, for the error message
---@return integer value the next raw byte, 0 to 255
function Reader:byte(field)
    local pos = self.pos
    local value = self.bytes[pos]
    if value == nil then
        error(truncated(field), 0)
    end
    self.pos = pos + 1
    return value
end

--- Reads one unsigned varint, in the integer range the contract carries.
---
--- This is the Rust `Reader::uint::<u64>`: a code above `MAX_INTEGER` is
--- refused here, because no field of the format reads a wider one.
---@param field string the field being read, for the error message
---@return integer value 0 to `MAX_INTEGER`
function Reader:varint(field)
    local value, shift, count = 0, 1, 0
    local group
    while true do
        local byte = self:byte(field)
        group = byte % 128
        -- Ten bytes at most hold a u64, and the tenth holds one bit: a longer
        -- varint is malformed, never a wrapping shift.
        if count >= 10 or (count == 9 and group > 1) then
            error(OVERFLOW, 0)
        end
        value = value + group * shift
        count = count + 1
        if byte < 128 then
            break
        end
        shift = shift * 128
    end
    -- A last group of zero after continuation bytes encodes nothing.
    if count > 1 and group == 0 then
        error(nonMinimal(field), 0)
    end
    if value > MAX_INTEGER then
        error(OVERFLOW, 0)
    end
    return value
end

--- Reads one unsigned varint that must fit the field it is stored in.
---@param field string the field being read, for the error message
---@param maximum integer the largest value the field holds
---@return integer value 0 to `maximum`
function Reader:uint(field, maximum)
    local value = self:varint(field)
    if value > maximum then
        error(outOfRange(field, value), 0)
    end
    return value
end

--- Reads one zigzagged value: the sign in the low bit, the magnitude above it.
---@param field string the field being read, for the error message
---@return integer value -(2^53 - 1) to 2^53 - 1
function Reader:zigzag(field)
    local magnitude, sign, shift, count = 0, 0, 1, 0
    local group
    while true do
        local byte = self:byte(field)
        group = byte % 128
        if count >= 10 or (count == 9 and group > 1) then
            error(OVERFLOW, 0)
        end
        if count == 0 then
            -- The code is 2 * magnitude + sign. Only the magnitude is
            -- accumulated, so nothing here is the code itself, which is twice
            -- the integer the contract bounds. The second byte's groups sit at
            -- 7i - 1 bits, one bit lower than in an unsigned varint.
            magnitude = math.floor(group / 2)
            sign = group - magnitude * 2
            shift = 64
        else
            magnitude = magnitude + group * shift
            shift = shift * 128
        end
        count = count + 1
        if byte < 128 then
            break
        end
    end
    if count > 1 and group == 0 then
        error(nonMinimal(field), 0)
    end
    -- The contract bounds the integer, not its code. A negative value reads as
    -- -(magnitude + 1), so from `MAX_INTEGER` upward the magnitude is already
    -- too large when the sign bit is set.
    if magnitude + sign > MAX_INTEGER then
        error(OVERFLOW, 0)
    end
    if sign == 1 then
        return -magnitude - 1
    end
    return magnitude
end

--- Reads one flag word, refusing the bits that carry no meaning.
---@param field string the flag word's field, for the error message
---@param allowed integer the bits with meaning: a run of low bits (2^k - 1),
---as every mask of the format is
---@return integer flags the flag word, at most `allowed`
function Reader:flag(field, allowed)
    local flags = self:varint(field)
    -- A run of low bits is kept by taking the value modulo one above the mask.
    local unknown = flags - flags % (allowed + 1)
    if unknown ~= 0 then
        error(Pack.unknownFlagMessage(field, unknown), 0)
    end
    return flags
end

--- Confirms the record ended with its last field.
function Reader:finish()
    local left = self.length - self.pos + 1
    if left > 0 then
        error(left .. " byte(s) follow the last field", 0)
    end
end

return Pack
