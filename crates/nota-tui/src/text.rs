//! Live text, wrapped to the screen.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Whether `c` is drawn as text: not a control character, and not one of
/// the bidirectional formatting characters, which could make the terminal
/// reorder the row they're on.
pub(crate) fn is_drawn(c: char) -> bool {
    !c.is_control()
        && !matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Whether `text` has anything to draw besides spaces.
pub(crate) fn has_visible_text(text: &str) -> bool {
    text.chars().any(|c| is_drawn(c) && !c.is_whitespace())
}

/// The grapheme clusters of `text` with the columns each takes, measured as
/// ratatui measures them when it draws.
pub(crate) fn graphemes(text: &str) -> impl DoubleEndedIterator<Item = (&str, usize)> {
    text.graphemes(true).map(|g| (g, g.width()))
}

/// The columns `text` takes when drawn.
pub(crate) fn display_width(text: &str) -> usize {
    graphemes(text).map(|(_, width)| width).sum()
}

/// Wraps `text` into lines at most `width` columns wide, breaking between
/// words. Runs of whitespace collapse to one space, and a word wider than
/// the line is broken across lines between grapheme clusters. Control and
/// bidirectional formatting characters are dropped (see [`is_drawn`]), so
/// text from the engine can't move the cursor or reorder the screen.
/// An empty `text` is no lines.
pub(crate) fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    if width == 0 {
        return lines;
    }
    let mut line = String::new();
    let mut line_width = 0;
    for word in text.split_whitespace() {
        let word: String = word.chars().filter(|&c| is_drawn(c)).collect();
        let word_width = display_width(&word);
        if word_width == 0 {
            continue;
        }
        if line_width > 0 && line_width + 1 + word_width <= width {
            line.push(' ');
            line.push_str(&word);
            line_width += 1 + word_width;
            continue;
        }
        if line_width > 0 {
            lines.push(std::mem::take(&mut line));
            line_width = 0;
        }
        // The word starts a line; break it if it's too wide for one.
        for (grapheme, grapheme_width) in graphemes(&word) {
            if line_width + grapheme_width > width && line_width > 0 {
                lines.push(std::mem::take(&mut line));
                line_width = 0;
            }
            line.push_str(grapheme);
            line_width += grapheme_width;
        }
    }
    if line_width > 0 {
        lines.push(line);
    }
    lines
}

/// What track 0 heard from `start` to `end` seconds into the session, for
/// tests: placed through a timeline whose one epoch starts at 0.
#[cfg(test)]
pub(crate) fn heard(start: u64, end: u64, text: &str) -> nota_core::Utterance {
    use nota_core::messages::Transcript;
    use nota_core::{SampleIndex, SampleRange, SampleRate, SessionTime, TrackId, TrackTimeline};

    let track = TrackId::new(0);
    let mut timeline = TrackTimeline::new(track);
    timeline
        .open_epoch(SessionTime::ZERO, SampleIndex::ZERO, SampleRate::SPEECH)
        .unwrap();
    let sample = |s: u64| SampleIndex::new(s * u64::from(SampleRate::SPEECH.hz()));
    let range = SampleRange::new(sample(start), sample(end)).unwrap();
    let transcript = Transcript::new(track, range, text.to_owned()).unwrap();
    nota_core::Utterance::place(transcript, &timeline).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_between_words() {
        assert_eq!(
            wrap("the quick brown fox jumps", 10),
            ["the quick", "brown fox", "jumps"]
        );
        // Exactly full lines.
        assert_eq!(wrap("abcde fghij", 5), ["abcde", "fghij"]);
        assert_eq!(wrap("ab cd", 5), ["ab cd"]);
    }

    #[test]
    fn collapses_whitespace_and_drops_control_characters() {
        assert_eq!(wrap("  a \n\t b  ", 10), ["a b"]);
        assert_eq!(wrap("a\u{1b}[2Jb c", 10), ["a[2Jb c"]);
        assert_eq!(wrap("\u{7} x", 10), ["x"]);
        assert!(wrap("   ", 10).is_empty());
        assert!(wrap("", 10).is_empty());
        assert!(wrap("anything", 0).is_empty());
    }

    #[test]
    fn drops_bidirectional_formatting() {
        assert_eq!(
            wrap("hello\u{202e}REC \u{2067}x\u{2069}", 20),
            ["helloREC x"]
        );
        assert!(!has_visible_text("\u{202e} \u{7}\n"));
        assert!(has_visible_text(" a "));
        // The zero-width joiner is formatting too, but emoji need it.
        assert!(is_drawn('\u{200d}'));
    }

    #[test]
    fn breaks_words_wider_than_the_line() {
        assert_eq!(wrap("abcdefgh ij", 3), ["abc", "def", "gh", "ij"]);
        assert_eq!(wrap("x abcdefg", 3), ["x", "abc", "def", "g"]);
    }

    #[test]
    fn counts_display_width_not_bytes() {
        // Each of these is two columns wide.
        assert_eq!(wrap("日本語 です", 6), ["日本語", "です"]);
        assert_eq!(wrap("日本語", 4), ["日本", "語"]);
        assert_eq!(wrap("…and so", 7), ["…and so"]);
    }

    #[test]
    fn keeps_grapheme_clusters_whole() {
        // A ZWJ sequence: several scalars, one two-column cluster.
        let scientist = "👩\u{200d}🔬";
        assert_eq!(display_width(scientist), 2);
        let word = scientist.repeat(5);
        assert_eq!(wrap(&word, 10), std::slice::from_ref(&word));
        assert_eq!(
            wrap(&word, 4),
            [
                scientist.repeat(2),
                scientist.repeat(2),
                scientist.to_owned()
            ]
        );
        // "e" and a combining acute accent stay together.
        assert_eq!(wrap("e\u{301}e\u{301}", 1), ["e\u{301}", "e\u{301}"]);
    }
}
