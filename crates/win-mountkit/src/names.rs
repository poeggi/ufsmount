// SPDX-License-Identifier: AGPL-3.0-or-later
//! File names from the disk (any bytes except `/` and NUL) as Windows names (UTF-16).
//!
//! - Valid UTF-8 is kept.
//! - Characters Windows forbids in names (`"*/:<>?\|` and 0x00-0x1F) map to
//!   U+F000 + char, as Cygwin does. So does a trailing space or dot.
//!   `/` and NUL occur only on a damaged disk.
//! - Bytes that are not valid UTF-8 map to U+F100 + byte, or with
//!   [`Charset::Latin1`] to the Latin-1 character.
//!
//! Lookups never reverse this mapping: directory tables are keyed by the
//! mapped name, so listing and opening always agree.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Charset {
    #[default]
    Utf8,
    Latin1,
}

const FORBIDDEN: &[u8] = b"\"*/:<>?\\|";

fn forbidden(c: char) -> bool {
    (c as u32) < 0x20 || (c.is_ascii() && FORBIDDEN.contains(&(c as u8)))
}

/// Map a name from the disk to a Windows name.
pub fn to_windows(name: &[u8], charset: Charset) -> Vec<u16> {
    let mut out = Vec::with_capacity(name.len());
    let mut buf = [0u16; 2];
    for chunk in name.utf8_chunks() {
        for c in chunk.valid().chars() {
            if forbidden(c) {
                out.push(0xf000 + c as u16);
            } else {
                out.extend_from_slice(c.encode_utf16(&mut buf));
            }
        }
        for &b in chunk.invalid() {
            out.push(match charset {
                Charset::Utf8 => 0xf100 + u16::from(b),
                Charset::Latin1 => u16::from(b),
            });
        }
    }
    if name != b"."
        && name != b".."
        && let Some(last) = out.last_mut()
        && (*last == u16::from(b' ') || *last == u16::from(b'.'))
    {
        *last += 0xf000;
    }
    out
}

/// A name from the disk for terminal output: control characters and bytes that are
/// not valid UTF-8 are shown as `\xNN`.
pub fn display(name: &[u8]) -> String {
    let mut s = String::new();
    for chunk in name.utf8_chunks() {
        for c in chunk.valid().chars() {
            if c.is_control() {
                s += &format!("\\u{{{:x}}}", c as u32);
            } else {
                s.push(c);
            }
        }
        for b in chunk.invalid() {
            s += &format!("\\x{b:02x}");
        }
    }
    s
}

/// Fold a Windows name for case-insensitive matching.
pub fn fold(name: &[u16]) -> String {
    String::from_utf16_lossy(name).to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn plain_and_unicode() {
        assert_eq!(to_windows(b"file.txt", Charset::Utf8), w("file.txt"));
        assert_eq!(
            to_windows("M\u{fc}ller \u{1f600}".as_bytes(), Charset::Utf8),
            w("M\u{fc}ller \u{1f600}")
        );
    }

    #[test]
    fn forbidden_characters() {
        assert_eq!(to_windows(b"a:b", Charset::Utf8), w("a\u{f03a}b"));
        assert_eq!(
            to_windows(b"q?\\|", Charset::Utf8),
            w("q\u{f03f}\u{f05c}\u{f07c}")
        );
        assert_eq!(
            to_windows(b"tab\there", Charset::Utf8),
            w("tab\u{f009}here")
        );
        assert_eq!(
            to_windows(b"../../x", Charset::Utf8),
            w("..\u{f02f}..\u{f02f}x")
        );
        assert_eq!(to_windows(b"/a\0", Charset::Utf8), w("\u{f02f}a\u{f000}"));
    }

    #[test]
    fn trailing_dot_and_space() {
        assert_eq!(to_windows(b"name.", Charset::Utf8), w("name\u{f02e}"));
        assert_eq!(to_windows(b"name ", Charset::Utf8), w("name\u{f020}"));
        assert_eq!(to_windows(b"a..", Charset::Utf8), w("a.\u{f02e}"));
        assert_eq!(to_windows(b".", Charset::Utf8), w("."));
        assert_eq!(to_windows(b"..", Charset::Utf8), w(".."));
        assert_eq!(to_windows(b"...", Charset::Utf8), w("..\u{f02e}"));
    }

    #[test]
    fn invalid_utf8() {
        assert_eq!(to_windows(b"caf\xe9", Charset::Utf8), w("caf\u{f1e9}"));
        assert_eq!(to_windows(b"caf\xe9", Charset::Latin1), w("caf\u{e9}"));
        assert_eq!(
            to_windows(b"\xff\xfe", Charset::Utf8),
            w("\u{f1ff}\u{f1fe}")
        );
    }

    #[test]
    fn distinct_names_stay_distinct() {
        let names: [&[u8]; 6] = [b"a:", b"a\xef\x80\xba", b"a\xf0", b"a.", b"a", b"a "];
        let mapped: Vec<_> = names.iter().map(|n| to_windows(n, Charset::Utf8)).collect();
        for i in 0..mapped.len() {
            for j in i + 1..mapped.len() {
                if i == 0 && j == 1 {
                    // A literal U+F03A collides with a mapped ':'; the volume layer keeps one.
                    assert_eq!(mapped[i], mapped[j]);
                } else {
                    assert_ne!(mapped[i], mapped[j], "{i} vs {j}");
                }
            }
        }
    }

    #[test]
    fn folding() {
        assert_eq!(fold(&w("ReadMe.TXT")), "readme.txt");
    }
}
