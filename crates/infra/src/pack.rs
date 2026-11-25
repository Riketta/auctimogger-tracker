//! Packing primitives of the database feed's records (`addon/SPEC.md`, section 5.4).
//!
//! A record is a byte stream of unsigned LEB128 varints (7 bits per byte, low
//! group first, continuation bit `0x80`), with signed values zigzagged. The
//! stream travels as text: it is read as a big-endian bit string and cut into
//! 6-bit symbols, each mapped to one character of [`ALPHABET`], the last
//! symbol zero-padded on the right. No record character needs escaping in a
//! Lua string, so a record is one printable word.
//!
//! Everything here is integer-exact. The contract's largest integer
//! ([`MAX_INTEGER`]) is the largest a Lua 5.1 double holds exactly.

use std::fmt;

/// The 64 characters of a record's text: digits, letters, `+` and `.`. No
/// quote, backslash, bracket, space or control character.
pub const ALPHABET: &str = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz+.";

/// The largest integer the contract carries (`2^53 - 1`).
pub const MAX_INTEGER: u64 = (1 << 53) - 1;

/// Why a packed value could not be read. Every variant is a malformed record;
/// reading never panics on hostile input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackError {
    /// A character outside [`ALPHABET`].
    Symbol {
        /// Byte offset of the offending character.
        offset: usize,
        /// The offending byte.
        byte: u8,
    },
    /// The text ended inside a field.
    Truncated {
        /// The field being read.
        field: &'static str,
    },
    /// More than four bits left after the last full byte, or leftover bits
    /// that are not all zero.
    Padding,
    /// Whole bytes left after the last field.
    Trailing {
        /// How many bytes were not read.
        count: usize,
    },
    /// A varint longer than its value needs: a redundant zero group.
    NonMinimalVarint {
        /// The field being read.
        field: &'static str,
    },
    /// A varint longer than ten bytes, or an integer above [`MAX_INTEGER`].
    Overflow,
    /// A flag word setting bits that carry no meaning.
    UnknownFlag {
        /// The flag word's field.
        field: &'static str,
        /// The bits without meaning.
        bits: u64,
    },
    /// A value above what its field can hold.
    OutOfRange {
        /// The field being read.
        field: &'static str,
        /// The value that was read.
        value: u64,
    },
    /// A record without any span.
    NoSpans,
}

impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Symbol { offset, byte } => {
                write!(f, "character {byte:#04x} at offset {offset} is not a record symbol")
            }
            Self::Truncated { field } => write!(f, "record ended inside `{field}`"),
            Self::Padding => write!(f, "record padding is not at most four zero bits"),
            Self::Trailing { count } => write!(f, "{count} byte(s) follow the last field"),
            Self::NonMinimalVarint { field } => write!(f, "`{field}` is a non-minimal varint"),
            Self::Overflow => write!(f, "varint is longer than ten bytes or above 2^53 - 1"),
            Self::UnknownFlag { field, bits } => {
                write!(f, "`{field}` sets undefined bits {bits:#x}")
            }
            Self::OutOfRange { field, value } => write!(f, "`{field}` holds {value}, out of range"),
            Self::NoSpans => write!(f, "record has no span"),
        }
    }
}

impl std::error::Error for PackError {}

// ------------------------------------------------------------------- writing

/// The low seven bits of a value, as one byte.
fn low7(value: u64) -> u8 {
    u8::try_from(value & 0x7F).unwrap_or(u8::MAX)
}

/// Appends `value` as an unsigned LEB128 varint: 7 bits per byte, low group
/// first, every byte but the last holding the continuation bit `0x80`.
pub fn put_varint(out: &mut Vec<u8>, value: u64) {
    let mut rest = value;
    while rest >= 0x80 {
        out.push(low7(rest) | 0x80);
        rest >>= 7;
    }
    out.push(low7(rest));
}

/// The zigzag encoding of `value`: the sign in the low bit, the magnitude
/// above it, so `0, -1, 1, -2, 2` read as `0, 1, 2, 3, 4`.
#[must_use]
#[allow(clippy::cast_sign_loss)] // the bit pattern is the encoding
pub fn zigzag(value: i64) -> u64 {
    let bits = value as u64;
    (bits << 1) ^ ((value >> 63) as u64)
}

/// Appends `value` zigzagged (see [`zigzag`]).
pub fn put_zigzag(out: &mut Vec<u8>, value: i64) {
    put_varint(out, zigzag(value));
}

