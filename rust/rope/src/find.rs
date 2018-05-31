// Copyright 2017 Google Inc. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Implementation of string finding in ropes.

use std::cmp::min;

use memchr::{memchr, memchr2, memchr3};

use rope::{BaseMetric, RopeInfo};
use tree::Cursor;
use regex::RegexBuilder;
use std::str;


const REGEX_SIZE_LIMIT: usize = 10000;


/// The result of a [`find`][find] operation.
/// 
/// [find]: fn.find.html
pub enum FindResult {
    /// The pattern was found at this position.
    Found(usize),
    /// The pattern was not found.
    NotFound,
    /// The cursor has been advanced by some amount. The pattern is not
    /// found before the new cursor, but may be at or beyond it.
    TryAgain,
}

/// A policy for case matching. There may be more choices in the future (for
/// example, an even more forgiving mode that ignores accents, or possibly
/// handling Unicode normalization).
#[derive(Clone, Copy, PartialEq)]
pub enum CaseMatching {
    /// Require an exact codepoint-for-codepoint match (implies case sensitivity).
    Exact,
    /// Case insensitive match. Guaranteed to work for the ASCII case, and
    /// reasonably well otherwise (it is currently defined in terms of the
    /// `to_lowercase` methods in the Rust standard library).
    CaseInsensitive,
}

/// Finds a pattern string in the rope referenced by the cursor, starting at
/// the current location of the cursor (and finding the first match). Both
/// case sensitive and case insensitive matching is provided, controlled by
/// the `cm` parameter. The `regex` parameter controls whether the query
/// should be considered as a regular expression.
/// 
/// On success, the cursor is updated to immediately follow the found string.
/// On failure, the cursor's position is indeterminate.
///
/// Can panic if `pat` is empty.
pub fn find(cursor: &mut Cursor<RopeInfo>, cm: CaseMatching, pat: &str, is_regex: bool) -> Option<usize> {
    match find_progress(cursor, cm, pat, usize::max_value(), is_regex) {
        FindResult::Found(start) => Some(start),
        FindResult::NotFound => None,
        FindResult::TryAgain => unreachable!("find_progress got stuck"),
    }
}

/// A variant of [`find`][find] that makes a bounded amount of progress, then either
/// returns or suspends (returning `TryAgain`).
/// 
/// The `num_steps` parameter controls the number of "steps" processed per
/// call. The unit of "step" is not formally defined but is typically
/// scanning one leaf (using a memchr-like scan) or testing one candidate
/// when scanning produces a result. It should be empirically tuned for a
/// balance between overhead and impact on interactive performance, but the
/// exact value is probably not critical.
/// 
/// [find]: fn.find.html
pub fn find_progress(cursor: &mut Cursor<RopeInfo>, cm: CaseMatching, pat: &str,
    num_steps: usize, is_regex: bool) -> FindResult
{
    if is_regex {
        // regex scanner cannot check if regex is partially matching
        find_progress_iter(cursor, pat, &|_| { Some(0) },
            &|cursor, pat| compare_cursor_regex(cursor, pat, cm), num_steps)
    }
    else {
        match cm {
            CaseMatching::Exact => {
                let b = pat.as_bytes()[0];
                let scanner = |s: &str| memchr(b, s.as_bytes());
                let matcher = compare_cursor_str;
                find_progress_iter(cursor, pat, &scanner, &matcher, num_steps)
            }
            CaseMatching::CaseInsensitive => {
                let pat_lower = pat.to_lowercase();
                let b = pat_lower.as_bytes()[0];
                let matcher = compare_cursor_str_casei;
                if b == b'i' {
                    // 0xC4 is first utf-8 byte of 'İ'
                    let scanner = |s: &str| memchr3(b'i', b'I', 0xC4, s.as_bytes());
                    find_progress_iter(cursor, &pat_lower, &scanner, &matcher, num_steps)
                } else if b == b'k' {
                    // 0xE2 is first utf-8 byte of u+212A (kelvin sign)
                    let scanner = |s: &str| memchr3(b'k', b'K', 0xE2, s.as_bytes());
                    find_progress_iter(cursor, &pat_lower, &scanner, &matcher, num_steps)
                } else if b >= b'a' && b <= b'z' {
                    let scanner = |s: &str| memchr2(b, b - 0x20, s.as_bytes());
                    find_progress_iter(cursor, &pat_lower, &scanner, &matcher, num_steps)
                } else if b < 0x80 {
                    let scanner = |s: &str| memchr(b, s.as_bytes());
                    find_progress_iter(cursor, &pat_lower, &scanner, &matcher, num_steps)
                } else {
                    let c = pat.chars().next().unwrap();
                    let scanner = |s: &str| scan_lowercase(c, s);
                    find_progress_iter(cursor, &pat_lower, &scanner, &matcher, num_steps)
                }
            }
        }
    }
}

