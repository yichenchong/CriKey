//! Ordering rules for driving the engine's input method context.
//!
//! CJK input works like this: the launcher owns the keyboard, gives keys to the
//! system input method, and sends the input method's *results* here as
//! `ImeEvent`. No composition key is ever injected into the engine. What
//! arrives is therefore a stream of preedit updates and commits, and this
//! module decides which signals the engine's input method context emits, in
//! which order. That decision is the entire correctness argument for
//! composition, so it lives in safe, engine-free Rust with tests, and the C
//! shim only does as it is told.
//!
//! Three measured facts shape the rules.
//!
//! * `preedit-started` on its own is a no-op: the engine only calls
//!   `get_preedit` when `preedit-changed` fires. A first syllable emitted with
//!   `started` alone never appears. So a composition that begins emits both.
//! * The preedit text must be in place *before* `preedit-changed`, because
//!   that is when the engine reads it back.
//! * winit emits `Preedit("", None)` immediately before `Commit`. Bridging
//!   that clear straight through to `preedit-finished` produces a spurious
//!   `compositionend:""` in the page before the real one. So an empty preedit
//!   does not end a composition here; it is remembered, and the composition is
//!   ended by the commit that follows -- or, if no commit follows because the
//!   user abandoned the composition, by the next event of any other kind.

/// One thing to do to the engine's input method context.
///
/// Deliberately primitive: no variant means "compose", so the ordering cannot
/// be re-decided further down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImeOp {
    /// Replace the preedit the context reports from `get_preedit`.
    ///
    /// `cursor_chars` is a CHARACTER offset into `text`, already clamped to
    /// its length -- the engine has no way to report a nonsensical one.
    SetPreedit {
        text: String,
        cursor_chars: u32,
    },
    PreeditStarted,
    PreeditChanged,
    PreeditFinished,
    /// Insert text into the focused element, ending any composition.
    Commit(String),
}

/// Translates launcher IME events into ordered engine operations.
#[derive(Debug, Default)]
pub struct ImeBridge {
    composing: bool,
    /// An empty preedit was seen while composing and has not been acted on.
    /// See the module comment: acting on it immediately is the defect.
    clear_deferred: bool,
}

impl ImeBridge {
    pub fn new() -> Self {
        Self::default()
    }

    /// True while the page has a live composition.
    pub fn composing(&self) -> bool {
        self.composing
    }

    /// A preedit update. `cursor` is the character range the launcher reported,
    /// or `None` for winit's absent cursor.
    pub fn preedit(&mut self, text: String, cursor: Option<(u32, u32)>) -> Vec<ImeOp> {
        let mut ops = Vec::with_capacity(3);
        if text.is_empty() {
            // Not an end of composition. Remembered, then either overtaken by
            // the commit that is about to arrive or flushed by the next
            // unrelated event.
            if self.composing {
                self.clear_deferred = true;
            }
            return ops;
        }
        self.clear_deferred = false;
        let cursor_chars = clamp_cursor(&text, cursor);
        ops.push(ImeOp::SetPreedit { text, cursor_chars });
        if !self.composing {
            self.composing = true;
            ops.push(ImeOp::PreeditStarted);
        }
        ops.push(ImeOp::PreeditChanged);
        ops
    }

    /// A commit. This is what ends a composition.
    pub fn commit(&mut self, text: String) -> Vec<ImeOp> {
        let mut ops = Vec::with_capacity(4);
        ops.push(ImeOp::Commit(text));
        if self.composing {
            ops.extend(self.end_composition());
        }
        ops
    }

    /// Acts on a deferred empty preedit, if one is outstanding.
    ///
    /// Called for every event that is not an IME event -- a key, a click, a
    /// resize, a navigation, a close. A composition the user abandoned rather
    /// than committed reaches its end here, at the first moment it is certain
    /// no commit is coming.
    pub fn flush(&mut self) -> Vec<ImeOp> {
        if self.composing && self.clear_deferred {
            self.end_composition()
        } else {
            Vec::new()
        }
    }