/// Reverses [`zigzag`].
#[must_use]
pub fn unzigzag(value: u64) -> i64 {
    // A contract-bounded varint keeps both halves inside `i64`; anything
    // larger saturates instead of wrapping.
    let magnitude = i64::try_from(value >> 1).unwrap_or(i64::MAX);
    let negative = i64::try_from(value & 1).unwrap_or(0);
    magnitude ^ -negative
}

/// Narrows a masked 6-bit value to the width [`symbol_char`] matches on.
fn symbol(bits: u16) -> u8 {
    u8::try_from(bits & 0x3F).unwrap_or(u8::MAX)
}

/// Renders a byte stream as record text (see the module docs).
#[must_use]
pub fn to_text(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 4 / 3 + 4);
    // `acc` holds the bits not yet emitted: fewer than six between bytes.
    let mut acc: u16 = 0;
    let mut nbits: u32 = 0;
    for &byte in bytes {
        acc = (acc << 8) | u16::from(byte);
        nbits += 8;
        while nbits >= 6 {
            nbits -= 6;
            out.push(symbol_char(symbol(acc >> nbits)));
            acc &= (1 << nbits) - 1;
        }
    }
    if nbits > 0 {
        out.push(symbol_char(symbol(acc << (6 - nbits))));
    }
    out
}

// ------------------------------------------------------------------- reading

/// Reads record text back into its byte stream.
///
/// # Errors
///
/// Returns [`PackError::Symbol`] for a character outside [`ALPHABET`] and
/// [`PackError::Padding`] when more than four bits follow the last full byte,
/// or those bits are not all zero.
pub fn from_text(text: &str) -> Result<Vec<u8>, PackError> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    // `acc` holds the bits not yet turned into bytes: fewer than eight
    // between symbols.
    let mut acc: u32 = 0;
    let mut nbits: u32 = 0;
    for (offset, &byte) in text.as_bytes().iter().enumerate() {
        let digit = symbol_of(byte).ok_or(PackError::Symbol { offset, byte })?;
        acc = (acc << 6) | u32::from(digit);
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push(u8::try_from(acc >> nbits).unwrap_or(u8::MAX));
        }
        acc &= (1 << nbits) - 1;
    }
    // The encoder pads a byte stream with at most four zero bits (8n bits
    // become 6 * ceil(8n / 6) symbols), so a longer zero tail was not written
    // by an encoder: whatever decodes is exactly what `encode` would emit.
    if nbits > 4 || acc != 0 {
        return Err(PackError::Padding);
    }
    Ok(out)
}

/// A cursor over one record's byte stream.
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// A reader positioned at the start of `bytes`.
    #[must_use]
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    /// Reads one unsigned varint. `field` names the field in errors.
    ///
    /// The value is raw: the contract's integer ceiling is applied where the
    /// value means something ([`Self::uint`], [`Self::zigzag`]), because a
    /// zigzag code is twice the integer it carries.
    ///
    /// # Errors
    ///
    /// Returns [`PackError::Truncated`] when the stream ends inside the value,
    /// [`PackError::NonMinimalVarint`] for a redundant zero group (so every
    /// text that decodes is exactly what the encoder writes) and
    /// [`PackError::Overflow`] for a varint longer than ten bytes.
    pub fn varint(&mut self, field: &'static str) -> Result<u64, PackError> {
        let mut value: u64 = 0;
        let mut shift: u32 = 0;
        loop {
            let byte = self.byte(field)?;
            let group = u64::from(byte & 0x7F);
            // Ten bytes at most hold a u64, and the tenth holds one bit: a
            // longer varint is malformed, never a wrapping shift.
            if shift >= 64 || (shift == 63 && group > 1) {
                return Err(PackError::Overflow);
            }
            value |= group << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        // A last group of zero after continuation bytes encodes nothing.
        if group_is_redundant(shift, value) {
            return Err(PackError::NonMinimalVarint { field });
        }
        Ok(value)
    }

    /// Reads one zigzagged value.
    ///
    /// # Errors
    ///
    /// As [`Self::varint`], plus [`PackError::Overflow`] for an integer whose
    /// magnitude is above [`MAX_INTEGER`].
    pub fn zigzag(&mut self, field: &'static str) -> Result<i64, PackError> {
        let value = unzigzag(self.varint(field)?);
        // The contract bounds the integer, not its code, which is twice as large.
        if value.unsigned_abs() > MAX_INTEGER {
            return Err(PackError::Overflow);
        }
        Ok(value)
    }

    /// Reads one unsigned value into the integer type its field is stored in.
    ///
    /// # Errors
    ///
    /// As [`Self::varint`], plus [`PackError::Overflow`] above
    /// [`MAX_INTEGER`] and [`PackError::OutOfRange`] when the value does not
    /// fit the field.
    pub fn uint<T: TryFrom<u64>>(&mut self, field: &'static str) -> Result<T, PackError> {
        let value = self.varint(field)?;
        if value > MAX_INTEGER {
            return Err(PackError::Overflow);
        }
        T::try_from(value).map_err(|_| PackError::OutOfRange { field, value })
    }

    /// Reads one raw byte. `field` names the field in errors.
    ///
    /// # Errors
    ///
    /// Returns [`PackError::Truncated`] at the end of the stream.
    pub fn byte(&mut self, field: &'static str) -> Result<u8, PackError> {
        let next = self.bytes.get(self.pos).copied().ok_or(PackError::Truncated { field })?;
        self.pos += 1;
        Ok(next)
    }

    /// Reads a flag word, refusing the bits that carry no meaning.
    ///
    /// # Errors
    ///
    /// As [`Self::varint`], plus [`PackError::UnknownFlag`] for a set bit
    /// outside `allowed`.
    pub fn flag(&mut self, field: &'static str, allowed: u64) -> Result<u64, PackError> {
        let flags = self.varint(field)?;
        let unknown = flags & !allowed;
        if unknown != 0 {
            return Err(PackError::UnknownFlag { field, bits: unknown });
        }
        Ok(flags)
    }

    /// Confirms the record ended with its last field.
    ///
    /// # Errors
    ///
    /// Returns [`PackError::Trailing`] while unread bytes remain.
    pub fn finish(&self) -> Result<(), PackError> {
        match self.bytes.len().checked_sub(self.pos) {
            Some(0) | None => Ok(()),
            Some(count) => Err(PackError::Trailing { count }),
        }
    }
}

