//! Central sanitizer for untrusted text headed to a terminal.
//!
//! Every untrusted field (archive member paths, link targets, snippets,
//! labels, progress messages, warnings, error chains) passes through
//! [`sanitize`] before printing. C0 controls map to the Unicode control
//! pictures (U+2400 block) so output stays honest and readable; DEL maps
//! to its picture (U+2421); C1 controls have no pictures and map to
//! U+FFFD. Clean strings are borrowed unchanged.

use std::borrow::Cow;

pub fn sanitize(s: &str) -> Cow<'_, str> {
    if !s.chars().any(is_risky) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(s.chars().map(map_char).collect())
}

fn is_risky(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}')
}

fn map_char(c: char) -> char {
    match c {
        '\u{0}'..='\u{1f}' => char::from_u32(0x2400 + c as u32).unwrap(),
        '\u{7f}' => '\u{2421}',
        '\u{80}'..='\u{9f}' => '\u{FFFD}',
        _ => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_are_pictured_del_and_c1_replaced() {
        assert_eq!(sanitize("a\x1b[31mb"), "a␛[31mb");
        assert_eq!(sanitize("x\x07y\x00z"), "x␇y␀z");
        assert_eq!(sanitize("d\x7fe"), "d␡e");
        assert_eq!(sanitize("c\u{9b}d"), "c\u{FFFD}d");
        assert_eq!(sanitize("newline\nkept-visible"), "newline␊kept-visible");
    }

    #[test]
    fn clean_text_is_borrowed_unchanged() {
        assert!(matches!(sanitize("días 写真 ok"), Cow::Borrowed(_)));
    }
}
