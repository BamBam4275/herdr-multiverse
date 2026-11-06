//! CRC32 matching Python's `zlib.crc32`, printed `%08x`.
//!
//! `activity.py` checksums every routing-log line up to (not including) the
//! trailing `\tcrc32=` field with `zlib.crc32` over the UTF-8 bytes
//! (`activity.py:986-996`), and prints the result lower-case, zero-padded to
//! eight hex digits (`f"{crc:08x}"`, `activity.py:1681`). `zlib.crc32` is the
//! standard CRC-32/ISO-HDLC variant — the same one `crc32fast` implements —
//! so no bit-twiddling is needed here, only matching the padding and casing
//! Python's `%08x` produces.

/// The CRC-32 of `data`, bit-for-bit identical to Python's `zlib.crc32`.
pub fn crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

/// `crc32(data)` formatted the way routing-log lines store it: lower-case,
/// zero-padded to eight hex digits (Python's `f"{crc:08x}"`).
pub fn crc32_hex(data: &[u8]) -> String {
    format!("{:08x}", crc32(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden vector cross-checked against Python's `zlib.crc32` for the
    /// empty string, so a crate swap or feature change can't silently drift
    /// from zlib's CRC-32 variant.
    #[test]
    fn matches_zlib_crc32_of_empty_input() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32_hex(b""), "00000000");
    }

    /// Golden vector for non-ASCII UTF-8 input, cross-checked against
    /// `zlib.crc32('é'.encode('utf-8'))` — guards against a future caller
    /// accidentally hashing a `&str`'s `chars()` or UTF-16 form instead of
    /// its raw UTF-8 bytes.
    #[test]
    fn matches_zlib_crc32_of_non_ascii_utf8() {
        assert_eq!(crc32_hex("é".as_bytes()), "0e048d3e");
    }

    /// Golden vector for a synthetic routing-log-shaped line (not a real
    /// fleet log line — those name real projects and must not land in this
    /// repo), cross-checked against:
    /// `zlib.crc32(prefix.encode("utf-8"))` for
    /// `prefix = '2026-11-02T12:00:00+00:00\tv=1\tseq=42\tevent=message\t
    /// id=20000101-fixer-000001\tfrom=multiverse\tto=example\t
    /// body="hello world"'`.
    #[test]
    fn matches_zlib_crc32_of_a_routing_log_shaped_line() {
        let prefix = "2026-11-02T12:00:00+00:00\tv=1\tseq=42\tevent=message\t\
                       id=20000101-fixer-000001\tfrom=multiverse\tto=example\t\
                       body=\"hello world\"";
        assert_eq!(crc32_hex(prefix.as_bytes()), "00fbbb8f");
    }
}