// -------------------------------------------------------------------- scalars

/// Whether a just-read varint ended in a redundant zero group: a single-byte
/// varint may be zero, a longer one may not end in zero.
fn group_is_redundant(shift: u32, value: u64) -> bool {
    shift > 0 && value >> shift == 0
}

/// A fraction stored as an integer: `round(fraction * 10000)`, half away from
/// zero. Not a number, and anything at or below zero, read as zero; a value
/// beyond a `u32` saturates at `u32::MAX`.
#[must_use]
pub fn basis_points(fraction: f64) -> u32 {
    // The float-to-integer `as` cast saturates: not a number and anything at
    // or below zero read as zero, anything beyond `u32` as `u32::MAX`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    ((fraction * 10_000.0).round() as u32)
}

/// Rounds `value` into a `u16`, saturating at both ends. Not a number, and
/// anything at or below zero, read as zero.
#[must_use]
pub fn saturate_u16(value: f64) -> u16 {
    // Saturating for the same reason as `basis_points`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (value.round() as u16)
}

// ------------------------------------------------------------------- symbols

/// The record character of one 6-bit symbol.
fn symbol_char(symbol: u8) -> char {
    match symbol {
        0..=9 => char::from(b'0' + symbol),
        10..=35 => char::from(b'A' + (symbol - 10)),
        36..=61 => char::from(b'a' + (symbol - 36)),
        62 => '+',
        _ => '.',
    }
}

