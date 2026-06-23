//! Hunspell `.dic` reader for autocomplete.
//!
//! Reads every `*.dic` file in a directory, parses the words (stripping
//! `/affix-flags` and comments), and groups them by ISO-639-1 language
//! code derived from the filename. `en_US.dic` and `en_GB.dic` both
//! merge into the `"en"` bucket. Feeds the composer's ghost-text
//! completion (see `App::ghost_suggestion`).

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::Path;

pub type Words = HashMap<String, BTreeSet<String>>;

/// Load every `*.dic` in `dir` and merge into the given lang -> words
/// map. Lang code = lowercase first 2 letters of the filename stem.
pub fn load_dir_into(dir: &Path, out: &mut Words) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for ent in entries.flatten() {
        let path = ent.path();
        if path.extension().and_then(|s| s.to_str()) != Some("dic") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let lang: String = stem.chars().take(2).collect::<String>().to_ascii_lowercase();
        if lang.chars().count() < 2 {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let bucket = out.entry(lang).or_default();
        parse_into(&text, bucket);
    }
}

/// Load all dictionaries irkt can find: the user's `dicts/` dir under the
/// config directory, plus the system hunspell/myspell dirs on Linux.
pub fn load_all() -> Words {
    let mut out = Words::new();
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

fn parse_into(text: &str, out: &mut BTreeSet<String>) {
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // The first non-blank line is a word count in valid hunspell .dic
        // files. Skip a pure-numeric first line.
        if i == 0 && line.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let word = match line.split_once('/') {
            Some((w, _)) => w,
            None => line,
        };
        let word = word.trim();
        if word.len() < 3 {
            continue;
        }
        if word.chars().any(|c| c.is_ascii_digit()) {
            continue;
        }
        out.insert(word.to_lowercase());
    }
}

/// Return the shortest dict word that starts with `prefix` and is
/// strictly longer than it. None if no match.
pub fn find_completion<'a>(dict: &'a BTreeSet<String>, prefix: &str) -> Option<&'a String> {
    if prefix.is_empty() {
        return None;
    }
    let lower = prefix.to_lowercase();
    let mut best: Option<&String> = None;
    for w in dict.range(lower.clone()..) {
        if !w.starts_with(&lower) {
            break;
        }
        if w.len() <= lower.len() {
            continue;
        }
        match best {
            Some(b) if b.len() <= w.len() => {}
            _ => best = Some(w),
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn completion_picks_shortest_longer_word() {
        let d = words(&["hello", "help", "helper", "helping"]);
        // "hel" -> shortest strictly-longer match is "help".
        assert_eq!(find_completion(&d, "hel").map(String::as_str), Some("help"));
        // Exact-length match is never returned (must be strictly longer).
        assert_eq!(find_completion(&d, "help").map(String::as_str), Some("helper"));
    }

    #[test]
    fn parse_strips_affixes_counts_and_short_words() {
        let mut out = BTreeSet::new();
        parse_into("3\nhola/NS\n# comment\nok\nmundo\n", &mut out);
        assert!(out.contains("hola")); // affix flag stripped
        assert!(out.contains("mundo"));
        assert!(!out.contains("ok")); // < 3 chars dropped
        assert!(!out.contains("3")); // count line dropped
    }
}