    fn end_composition(&mut self) -> Vec<ImeOp> {
        self.composing = false;
        self.clear_deferred = false;
        vec![
            ImeOp::SetPreedit {
                text: String::new(),
                cursor_chars: 0,
            },
            ImeOp::PreeditChanged,
            ImeOp::PreeditFinished,
        ]
    }
}

/// Character offset of the char boundary at or before UTF-8 byte offset `byte`.
///
/// The launcher's input source, winit, reports preedit cursor ranges as UTF-8
/// *byte* offsets while the engine wants *character* offsets, so somebody has
/// to convert, and getting it wrong puts the candidate window in the wrong
/// place for every non-ASCII composition -- which is all of them. This is that
/// conversion, kept here next to the ordering rules it travels with, so the
/// two halves of the IME bridge are tested together.
///
/// A byte offset past the end clamps to the end. One landing inside a
/// multi-byte sequence rounds down to the character that contains it: that
/// character is genuinely where the caret is, and rounding up would report a
/// position the user has not reached.
pub fn char_offset_from_utf8_byte(text: &str, byte: usize) -> u32 {
    if byte >= text.len() {
        return char_count(text);
    }
    let mut offset = 0u32;
    for (at, _) in text.char_indices() {
        // A byte that is exactly a character's start needs no rounding: the
        // caret sits before that character.
        if at == byte {
            return offset;
        }
        // Having passed the byte without landing on a boundary, the character
        // just counted is the one containing it, so the caret is before that
        // character rather than after it.
        if at > byte {
            return offset.saturating_sub(1);
        }
        offset += 1;
    }
    // Every character starts before the byte and none starts at it, so the
    // byte is inside the last character.
    offset.saturating_sub(1)
}

fn char_count(text: &str) -> u32 {
    u32::try_from(text.chars().count()).unwrap_or(u32::MAX)
}

