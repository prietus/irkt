//! Hunspell spell checking via the pure-Rust `spellbook` crate.
//!
//! Unlike [`crate::dict`] (which feeds autocomplete from raw `.dic` word
//! lists), real spell checking needs the matching `.aff` affix rules — the
//! `.dic` only carries lemmas (`habla/NS`), and inflected forms (`hablamos`)
//! come from affix expansion. We load every `.aff`+`.dic` pair found in a
//! directory and key the resulting checkers by ISO-639-1 lang code derived
//! from the filename stem (`es_ES` -> `"es"`).

use spellbook::Dictionary;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

pub type Spellers = HashMap<String, Dictionary>;

/// Load every `.aff`+`.dic` pair in `dir` into the lang -> checker map.
/// Pairs whose lang is already present are skipped (first one wins),
/// as are `.dic` files without a sibling `.aff`.
pub fn load_dir_into(dir: &Path, out: &mut Spellers) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for ent in entries.flatten() {
        let path = ent.path();
        if path.extension().and_then(|s| s.to_str()) != Some("aff") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let lang: String = stem.chars().take(2).collect::<String>().to_ascii_lowercase();
        if lang.chars().count() < 2 || out.contains_key(&lang) {
            continue;
        }
        let dic_path = path.with_extension("dic");
        let (Ok(aff), Ok(dic)) = (fs::read_to_string(&path), fs::read_to_string(&dic_path)) else {
            continue;
        };
        if let Ok(dict) = Dictionary::new(&aff, &dic) {
            out.insert(lang, dict);
        }
    }
}

/// Load all spell checkers irkt can find: the user's `dicts/` dir under the
/// config directory, plus the system hunspell/myspell dirs on Linux.
pub fn load_all() -> Spellers {
    let mut out = Spellers::new();
    if let Some(dir) = directories::ProjectDirs::from("", "", "irkt") {
        load_dir_into(&dir.config_dir().join("dicts"), &mut out);
    }
    #[cfg(target_os = "linux")]
    {
        for sys in ["/usr/share/hunspell", "/usr/share/myspell/dicts"] {
            load_dir_into(Path::new(sys), &mut out);
        }
    }
    out
}

/// Byte ranges of misspelled words in `input`, checked against `dict`.
///
/// Skips anything that isn't prose: URLs, words with digits, `#channel`
/// / `@nick` / `&chan` tokens, nicks present in `nicks`, and the final
/// word when the input doesn't end in whitespace (still being typed).
pub fn misspelled_ranges(dict: &Dictionary, input: &str, nicks: &[String]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let ends_mid_word = !input.ends_with(|c: char| c.is_whitespace());
    for (start, token) in split_tokens(input) {
        let end = start + token.len();
        // Don't flag the word under the cursor while it's being typed.
        if ends_mid_word && end == input.len() {
            continue;
        }
        if token.starts_with('#') || token.starts_with('@') || token.starts_with('&') {
            continue;
        }
        if token.contains("://") || token.starts_with("www.") {
            continue;
        }
        if token.chars().any(|c| c.is_ascii_digit()) {
            continue;
        }
        // Trim punctuation off both edges; what's left is the word.
        let core = token.trim_matches(|c: char| !c.is_alphabetic());
        if core.chars().count() < 2 {
            continue;
        }
        if nicks.iter().any(|n| n.eq_ignore_ascii_case(core)) {
            continue;
        }
        if dict.check(core) {
            continue;
        }
        let core_start = start + (core.as_ptr() as usize - token.as_ptr() as usize);
        out.push((core_start, core_start + core.len()));
    }
    out
}

/// Up to `max` correction suggestions for a single word.
pub fn suggestions(dict: &Dictionary, word: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    dict.suggest(word, &mut out);
    out.truncate(max);
    out
}

/// Whitespace-split with byte offsets.
fn split_tokens(s: &str) -> impl Iterator<Item = (usize, &str)> {
    s.split_whitespace()
        .map(move |tok| (tok.as_ptr() as usize - s.as_ptr() as usize, tok))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A tiny in-memory hunspell dictionary (no affix rules needed here).
    fn mini_dict() -> Dictionary {
        let aff = "SET UTF-8\n";
        let dic = "3\nhello\nworld\ntypo\n";
        Dictionary::new(aff, dic).expect("valid mini dictionary")
    }

    #[test]
    fn flags_unknown_words_but_not_known_or_nicks() {
        let d = mini_dict();
        let nicks = vec!["alice".to_string()];
        // Trailing space so the last word counts as finished.
        let ranges = misspelled_ranges(&d, "hello wrld alice ", &nicks);
        assert_eq!(ranges.len(), 1, "only 'wrld' should be flagged");
        let (s, e) = ranges[0];
        assert_eq!(&"hello wrld alice "[s..e], "wrld");
    }

    #[test]
    fn skips_the_word_under_the_cursor() {
        let d = mini_dict();
        // No trailing space: "wrld" is still being typed, so it's not flagged.
        assert!(misspelled_ranges(&d, "hello wrld", &[]).is_empty());
    }

    #[test]
    fn skips_urls_channels_and_numbers() {
        let d = mini_dict();
        let input = "hello https://x.example/zzz #chan 1234 ";
        assert!(misspelled_ranges(&d, input, &[]).is_empty());
    }

    #[test]
    fn suggestions_are_capped() {
        let d = mini_dict();
        let s = suggestions(&d, "helo", 5);
        assert!(s.len() <= 5);
    }
}