// Run the core repeatedly until there is a result, up to a certain number of steps.
fn find_progress_iter(cursor: &mut Cursor<RopeInfo>, pat: &str,
        scanner: &Fn(&str) -> Option<usize>,
        matcher: &Fn(&mut Cursor<RopeInfo>, &str) -> Option<usize>,
        num_steps: usize
    ) -> FindResult
{
    for _ in 0..num_steps {
        match find_core(cursor, pat, scanner, matcher) {
            FindResult::TryAgain => (),
            result => return result,
        }
    }
    FindResult::TryAgain
}

// The core of the find algorithm. It takes a "scanner", which quickly
// scans through a single leaf searching for some prefix of the pattern,
// then a "matcher" which confirms that such a candidate actually matches
// in the full rope.
fn find_core(cursor: &mut Cursor<RopeInfo>, pat: &str,
        scanner: &Fn(&str) -> Option<usize>,
        matcher: &Fn(&mut Cursor<RopeInfo>, &str) -> Option<usize>
    ) -> FindResult
{
    let orig_pos = cursor.pos();
    if let Some((leaf, pos_in_leaf)) = cursor.get_leaf() {
        if let Some(off) = scanner(&leaf[pos_in_leaf..]) {
            let candidate_pos = orig_pos + off;
            cursor.set(candidate_pos);
            match matcher(cursor, pat) {
                Some(actual_pos) => return FindResult::Found(actual_pos),
                None => {
                    // Advance cursor to next codepoint.
                    // Note: could be optimized in some cases but general case is sometimes needed.
                    cursor.set(candidate_pos);
                    cursor.next::<BaseMetric>();
                }
            }
        } else {
            let _ = cursor.next_leaf();
        }
        FindResult::TryAgain
    } else {
        FindResult::NotFound
    }
}


/// Compare whether the substring beginning at the current cursor location
/// is equal to the provided string. Leaves the cursor at an indeterminate
/// position on failure, but the end of the string on success. Returns the
/// start position of the match.
pub fn compare_cursor_str(cursor: &mut Cursor<RopeInfo>, mut pat: &str) -> Option<usize> {
    let start_position = cursor.pos();
    if pat.is_empty() {
        return Some(start_position);
    }
    let success_pos = cursor.pos() + pat.len();
    while let Some((leaf, pos_in_leaf)) = cursor.get_leaf() {
        let n = min(pat.len(), leaf.len() - pos_in_leaf);
        if leaf.as_bytes()[pos_in_leaf..pos_in_leaf + n] != pat.as_bytes()[..n] {
            return None;
        }
        pat = &pat[n..];
        if pat.is_empty() {
            cursor.set(success_pos);
            return Some(start_position);
        }
        let _ = cursor.next_leaf();
    }
    None
}

