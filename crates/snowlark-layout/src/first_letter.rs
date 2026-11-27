// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS Pseudo 4 §3.2: which text a `::first-letter` takes. Its box is made by the box tree builder
//! (`build.rs`), which finds the text box holding the first letter of a block container's first
//! formatted line and splits it.

use icu_properties::props::GeneralCategory as Gc;
use icu_properties::CodePointMapData;

fn category(c: char) -> Gc {
    CodePointMapData::<Gc>::new().get(c)
}

/// Where the search for the first letter is, across the text boxes it looks through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Punctuation {
    /// Nothing but white space seen yet.
    NotStarted,
    /// Only punctuation (and the spaces between it) seen: the letter may be in a later text box.
    Started,
    /// The search is over.
    Finished,
}

/// "All punctuation (characters of the Unicode general categories P*) that precedes the first
/// letter".
fn preceding_punctuation(c: char) -> bool {
    matches!(
        category(c),
        Gc::ConnectorPunctuation
            | Gc::DashPunctuation
            | Gc::OpenPunctuation
            | Gc::ClosePunctuation
            | Gc::InitialPunctuation
            | Gc::FinalPunctuation
            | Gc::OtherPunctuation
    )
}

/// "Any intervening typographic space (Zs other than U+3000 IDEOGRAPHIC SPACE)" before the letter.
fn preceding_space(c: char) -> bool {
    category(c) == Gc::SpaceSeparator && c != '\u{3000}'
}

/// The punctuation that may follow the letter: P* but for opening punctuation and dashes.
fn following_punctuation(c: char) -> bool {
    matches!(
        category(c),
        Gc::ConnectorPunctuation
            | Gc::ClosePunctuation
            | Gc::InitialPunctuation
            | Gc::FinalPunctuation
            | Gc::OtherPunctuation
    )
}

/// The typographic space that may come between the letter and the punctuation after it: Zs but
/// for U+3000 and the word separators.
fn following_space(c: char) -> bool {
    category(c) == Gc::SpaceSeparator
        && !matches!(
            c,
            '\u{3000}'
                | ' '
                | '\u{A0}'
                | '\u{1361}'
                | '\u{10100}'
                | '\u{10101}'
                | '\u{1039F}'
                | '\u{1091F}'
        )
}

/// White space skipped before the first letter (as collapsing would remove it at the line's start);
/// a preserved line break ends the search.
fn leading_space(c: char, preserve_breaks: bool) -> bool {
    match c {
        '\n' | '\r' => !preserve_breaks,
        ' ' | '\t' | '\u{C}' | '\u{A0}' => true,
        _ => false,
    }
}

/// The byte length of the typographic character unit starting at `at`: a character with the
/// combining marks and joiners after it.
fn unit_len(text: &str, at: usize) -> usize {
    let mut chars = text[at..].char_indices();
    let Some((_, first)) = chars.next() else {
        return 0;
    };
    let mut end = first.len_utf8();
    let mut joined = false;
    for (i, c) in chars {
        let extends = matches!(
            category(c),
            Gc::NonspacingMark | Gc::SpacingMark | Gc::EnclosingMark
        ) || matches!(c, '\u{200D}' | '\u{FE00}'..='\u{FE0F}' | '\u{E0100}'..='\u{E01EF}')
            || joined;
        if !extends {
            break;
        }
        joined = c == '\u{200D}';
        end = i + c.len_utf8();
    }
    end
}

/// How many bytes at the start of `text` belong to the `::first-letter`: white space, the
/// punctuation before the letter, the letter, and the punctuation after it (with the spaces between).
/// When the text is only punctuation so far, `state` stays [`Punctuation::Started`] and the whole
/// text is taken: the letter may follow in the next text box.
pub(crate) fn length(text: &str, preserve_breaks: bool, state: &mut Punctuation) -> usize {
    debug_assert_ne!(*state, Punctuation::Finished);
    let mut len = 0;
    if text.is_empty() {
        return 0;
    }
    let next = |at: usize| text[at..].chars().next();
    if *state == Punctuation::NotStarted {
        while let Some(c) = next(len).filter(|&c| leading_space(c, preserve_breaks)) {
            len += c.len_utf8();
        }
        if len == text.len() {
            return 0;
        }
    }
    while let Some(c) = next(len) {
        let more =
            preceding_punctuation(c) || (*state == Punctuation::Started && preceding_space(c));
        if !more {
            break;
        }
        len += unit_len(text, len);
        *state = Punctuation::Started;
    }
    if len == text.len() && *state == Punctuation::Started {
        return len;
    }
    *state = Punctuation::Finished;
    let Some(c) = next(len) else {
        return 0;
    };
    if leading_space(c, false) {
        return 0;
    }
    len += unit_len(text, len);
    // The punctuation after the letter, in the same text.
    let mut with_space = len;
    while let Some(c) = next(with_space) {
        if following_space(c) {
            with_space += unit_len(text, with_space);
        } else if following_punctuation(c) {
            with_space += unit_len(text, with_space);
            len = with_space;
        } else {
            break;
        }
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    fn len(t: &str) -> usize {
        length(t, false, &mut Punctuation::NotStarted)
    }

    #[test]
    fn the_letter_with_the_punctuation_around_it() {
        assert_eq!(len("Hello"), 1);
        assert_eq!(len("  Hello"), 3);
        assert_eq!(len("\u{201C}Hello"), 4);
        assert_eq!(len("\"(T)\" is"), 5);
        assert_eq!(len("T. and"), 2);
        // a dash after the letter is not taken
        assert_eq!(len("A-b"), 1);
    }

    #[test]
    fn a_combining_mark_belongs_to_its_letter() {
        assert_eq!(len("e\u{301}tude"), 3);
    }

    #[test]
    fn only_spaces_or_a_preserved_break_has_no_letter() {
        assert_eq!(len("   "), 0);
        assert_eq!(length("\nA", true, &mut Punctuation::NotStarted), 0);
        assert_eq!(len("\nA"), 2);
    }

    #[test]
    fn punctuation_alone_waits_for_the_next_text() {
        let mut s = Punctuation::NotStarted;
        assert_eq!(length("\"", false, &mut s), 1);
        assert_eq!(s, Punctuation::Started);
        assert_eq!(length("Ab", false, &mut s), 1);
        assert_eq!(s, Punctuation::Finished);
    }
}
