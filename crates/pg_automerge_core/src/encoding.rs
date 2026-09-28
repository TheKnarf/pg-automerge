//! Small, dependency-free text encodings used by the extension: the `\x` hex
//! form of the type's text I/O, base64 for `Bytes` values and ISO 8601 for
//! `Timestamp` values.

use crate::Error;

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// Encode `bytes` as `\x` followed by lowercase hex, the same shape `bytea`
/// uses with `bytea_output = 'hex'`.
pub fn to_hex_literal(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("\\x");
    for &b in bytes {
        out.push(HEX_DIGITS[usize::from(b >> 4)] as char);
        out.push(HEX_DIGITS[usize::from(b & 0x0f)] as char);
    }
    out
}

/// Decode the `\x<hex>` form produced by [`to_hex_literal`]. Hex digits may
/// be upper or lower case; anything else is rejected.
pub fn from_hex_literal(input: &str) -> Result<Vec<u8>, Error> {
    let hex = input.strip_prefix("\\x").ok_or_else(|| {
        Error::InvalidInput(
            "invalid input syntax for type automerge: expected \"\\x\" followed by hex digits"
                .into(),
        )
    })?;
    let hex = hex.as_bytes();
    if hex.len() % 2 != 0 {
        return Err(Error::InvalidInput(
            "invalid input syntax for type automerge: odd number of hex digits".into(),
        ));
    }
    let (pairs, _) = hex.as_chunks::<2>();
    pairs
        .iter()
        .map(|&[hi, lo]| Ok((hex_value(hi)? << 4) | hex_value(lo)?))
        .collect()
}

fn hex_value(c: u8) -> Result<u8, Error> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(Error::InvalidInput(format!(
            "invalid input syntax for type automerge: invalid hex digit {:?}",
            c as char
        ))),
    }
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 (RFC 4648 section 4) with `=` padding.
pub fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let sextet = |shift: u32| BASE64_ALPHABET[((n >> shift) & 0x3f) as usize] as char;
        out.push(sextet(18));
        out.push(sextet(12));
        out.push(if chunk.len() > 1 { sextet(6) } else { '=' });
        out.push(if chunk.len() > 2 { sextet(0) } else { '=' });
    }
    out
}

/// Format milliseconds since the Unix epoch as ISO 8601 UTC with millisecond
/// precision, e.g. `2024-01-02T03:04:05.678Z`.
///
/// Years outside 0000..=9999 use the expanded six-digit signed form
/// (`+010000-01-01T00:00:00.000Z`, `-000001-...`), matching JavaScript's
/// `Date.prototype.toISOString`, which is where Automerge timestamps usually
/// come from. The proleptic Gregorian calendar is used throughout.
pub fn iso8601_millis(ms: i64) -> String {
    let millis = ms.rem_euclid(1000);
    let secs = ms.div_euclid(1000);
    let sec_of_day = secs.rem_euclid(86_400);
    let days = secs.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (h, m, s) = (sec_of_day / 3600, (sec_of_day / 60) % 60, sec_of_day % 60);
    let year = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else if year < 0 {
        format!("-{:06}", -year)
    } else {
        format!("+{year:06}")
    };
    format!("{year}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}.{millis:03}Z")
}

/// Days since 1970-01-01 to (year, month, day) in the proleptic Gregorian
/// calendar. Howard Hinnant's `civil_from_days` algorithm; exact for the
/// whole range reachable from an `i64` millisecond timestamp.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let bytes = [0u8, 1, 0x7f, 0x80, 0xab, 0xff];
        let s = to_hex_literal(&bytes);
        assert_eq!(s, "\\x00017f80abff");
        assert_eq!(from_hex_literal(&s).unwrap(), bytes);
        assert_eq!(from_hex_literal("\\xABff").unwrap(), [0xab, 0xff]);
        assert_eq!(from_hex_literal("\\x").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn hex_rejects_garbage() {
        for bad in ["", "00", "\\x0", "\\xzz", "\\x 00", "{\"a\":1}"] {
            assert!(
                matches!(from_hex_literal(bad), Err(Error::InvalidInput(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn base64_rfc4648_vectors() {
        let cases = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (input, expected) in cases {
            assert_eq!(base64(input.as_bytes()), expected);
        }
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }

    #[test]
    fn iso8601() {
        assert_eq!(iso8601_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            iso8601_millis(1_704_164_645_678),
            "2024-01-02T03:04:05.678Z"
        );
        assert_eq!(iso8601_millis(-1), "1969-12-31T23:59:59.999Z");
        assert_eq!(iso8601_millis(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(
            iso8601_millis(253_402_300_799_999),
            "9999-12-31T23:59:59.999Z"
        );
        assert_eq!(
            iso8601_millis(253_402_300_800_000),
            "+010000-01-01T00:00:00.000Z"
        );
        assert_eq!(
            iso8601_millis(-62_167_219_200_000),
            "0000-01-01T00:00:00.000Z"
        );
        assert_eq!(
            iso8601_millis(-62_167_219_200_001),
            "-000001-12-31T23:59:59.999Z"
        );
        // The extremes must not overflow.
        iso8601_millis(i64::MIN);
        iso8601_millis(i64::MAX);
    }
}