/// Like `compare_cursor_str` but case invariant (using to_lowercase() to
/// normalize both strings before comparison). Returns the start position
/// of the match.
fn compare_cursor_str_casei(cursor: &mut Cursor<RopeInfo>, pat: &str) -> Option<usize> {
    let start_position = cursor.pos();
    let mut pat_iter = pat.chars();
    let mut c = pat_iter.next().unwrap();
    loop {
        if let Some(rope_c) = cursor.next_codepoint() {
            for lc_c in rope_c.to_lowercase() {
                if c != lc_c {
                    return None;
                }
                if let Some(next_c) = pat_iter.next() {
                    c = next_c;
                } else {
                    return Some(start_position);
                }
            }
        } else {
            // end of string before pattern is complete
            return None;
        }
    }
}

/// Compare whether the substring beginning at the cursor location matches
/// the provided regular expression. The substring begins at the beginning
/// of the leaf/start of the line.
/// If the regular expression can match multiple lines then all leaves are
/// consumed and matched against the regular expression. Otherwise only the
/// current leaf is matched. Returns the start position of the match.
fn compare_cursor_regex(cursor: &mut Cursor<RopeInfo>, pat: &str, cm: CaseMatching) -> Option<usize> {
    let orig_position = cursor.pos();

    if pat.is_empty() {
        return Some(orig_position);
    }

    // create regex from untrusted input
    let regex = RegexBuilder::new(pat)
        .size_limit(REGEX_SIZE_LIMIT)
        .case_insensitive(cm == CaseMatching::CaseInsensitive)
        .build();

    match regex {
        Ok(regex) => {
            let mut text = String::new();

            match cursor.get_leaf() {
                // get text of current leaf
                Some((leaf, pos_in_leaf)) => {
                    text.push_str(str::from_utf8(&leaf.as_bytes()[pos_in_leaf..]).unwrap());
                    let _ = cursor.next_leaf();
                },
                _ => return None
            }

            if is_multiline_regex(pat) {
                // consume all remaining leaves
                while let Some((leaf, _)) = cursor.get_leaf() {
                    text.push_str(leaf);
                    let _ = cursor.next_leaf();
                }
            }

            // match regex against text
            match regex.find(&text) {
                Some(mat) => {
                    // calculate start position based on where the match starts
                    let start_position = orig_position + mat.start();
                    // update cursor and set to end of match
                    let end_position = orig_position + mat.end();
                    cursor.set(end_position);
                    return Some(start_position)
                },
                None => return None
            }
        },
        _ => return None,
    }
}

/// Checks if a regular expression can match multiple lines.
fn is_multiline_regex(regex: &str) -> bool {
    // regex characters that match line breaks
    // todo: currently multiline mode is ignored
    let multiline_indicators = vec!["\n", "\r", "[[:space:]]"];

    multiline_indicators.iter().any(|&i| regex.contains(i))
}