/// The 6-bit symbol of one record character; `None` for any other byte.
const fn symbol_of(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'Z' => Some(byte - b'A' + 10),
        b'a'..=b'z' => Some(byte - b'a' + 36),
        b'+' => Some(62),
        b'.' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    // Test assertions may unwrap: fixture-driven, deterministic inputs.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    fn read_one(bytes: &[u8]) -> Result<u64, PackError> {
        Reader::new(bytes).varint("test")
    }

    fn read_zigzag(bytes: &[u8]) -> Result<i64, PackError> {
        Reader::new(bytes).zigzag("test")
    }

    #[test]
    fn varint_writes_the_documented_byte_layout() {
        let cases: [(u64, &[u8]); 6] = [
            (0, &[0x00]),
            (127, &[0x7F]),
            (128, &[0x80, 0x01]),
            (300, &[0xAC, 0x02]),
            (u64::from(u32::MAX), &[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]),
            (u64::MAX, &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01]),
        ];
        for (value, expected) in cases {
            let mut out = Vec::new();
            put_varint(&mut out, value);
            assert_eq!(out, expected, "varint({value})");
        }
    }

    #[test]
    fn varint_round_trips_the_boundary_values() {
        let values = [
            0,
            1,
            127,
            128,
            16_383,
            16_384,
            1 << 21,
            1 << 28,
            1 << 32,
            1 << 35,
            1 << 42,
            1 << 49,
            MAX_INTEGER,
        ];
        for value in values {
            let mut out = Vec::new();
            put_varint(&mut out, value);
            assert_eq!(read_one(&out), Ok(value), "varint {value}");
        }
    }

    /// A raw varint is exact over the whole `u64` range: a zigzag code is
    /// twice the integer it carries, so the ceiling is not applied here.
    #[test]
    fn raw_varints_cover_the_whole_u64_range() {
        for value in [MAX_INTEGER, MAX_INTEGER + 1, 1 << 56, u64::MAX] {
            let mut out = Vec::new();
            put_varint(&mut out, value);
            assert_eq!(read_one(&out), Ok(value), "varint {value}");
        }
    }

    /// The contract's ceiling is a decoder rule, not only a Lua limit: what
    /// the game could not read exactly is refused, never wrapped.
    #[test]
    fn integers_above_the_contract_maximum_are_refused() {
        for value in [MAX_INTEGER + 1, 1 << 56, u64::MAX] {
            let mut out = Vec::new();
            put_varint(&mut out, value);
            assert_eq!(
                Reader::new(&out).uint::<u64>("test"),
                Err(PackError::Overflow),
                "uint {value}"
            );
        }
        let beyond = i64::try_from(MAX_INTEGER).unwrap() + 1;
        for value in [beyond, -beyond, i64::MAX, i64::MIN] {
            let mut out = Vec::new();
            put_zigzag(&mut out, value);
            assert_eq!(read_zigzag(&out), Err(PackError::Overflow), "zigzag {value}");
        }
    }

    /// LEB128 is minimal: a trailing zero group encodes nothing, and refusing
    /// it keeps every accepted text identical to what the encoder writes.
    #[test]
    fn non_minimal_varints_are_refused() {
        for bytes in [&[0x80, 0x00][..], &[0xFF, 0x00], &[0x80, 0x80, 0x00], &[0x81, 0x80, 0x00]] {
            assert_eq!(
                read_one(bytes),
                Err(PackError::NonMinimalVarint { field: "test" }),
                "{bytes:02x?}"
            );
        }
        // The minimal forms of the same values.
        assert_eq!(read_one(&[0x00]), Ok(0));
        assert_eq!(read_one(&[0x7F]), Ok(127));
        assert_eq!(read_one(&[0x80, 0x01]), Ok(128));
        assert_eq!(read_one(&[0x81, 0x80, 0x01]), Ok(16_385));
    }

    #[test]
    fn varint_errors_are_errors_not_wraps() {
        // Eleven continuation bytes: longer than any u64 varint.
        let mut endless = vec![0x80; 11];
        endless.push(0x00);
        assert_eq!(read_one(&endless), Err(PackError::Overflow));
        // Cut inside the value.
        assert_eq!(read_one(&[]), Err(PackError::Truncated { field: "test" }));
        assert_eq!(read_one(&[0x80]), Err(PackError::Truncated { field: "test" }));
    }

    #[test]
    fn zigzag_follows_the_contract_order() {
        let pairs = [(0, 0), (-1, 1), (1, 2), (-2, 3), (2, 4), (-3, 5)];
        for (value, encoded) in pairs {
            assert_eq!(zigzag(value), encoded, "zigzag({value})");
        }
    }

    #[test]
    fn zigzag_round_trips_both_signs_at_the_edges() {
        let limit = i64::try_from(MAX_INTEGER).unwrap();
        let values = [0, 1, -1, 127, -128, 1 << 32, -(1 << 32), limit, -limit, i64::MAX, i64::MIN];
        for value in values {
            assert_eq!(unzigzag(zigzag(value)), value, "zigzag {value}");
            let mut out = Vec::new();
            put_zigzag(&mut out, value);
            if value.unsigned_abs() <= MAX_INTEGER {
                assert_eq!(read_zigzag(&out), Ok(value), "read {value}");
            } else {
                // Beyond the contract: refused by the reader, not wrapped.
                assert_eq!(read_zigzag(&out), Err(PackError::Overflow), "read {value}");
            }
        }
    }

    #[test]
    fn text_round_trips_every_length() {
        for len in 0..=64usize {
            let bytes: Vec<u8> = (0..len).map(|i| u8::try_from(i * 37 % 256).unwrap()).collect();
            let text = to_text(&bytes);
            assert_eq!(text.len(), (len * 8).div_ceil(6), "symbol count for {len} bytes");
            assert_eq!(from_text(&text), Ok(bytes), "{len} bytes");
        }
    }

    /// Pinned against the Lua 5.1 prototype the packing was measured with, so
    /// the two implementations cannot drift apart unnoticed.
    #[test]
    fn text_matches_the_lua_prototype() {
        let cases: [(&[u8], &str); 11] = [
            (&[], ""),
            (&[0x00], "00"),
            (&[0x01], "0G"),
            (&[0x80], "W0"),
            (&[0xFF, 0x00, 0x01], ".m01"),
            (&[0x01, 0x02, 0x03], "0G83"),
            (&[0xFF, 0xFF, 0xFF, 0xFF], ".....m"),
            (&[0x00, 0x00, 0x00], "0000"),
            (&[0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], "0G00000000000000"),
            (&[0x80, 0x80, 0x80, 0x80, 0x10], "W820W10"),
            (&[0xAC, 0x02], "h08"),
        ];
        for (bytes, text) in cases {
            assert_eq!(to_text(bytes), text, "{bytes:02x?}");
            assert_eq!(from_text(text), Ok(bytes.to_vec()), "{text}");
        }
    }

    #[test]
    fn text_uses_the_whole_alphabet_and_nothing_else() {
        // 37 is coprime with 256, so this walks every byte value four times.
        let bytes: Vec<u8> =
            (0..1024usize).map(|step| u8::try_from(step * 37 % 256).unwrap()).collect();
        let text = to_text(&bytes);
        assert!(text.is_ascii(), "a record is ASCII");
        assert_eq!(
            text.chars().filter(|c| !ALPHABET.contains(*c)).count(),
            0,
            "only alphabet characters"
        );
        assert_eq!(ALPHABET.len(), 64);
        for (index, &byte) in ALPHABET.as_bytes().iter().enumerate() {
            let digit = u8::try_from(index).unwrap();
            assert_eq!(symbol_of(byte), Some(digit), "symbol {byte:#04x}");
            assert_eq!(symbol_char(digit), char::from(byte), "digit {digit}");
            assert!(
                text.contains(char::from(byte)),
                "symbol `{}` is never produced",
                char::from(byte)
            );
        }
        assert_eq!(symbol_of(b'-'), None);
        assert_eq!(symbol_of(b'/'), None);
        assert_eq!(symbol_of(b'_'), None);
    }

    #[test]
    fn text_refuses_characters_outside_the_alphabet() {
        assert!(from_text("0Az9Qm+.").is_ok(), "every alphabet character is accepted");
        assert_eq!(from_text("a-b"), Err(PackError::Symbol { offset: 1, byte: b'-' }));
        assert_eq!(from_text("a b"), Err(PackError::Symbol { offset: 1, byte: b' ' }));
        assert_eq!(from_text("a\""), Err(PackError::Symbol { offset: 1, byte: b'"' }));
        assert_eq!(from_text("a\\"), Err(PackError::Symbol { offset: 1, byte: b'\\' }));
        assert_eq!(from_text("a\n"), Err(PackError::Symbol { offset: 1, byte: b'\n' }));
        assert_eq!(from_text("a\t"), Err(PackError::Symbol { offset: 1, byte: b'\t' }));
        assert_eq!(from_text("\u{7f}"), Err(PackError::Symbol { offset: 0, byte: 0x7F }));
        // A multibyte character: its first byte is already outside the alphabet.
        assert_eq!(from_text("aé"), Err(PackError::Symbol { offset: 1, byte: 0xC3 }));
        assert_eq!(from_text("a\u{1f9a0}"), Err(PackError::Symbol { offset: 1, byte: 0xF0 }));
    }

    /// The contract's end-of-record rule: after the last field at most four
    /// bits remain, and they are all zero.
    #[test]
    fn text_padding_follows_the_contract_rule() {
        // One symbol is six bits: more than an encoder ever pads with.
        assert_eq!(from_text("0"), Err(PackError::Padding));
        assert_eq!(from_text("1"), Err(PackError::Padding));
        // Two symbols are one byte plus four padding bits.
        assert_eq!(from_text("00"), Ok(vec![0x00]));
        assert_eq!(from_text("01"), Err(PackError::Padding));
        // Three symbols are two bytes plus two padding bits.
        assert_eq!(from_text("000"), Ok(vec![0x00, 0x00]));
        assert_eq!(from_text("001"), Err(PackError::Padding));
        // Four symbols are exactly three bytes: nothing to pad.
        assert_eq!(from_text("001."), Ok(vec![0x00, 0x00, 0x7F]));
        // A zero symbol appended to a complete record is six stray bits.
        assert_eq!(from_text("00000"), Err(PackError::Padding));
    }

    #[test]
    fn every_padded_length_reads_back_and_refuses_a_set_padding_bit() {
        for len in 1..=12usize {
            let bytes = vec![0x00; len];
            let text = to_text(&bytes);
            let padded = text.len() * 6 - len * 8;
            assert!(padded < 8, "{len} bytes leave {padded} bits");
            assert_eq!(from_text(&text), Ok(bytes.clone()), "{len} bytes");
            if padded > 0 {
                // The lowest bit of the last symbol is the last padding bit.
                let mut corrupted = text.clone();
                corrupted.pop();
                corrupted.push('1');
                assert_eq!(from_text(&corrupted), Err(PackError::Padding), "{text}");
            }
        }
    }

    #[test]
    fn basis_points_rounds_half_away_from_zero_and_saturates() {
        let cases = [
            (0.25, 2_500),
            (0.8889, 8_889),
            (0.3617, 3_617),
            (1.0, 10_000),
            (0.000_050_001, 1),
            (0.000_049_999, 0),
            (0.3333, 3_333),
            (2.5, 25_000),
            (0.0, 0),
            (-0.5, 0),
            (f64::NAN, 0),
            (f64::INFINITY, u32::MAX),
            (f64::MAX, u32::MAX),
        ];
        for (fraction, expected) in cases {
            assert_eq!(basis_points(fraction), expected, "bp({fraction})");
        }
    }

    #[test]
    fn saturate_u16_clamps_at_both_ends() {
        let cases = [
            (39.44, 39),
            (0.4, 0),
            (65_535.4, u16::MAX),
            (70_000.0, u16::MAX),
            (0.0, 0),
            (-1.0, 0),
            (f64::NAN, 0),
            (f64::INFINITY, u16::MAX),
        ];
        for (value, expected) in cases {
            assert_eq!(saturate_u16(value), expected, "u16({value})");
        }
    }

    #[test]
    fn reader_reports_unread_bytes_and_field_ranges() {
        let bytes = [0xFF, 0xFF, 0x01, 0x80, 0x80, 0x04];
        // 32767 fits a u16; the next varint is 65536, which does not.
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.uint::<u16>("small"), Ok(32_767));
        assert_eq!(
            reader.uint::<u16>("big"),
            Err(PackError::OutOfRange { field: "big", value: 65_536 })
        );
        assert_eq!(reader.finish(), Ok(()));

        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.varint("first"), Ok(32_767));
        assert_eq!(reader.finish(), Err(PackError::Trailing { count: 3 }));
        assert_eq!(reader.byte("late"), Ok(0x80));
        assert_eq!(reader.byte("late"), Ok(0x80));
        assert_eq!(reader.byte("late"), Ok(0x04));
        assert_eq!(reader.byte("late"), Err(PackError::Truncated { field: "late" }));

        let mut reader = Reader::new(&[0x01, 0x02]);
        assert_eq!(reader.flag("known", 0x01), Ok(0x01));
        assert_eq!(
            reader.flag("unknown", 0x01),
            Err(PackError::UnknownFlag { field: "unknown", bits: 0x02 })
        );
    }

    #[test]
    fn display_names_the_cause() {
        let cases = [
            (
                PackError::Symbol { offset: 3, byte: 0x2D },
                "character 0x2d at offset 3 is not a record symbol",
            ),
            (PackError::Truncated { field: "live.floor" }, "record ended inside `live.floor`"),
            (PackError::Padding, "record padding is not at most four zero bits"),
            (PackError::Trailing { count: 1 }, "1 byte(s) follow the last field"),
            (PackError::Overflow, "varint is longer than ten bytes or above 2^53 - 1"),
            (
                PackError::NonMinimalVarint { field: "live.floor" },
                "`live.floor` is a non-minimal varint",
            ),
            (
                PackError::UnknownFlag { field: "flags", bits: 0x80 },
                "`flags` sets undefined bits 0x80",
            ),
            (
                PackError::OutOfRange { field: "supplyDays", value: 65_536 },
                "`supplyDays` holds 65536, out of range",
            ),
            (PackError::NoSpans, "record has no span"),
        ];
        for (error, text) in cases {
            assert_eq!(error.to_string(), text);
        }
    }
}
