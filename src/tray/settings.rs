// SPDX-License-Identifier: AGPL-3.0-or-later
//! The tray's settings, in the admin-only `<ProgramData>\ufsmount`.

use std::collections::HashMap;
use std::path::PathBuf;

use super::task::base_dir;

/// A file system's id from its superblock: it stays the same on any port.
pub type FsId = [u32; 2];

fn file(name: &str) -> Option<PathBuf> {
    base_dir().ok().map(|d| d.join(name))
}

fn write(name: &str, text: &str) {
    let Some(path) = file(name) else { return };
    if let Err(e) = std::fs::write(&path, text) {
        log::warn!("{}: {e}", path.display());
    }
}

/// "Mount automatically": on unless turned off.
pub fn auto_mount() -> bool {
    file("auto-mount")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .is_none_or(|t| t.trim() != "0")
}

pub fn set_auto_mount(on: bool) {
    write("auto-mount", if on { "1" } else { "0" });
}

/// The drive letter chosen for each file system.
pub fn letters() -> HashMap<FsId, char> {
    file("letters")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| parse_letters(&t))
        .unwrap_or_default()
}

pub fn save_letters(letters: &HashMap<FsId, char>) {
    write("letters", &format_letters(letters));
}

/// Lines of `<id0>-<id1> <letter>`, ids in hex. Other lines are ignored.
fn parse_letters(text: &str) -> HashMap<FsId, char> {
    text.lines()
        .filter_map(|line| {
            let (id, letter) = line.split_once(' ')?;
            let (a, b) = id.split_once('-')?;
            let id = [
                u32::from_str_radix(a, 16).ok()?,
                u32::from_str_radix(b, 16).ok()?,
            ];
            let mut chars = letter.chars();
            let letter = chars.next().filter(|c| ('D'..='U').contains(c))?;
            chars.next().is_none().then_some((id, letter))
        })
        .collect()
}

fn format_letters(letters: &HashMap<FsId, char>) -> String {
    let mut lines: Vec<String> = letters
        .iter()
        .map(|([a, b], l)| format!("{a:08x}-{b:08x} {l}\n"))
        .collect();
    lines.sort();
    lines.concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_round_trip() {
        let map = HashMap::from([([1, 0xdead_beef], 'T'), ([7, 7], 'U')]);
        let text = format_letters(&map);
        assert_eq!(text, "00000001-deadbeef T\n00000007-00000007 U\n");
        assert_eq!(parse_letters(&text), map);
    }

    #[test]
    fn bad_lines_are_ignored() {
        let text = "1-2 C\n1-2 V\n1-2 TT\nx-2 T\n1 T\n\n3-4 D\n";
        assert_eq!(parse_letters(text), HashMap::from([([3, 4], 'D')]));
    }
}