/// The cursor offset the engine should report for `text`.
///
/// An absent cursor puts the caret at the end of the preedit, which is where
/// it is while the user is still typing the composition.
fn clamp_cursor(text: &str, cursor: Option<(u32, u32)>) -> u32 {
    let length = char_count(text);
    match cursor {
        None => length,
        Some((begin, _end)) => begin.min(length),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(text: &str, cursor_chars: u32) -> ImeOp {
        ImeOp::SetPreedit {
            text: text.to_owned(),
            cursor_chars,
        }
    }

    #[test]
    fn a_composition_starts_with_started_then_changed() {
        let mut bridge = ImeBridge::new();
        assert_eq!(
            bridge.preedit("n".to_owned(), Some((1, 1))),
            vec![set("n", 1), ImeOp::PreeditStarted, ImeOp::PreeditChanged],
            "started alone is a no-op in the engine; changed must follow it"
        );
        assert!(bridge.composing());
    }

    #[test]
    fn the_preedit_is_set_before_changed_is_emitted() {
        let mut bridge = ImeBridge::new();
        let ops = bridge.preedit("ni".to_owned(), Some((2, 2)));
        let set_at = ops.iter().position(|op| matches!(op, ImeOp::SetPreedit { .. }));
        let changed_at = ops.iter().position(|op| *op == ImeOp::PreeditChanged);
        assert!(
            set_at < changed_at,
            "the engine reads the preedit on changed: {ops:?}"
        );
    }

    #[test]
    fn later_updates_do_not_restart_the_composition() {
        let mut bridge = ImeBridge::new();
        bridge.preedit("n".to_owned(), Some((1, 1)));
        assert_eq!(
            bridge.preedit("nih".to_owned(), Some((3, 3))),
            vec![set("nih", 3), ImeOp::PreeditChanged]
        );
        assert_eq!(
            bridge.preedit("nihao".to_owned(), Some((5, 5))),
            vec![set("nihao", 5), ImeOp::PreeditChanged]
        );
    }

    #[test]
    fn the_winit_clear_before_a_commit_emits_nothing() {
        // The defect this exists to prevent: bridging Preedit("") straight to
        // preedit-finished puts a compositionend:"" in the page before the
        // real one.
        let mut bridge = ImeBridge::new();
        bridge.preedit("nihao".to_owned(), Some((5, 5)));
        assert_eq!(bridge.preedit(String::new(), None), Vec::new());
        assert!(
            bridge.composing(),
            "the composition is still live until the commit"
        );
    }

    #[test]
    fn the_commit_ends_the_composition_exactly_once() {
        let mut bridge = ImeBridge::new();
        bridge.preedit("nihao".to_owned(), Some((5, 5)));
        bridge.preedit(String::new(), None);
        assert_eq!(
            bridge.commit("\u{4f60}\u{597d}".to_owned()),
            vec![
                ImeOp::Commit("\u{4f60}\u{597d}".to_owned()),
                set("", 0),
                ImeOp::PreeditChanged,
                ImeOp::PreeditFinished,
            ],
            "the commit comes first, then the composition is closed"
        );
        assert!(!bridge.composing());
        assert_eq!(bridge.flush(), Vec::new(), "nothing is left outstanding");
    }

    #[test]
    fn the_full_pinyin_cycle_ends_a_composition_once() {
        let mut bridge = ImeBridge::new();
        let mut ops = Vec::new();
        for (text, cursor) in [("n", 1), ("ni", 2), ("nih", 3), ("niha", 4), ("nihao", 5)] {
            ops.extend(bridge.preedit(text.to_owned(), Some((cursor, cursor))));
        }
        ops.extend(bridge.preedit(String::new(), None));
        ops.extend(bridge.commit("\u{4f60}\u{597d}".to_owned()));

        let starts = ops.iter().filter(|op| **op == ImeOp::PreeditStarted).count();
        let finishes = ops.iter().filter(|op| **op == ImeOp::PreeditFinished).count();
        let commits = ops.iter().filter(|op| matches!(op, ImeOp::Commit(_))).count();
        assert_eq!((starts, finishes, commits), (1, 1, 1));
        assert_eq!(*ops.first().expect("ops"), set("n", 1));
        assert_eq!(*ops.last().expect("ops"), ImeOp::PreeditFinished);
        // And the finish is after the commit, never before it.
        let commit_at = ops.iter().position(|op| matches!(op, ImeOp::Commit(_)));
        let finish_at = ops.iter().position(|op| *op == ImeOp::PreeditFinished);
        assert!(commit_at < finish_at);
    }

    #[test]
    fn a_commit_with_no_composition_only_commits() {
        // Latin input, or an input method that commits without composing.
        let mut bridge = ImeBridge::new();
        assert_eq!(bridge.commit("x".to_owned()), vec![ImeOp::Commit("x".to_owned())]);
        assert!(!bridge.composing());
    }

    #[test]
    fn an_abandoned_composition_is_closed_by_the_next_unrelated_event() {
        let mut bridge = ImeBridge::new();
        bridge.preedit("nihao".to_owned(), Some((5, 5)));
        bridge.preedit(String::new(), None);
        assert_eq!(
            bridge.flush(),
            vec![set("", 0), ImeOp::PreeditChanged, ImeOp::PreeditFinished]
        );
        assert!(!bridge.composing());
        assert_eq!(bridge.flush(), Vec::new(), "flushing twice must not end it twice");
    }

    #[test]
    fn a_resumed_composition_cancels_the_deferred_clear() {
        // An input method that clears and then continues composing -- the
        // deferred clear must be forgotten, not applied later.
        let mut bridge = ImeBridge::new();
        bridge.preedit("ni".to_owned(), Some((2, 2)));
        bridge.preedit(String::new(), None);
        assert_eq!(
            bridge.preedit("nih".to_owned(), Some((3, 3))),
            vec![set("nih", 3), ImeOp::PreeditChanged],
            "no second started: the composition never actually ended"
        );
        assert_eq!(bridge.flush(), Vec::new());
    }

    #[test]
    fn flushing_a_live_composition_leaves_it_alone() {
        let mut bridge = ImeBridge::new();
        bridge.preedit("ni".to_owned(), Some((2, 2)));
        assert_eq!(bridge.flush(), Vec::new());
        assert!(bridge.composing());
    }

    #[test]
    fn an_empty_preedit_outside_a_composition_does_nothing() {
        let mut bridge = ImeBridge::new();
        assert_eq!(bridge.preedit(String::new(), None), Vec::new());
        assert_eq!(bridge.flush(), Vec::new());
        assert!(!bridge.composing());
    }

    #[test]
    fn a_second_composition_starts_cleanly_after_a_commit() {
        let mut bridge = ImeBridge::new();
        bridge.preedit("ni".to_owned(), Some((2, 2)));
        bridge.commit("\u{4f60}".to_owned());
        assert_eq!(
            bridge.preedit("hao".to_owned(), Some((3, 3))),
            vec![set("hao", 3), ImeOp::PreeditStarted, ImeOp::PreeditChanged]
        );
    }

    #[test]
    fn cursor_offsets_are_clamped_to_the_preedit() {
        let mut bridge = ImeBridge::new();
        // Two characters, and a launcher claiming the caret is at nine.
        assert_eq!(
            bridge.preedit("\u{4f60}\u{597d}".to_owned(), Some((9, 9))),
            vec![
                set("\u{4f60}\u{597d}", 2),
                ImeOp::PreeditStarted,
                ImeOp::PreeditChanged
            ]
        );
    }

    #[test]
    fn an_absent_cursor_sits_at_the_end_of_the_preedit() {
        let mut bridge = ImeBridge::new();
        assert_eq!(
            bridge.preedit("nihao".to_owned(), None),
            vec![set("nihao", 5), ImeOp::PreeditStarted, ImeOp::PreeditChanged]
        );
    }

    #[test]
    fn byte_offsets_become_character_offsets() {
        // The observed winit report for a two-character Chinese preedit is
        // Preedit("\u{4f60}\u{597d}", Some((6, 6))): six BYTES, two CHARACTERS.
        let text = "\u{4f60}\u{597d}";
        assert_eq!(text.len(), 6);
        assert_eq!(char_offset_from_utf8_byte(text, 6), 2);
        assert_eq!(char_offset_from_utf8_byte(text, 3), 1);
        assert_eq!(char_offset_from_utf8_byte(text, 0), 0);
    }

    #[test]
    fn byte_offsets_are_identical_for_ascii() {
        let text = "nihao";
        for byte in 0..=5 {
            assert_eq!(char_offset_from_utf8_byte(text, byte), byte as u32);
        }
    }

    #[test]
    fn byte_offsets_inside_a_sequence_round_down() {
        let text = "\u{4f60}\u{597d}";
        // Bytes 1 and 2 are inside the first character.
        assert_eq!(char_offset_from_utf8_byte(text, 1), 0);
        assert_eq!(char_offset_from_utf8_byte(text, 2), 0);
        // Bytes 4 and 5 are inside the second.
        assert_eq!(char_offset_from_utf8_byte(text, 4), 1);
        assert_eq!(char_offset_from_utf8_byte(text, 5), 1);
    }

    #[test]
    fn byte_offsets_past_the_end_clamp() {
        assert_eq!(char_offset_from_utf8_byte("ab", 99), 2);
        assert_eq!(char_offset_from_utf8_byte("", 0), 0);
        assert_eq!(char_offset_from_utf8_byte("", 7), 0);
    }

    #[test]
    fn astral_characters_count_as_one_each() {
        // U+20BB7 is four UTF-8 bytes and one character; a surrogate-pair
        // count would report two and misplace the caret.
        let text = "\u{20BB7}\u{20BB7}";
        assert_eq!(text.len(), 8);
        assert_eq!(char_offset_from_utf8_byte(text, 8), 2);
        assert_eq!(char_offset_from_utf8_byte(text, 4), 1);
    }
}
