//! Accent- and case-insensitive text folding for matching.
//!
//! Searching for `stiinta` finds `Știință`, like Everything with "match
//! diacritics" off. Names are folded once per index (only those with
//! non-ASCII bytes) and needles once per query, so matching stays a byte
//! comparison. Folding: Unicode canonical decomposition, combining marks
//! dropped, a few Latin letters without a decomposition mapped to their
//! base letters, then lowercase. Invalid UTF-8 is folded lossily; the raw
//! bytes are still matched directly.
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

/// Letters that have no canonical decomposition but an obvious base.
fn base(c: char) -> Option<&'static str> {
    Some(match c {
        'ø' | 'Ø' => "o",
        'ł' | 'Ł' => "l",
        'đ' | 'Đ' | 'ð' | 'Ð' => "d",
        'ħ' | 'Ħ' => "h",
        'ŧ' | 'Ŧ' => "t",
        'ı' => "i",
        'ß' | 'ẞ' => "ss",
        'æ' | 'Æ' => "ae",
        'œ' | 'Œ' => "oe",
        _ => return None,
    })
}

/// Pushes the folded form of one character.
fn fold_char(c: char, out: &mut String) {
    for d in c.nfd() {
        if is_combining_mark(d) {
            continue;
        }
        match base(d) {
            Some(replacement) => out.push_str(replacement),
            None => out.extend(d.to_lowercase()),
        }
    }
}

/// The folded form of `text`. ASCII input only changes case.
pub fn fold(text: &[u8]) -> Vec<u8> {
    if text.is_ascii() {
        return text.to_ascii_lowercase();
    }
    let text = String::from_utf8_lossy(text);
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        fold_char(c, &mut out);
    }
    out.into_bytes()
}

/// The folded form of `text` with, for each folded byte, the byte offset in
/// `text` of the character it came from. Used to map matches back onto the
/// original text for highlighting.
pub fn fold_with_offsets(text: &str) -> (Vec<u8>, Vec<usize>) {
    let mut folded = String::with_capacity(text.len());
    let mut offsets = Vec::with_capacity(text.len());
    let mut piece = String::new();
    for (offset, c) in text.char_indices() {
        piece.clear();
        fold_char(c, &mut piece);
        folded.push_str(&piece);
        offsets.extend(std::iter::repeat_n(offset, piece.len()));
    }
    (folded.into_bytes(), offsets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_accents_case_and_special_letters() {
        assert_eq!(fold("Știință".as_bytes()), b"stiinta");
        assert_eq!(fold("ÉCOLE Ærø Łódź".as_bytes()), b"ecole aero lodz");
        assert_eq!(fold("Straße".as_bytes()), b"strasse");
        assert_eq!(fold(b"Plain ASCII"), b"plain ascii");
        // Decomposed input (as written by macOS) folds the same as composed.
        assert_eq!(fold("e\u{301}cole".as_bytes()), fold("école".as_bytes()));
        assert_eq!(
            fold(b"bad \xff bytes").len(),
            "bad \u{fffd} bytes".to_lowercase().len()
        );
        let (folded, offsets) = fold_with_offsets("Ab Ștî");
        assert_eq!(folded, b"ab sti");
        assert_eq!(offsets, [0, 1, 2, 3, 5, 6]);
    }
}