/// Scan for a codepoint that, after conversion to lowercase, matches the probe.
fn scan_lowercase(probe: char, s: &str) -> Option<usize> {
    for (i, c) in s.char_indices() {
        if c.to_lowercase().next().unwrap() == probe {
            return Some(i);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::CaseMatching::{Exact, CaseInsensitive, RegularExpression};
    use tree::Cursor;
    use rope::Rope;

    #[test]
    fn find_small() {
        let a = Rope::from("Löwe 老虎 Léopard");
        let mut c = Cursor::new(&a, 0);
        assert_eq!(find(&mut c, Exact, "L"), Some(0));
        assert_eq!(find(&mut c, Exact, "L"), Some(13));
        assert_eq!(find(&mut c, Exact, "L"), None);
        c.set(0);
        assert_eq!(find(&mut c, Exact, "Léopard"), Some(13));
        assert_eq!(find(&mut c, Exact, "Léopard"), None);
        c.set(0);
        // Note: these two characters both start with 0xE8 in utf-8
        assert_eq!(find(&mut c, Exact, "老虎"), Some(6));
        assert_eq!(find(&mut c, Exact, "老虎"), None);
        c.set(0);
        assert_eq!(find(&mut c, Exact, "虎"), Some(9));
        assert_eq!(find(&mut c, Exact, "虎"), None);
        c.set(0);
        assert_eq!(find(&mut c, Exact, "Tiger"), None);
    }

    #[test]
    fn find_medium() {
        let mut s = String::new();
        for _ in 0..4000 {
            s.push('x');
        }
        s.push_str("Löwe 老虎 Léopard");
        let a = Rope::from(&s);
        let mut c = Cursor::new(&a, 0);
        assert_eq!(find(&mut c, Exact, "L"), Some(4000));
        assert_eq!(find(&mut c, Exact, "L"), Some(4013));
        assert_eq!(find(&mut c, Exact, "L"), None);
        c.set(0);
        assert_eq!(find(&mut c, Exact, "Léopard"), Some(4013));
        assert_eq!(find(&mut c, Exact, "Léopard"), None);
        c.set(0);
        assert_eq!(find(&mut c, Exact, "老虎"), Some(4006));
        assert_eq!(find(&mut c, Exact, "老虎"), None);
        c.set(0);
        assert_eq!(find(&mut c, Exact, "虎"), Some(4009));
        assert_eq!(find(&mut c, Exact, "虎"), None);
        c.set(0);
        assert_eq!(find(&mut c, Exact, "Tiger"), None);
    }

    #[test]
    fn find_casei_small() {
        let a = Rope::from("Löwe 老虎 Léopard");
        let mut c = Cursor::new(&a, 0);
        assert_eq!(find(&mut c, CaseInsensitive, "l"), Some(0));
        assert_eq!(find(&mut c, CaseInsensitive, "l"), Some(13));
        assert_eq!(find(&mut c, CaseInsensitive, "l"), None);
        c.set(0);
        assert_eq!(find(&mut c, CaseInsensitive, "léopard"), Some(13));
        assert_eq!(find(&mut c, CaseInsensitive, "léopard"), None);
        c.set(0);
        assert_eq!(find(&mut c, CaseInsensitive, "LÉOPARD"), Some(13));
        assert_eq!(find(&mut c, CaseInsensitive, "LÉOPARD"), None);
        c.set(0);
        assert_eq!(find(&mut c, CaseInsensitive, "老虎"), Some(6));
        assert_eq!(find(&mut c, CaseInsensitive, "老虎"), None);
        c.set(0);
        assert_eq!(find(&mut c, CaseInsensitive, "Tiger"), None);
    }

    #[test]
    fn find_casei_ascii_nonalpha() {
        let a = Rope::from("![cfg(test)]");
        let mut c = Cursor::new(&a, 0);
        assert_eq!(find(&mut c, CaseInsensitive, "(test)"), Some(5));
        c.set(0);
        assert_eq!(find(&mut c, CaseInsensitive, "(TEST)"), Some(5));
    }

    #[test]
    fn find_casei_special() {
        let a = Rope::from("İ");
        let mut c = Cursor::new(&a, 0);
        assert_eq!(find(&mut c, CaseInsensitive, "i̇"), Some(0));

        let a = Rope::from("i̇");
        let mut c = Cursor::new(&a, 0);
        assert_eq!(find(&mut c, CaseInsensitive, "İ"), Some(0));

        let a = Rope::from("\u{212A}");
        let mut c = Cursor::new(&a, 0);
        assert_eq!(find(&mut c, CaseInsensitive, "k"), Some(0));

        let a = Rope::from("k");
        let mut c = Cursor::new(&a, 0);
        assert_eq!(find(&mut c, CaseInsensitive, "\u{212A}"), Some(0));
    }

    #[test]
    fn find_casei_0xc4() {
        let a = Rope::from("\u{0100}I");
        let mut c = Cursor::new(&a, 0);
        assert_eq!(find(&mut c, CaseInsensitive, "i"), Some(2));
    }

    #[test]
    fn find_regex_small() {
        let a = Rope::from("Löwe 老虎 Léopard\nSecond line");
        let mut c = Cursor::new(&a, 0);
        assert_eq!(find(&mut c, RegularExpression, "L"), Some(0));
        assert_eq!(find(&mut c, RegularExpression, "L"), Some(13));
        assert_eq!(find(&mut c, RegularExpression, "L"), None);
        c.set(0);
        assert_eq!(find(&mut c, RegularExpression, "Léopard"), Some(13));
        assert_eq!(find(&mut c, RegularExpression, "Léopard"), None);
        c.set(0);
        assert_eq!(find(&mut c, RegularExpression, "老虎"), Some(6));
        assert_eq!(find(&mut c, RegularExpression, "老虎"), None);
        c.set(0);
        assert_eq!(find(&mut c, RegularExpression, "Tiger"), None);
        c.set(0);
        assert_eq!(find(&mut c, RegularExpression, "."), Some(0));
        assert_eq!(find(&mut c, RegularExpression, "\\s"), Some(5));
        assert_eq!(find(&mut c, RegularExpression, "\\sLéopard\n.*"), Some(12));
    }

    #[test]
    fn find_regex_medium() {
        let mut s = String::new();
        for _ in 0..4000 {
            s.push('x');
        }
        s.push_str("Löwe 老虎 Léopard\nSecond line");
        let a = Rope::from(&s);
        let mut c = Cursor::new(&a, 0);
        assert_eq!(find(&mut c, RegularExpression, "L"), Some(4000));
        assert_eq!(find(&mut c, RegularExpression, "L"), Some(4013));
        assert_eq!(find(&mut c, RegularExpression, "L"), None);
        c.set(0);
        assert_eq!(find(&mut c, RegularExpression, "Léopard"), Some(4013));
        assert_eq!(find(&mut c, RegularExpression, "Léopard"), None);
        c.set(0);
        assert_eq!(find(&mut c, RegularExpression, "老虎"), Some(4006));
        assert_eq!(find(&mut c, RegularExpression, "老虎"), None);
        c.set(0);
        assert_eq!(find(&mut c, RegularExpression, "Tiger"), None);
        c.set(0);
        assert_eq!(find(&mut c, RegularExpression, "."), Some(0));
        assert_eq!(find(&mut c, RegularExpression, "\\s"), Some(4005));
        assert_eq!(find(&mut c, RegularExpression, "\\sLéopard\n.*"), Some(4012));
    }

    #[test]
    fn compare_cursor_str_small() {
        let a = Rope::from("Löwe 老虎 Léopard");
        let mut c = Cursor::new(&a, 0);
        let pat = "Löwe 老虎 Léopard";
        assert!(compare_cursor_str(&mut c, pat).is_some());
        assert_eq!(c.pos(), pat.len());
        c.set(0);
        let pat = "Löwe";
        assert!(compare_cursor_str(&mut c, pat).is_some());
        assert_eq!(c.pos(), pat.len());
        c.set(0);
        // Empty string is valid for compare_cursor_str (but not find)
        let pat = "";
        assert!(compare_cursor_str(&mut c, pat).is_some());
        assert_eq!(c.pos(), pat.len());
        c.set(0);
        assert!(compare_cursor_str(&mut c, "Löwe 老虎 Léopardfoo").is_none());
    }

    #[test]
    fn compare_cursor_str_medium() {
        let mut s = String::new();
        for _ in 0..4000 {
            s.push('x');
        }
        s.push_str("Löwe 老虎 Léopard");
        let a = Rope::from(&s);
        let mut c = Cursor::new(&a, 0);
        assert!(compare_cursor_str(&mut c, &s).is_some());
        assert_eq!(c.pos(), s.len());
        c.set(2000);
        assert!(compare_cursor_str(&mut c, &s[2000..]).is_some());
        assert_eq!(c.pos(), s.len());
    }
}
