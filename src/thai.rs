//! Thai word boundaries for double-click selection. Thai is written without spaces, so a
//! dictionary decides where words end: maximal matching (fewest unknown characters, then fewest
//! words) over a 60k-word list from PyThaiNLP (CC0), stored in `assets/thai-words.bin`.
//!
//! The list is front-coded single bytes (`char - U+0E00`): per word, the length shared with the
//! previous word, the rest, then 0. It is decoded on the first Thai double-click only.

use std::sync::OnceLock;

static PACKED: &[u8] = include_bytes!("../assets/thai-words.bin");
const MAX_WORD: usize = 24;

struct Dict {
    /// Words back to back, sorted.
    bytes: Vec<u8>,
    /// Start of each word in `bytes`; word i ends where word i + 1 starts.
    starts: Vec<u32>,
}

impl Dict {
    fn load() -> Dict {
        let (mut bytes, mut starts, mut prev_start, mut i) = (Vec::with_capacity(560_000), Vec::with_capacity(61_000), 0usize, 0);
        while i < PACKED.len() {
            let shared = PACKED[i] as usize;
            let rest = PACKED[i + 1..].iter().position(|&b| b == 0).map_or(PACKED.len(), |n| i + 1 + n);
            let start = bytes.len();
            bytes.extend_from_within(prev_start..prev_start + shared);
            bytes.extend_from_slice(&PACKED[i + 1..rest]);
            starts.push(start as u32);
            prev_start = start;
            i = rest + 1;
        }
        Dict { bytes, starts }
    }

    fn word(&self, i: usize) -> &[u8] {
        let end = self.starts.get(i + 1).map_or(self.bytes.len(), |&e| e as usize);
        &self.bytes[self.starts[i] as usize..end]
    }

    fn contains(&self, w: &[u8]) -> bool {
        let (mut lo, mut hi) = (0, self.starts.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.word(mid).cmp(w) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return true,
            }
        }
        false
    }
}

fn dict() -> &'static Dict {
    static DICT: OnceLock<Dict> = OnceLock::new();
    DICT.get_or_init(Dict::load)
}

pub fn is_thai(c: char) -> bool {
    ('\u{0E01}'..='\u{0E5B}').contains(&c)
}

/// Split a run of Thai text into words. `cells` holds each cell's characters (a base letter and
/// its stacked marks); words only break between cells. Returns word ranges as cell indices.
pub fn words(cells: &[Vec<char>]) -> Vec<(usize, usize)> {
    let n = cells.len();
    let text: Vec<u8> = cells.iter().flatten().map(|&c| (c as u32).wrapping_sub(0x0E00) as u8).collect();
    // Byte offset of each cell boundary.
    let mut at = Vec::with_capacity(n + 1);
    at.push(0);
    for c in cells {
        at.push(at.last().unwrap() + c.len());
    }
    let d = dict();
    // best[i]: (unknown cells, words) for the first i cells; from[i]: (start of last word, known).
    let mut best = vec![(usize::MAX, usize::MAX); n + 1];
    let mut from = vec![(0, false); n + 1];
    best[0] = (0, 0);
    for i in 0..n {
        let (unknown, count) = best[i];
        if unknown == usize::MAX {
            continue;
        }
        let mut relax = |j: usize, cost: (usize, usize), known: bool| {
            if cost < best[j] {
                best[j] = cost;
                from[j] = (i, known);
            }
        };
        relax(i + 1, (unknown + 1, count + 1), false);
        for j in i + 1..=n {
            if at[j] - at[i] > MAX_WORD {
                break;
            }
            if d.contains(&text[at[i]..at[j]]) {
                relax(j, (unknown, count + 1), true);
            }
        }
    }
    // Walk back, merging neighbouring unknown cells into one piece.
    let mut out: Vec<(usize, usize, bool)> = Vec::new();
    let mut j = n;
    while j > 0 {
        let (i, known) = from[j];
        match out.last_mut() {
            Some(last) if !known && !last.2 => last.0 = i,
            _ => out.push((i, j, known)),
        }
        j = i;
    }
    out.reverse();
    out.into_iter().map(|(a, b, _)| (a, b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(s: &str) -> Vec<String> {
        // One cell per base character, with following combining marks folded in (as the grid does).
        let mut cells: Vec<Vec<char>> = Vec::new();
        for c in s.chars() {
            let mark = matches!(c, '\u{0E31}' | '\u{0E34}'..='\u{0E3A}' | '\u{0E47}'..='\u{0E4E}');
            match cells.last_mut() {
                Some(cell) if mark => cell.push(c),
                _ => cells.push(vec![c]),
            }
        }
        words(&cells).into_iter().map(|(a, b)| cells[a..b].iter().flatten().collect()).collect()
    }

    #[test]
    fn splits_thai_into_words() {
        assert_eq!(split("สวัสดีครับ"), ["สวัสดี", "ครับ"]);
        assert_eq!(split("ฉันชอบแมว"), ["ฉัน", "ชอบ", "แมว"]);
        assert_eq!(split("ภาษาไทย"), ["ภาษาไทย"]);
        // Unknown letters stay together instead of splitting into single characters.
        assert_eq!(split("กินขฃฅ").concat(), "กินขฃฅ");
        assert_eq!(split(""), Vec::<String>::new());
    }
}
