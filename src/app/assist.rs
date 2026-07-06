//! Composer assistance: ghost-text completion (commands / nicks / dictionary
//! words) and live spell checking. Both are opt-in per channel via `/lang`
//! and degrade to no-ops when no dictionary is loaded.

use super::state::*;
use crate::dict;
use crate::keys::COMMANDS;
use crate::spell;

/// Byte offset where the last whitespace-delimited word in `s` begins.
fn last_word_start(s: &str) -> usize {
    s.char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0)
}

impl App {
    /// State-file key for the active buffer's language setting.
    /// `"<network>/<channel-lowercased>"`. None for the status buffer.
    pub(crate) fn lang_key(&self) -> Option<String> {
        let net = self.active_net()?;
        let buf = self.active_buffer()?;
        if matches!(buf.kind, BufferKind::Status) {
            return None;
        }
        Some(format!("{}/{}", net.cfg.name, buf.name.to_lowercase()))
    }

    /// The active buffer's configured language code (e.g. "en", "es"), if any.
    pub(crate) fn active_lang(&self) -> Option<&str> {
        let key = self.lang_key()?;
        self.channel_langs.get(&key).map(String::as_str)
    }

    /// Inline ghost-text suggestion for the word under the cursor: the *rest*
    /// of a `/command`, a channel nick, or — when the channel has a language —
    /// a dictionary word. Only offered when the cursor sits at the end of the
    /// input and the last char isn't whitespace.
    pub fn ghost_suggestion(&self) -> Option<String> {
        let s = &self.input;
        if s.is_empty() || self.cursor != s.len() {
            return None;
        }
        if s.chars().last().is_none_or(|c| c.is_whitespace()) {
            return None;
        }
        if self.react_mode {
            return None;
        }
        let start = last_word_start(s);
        let raw = &s[start..];

        // `/command` completion (only at the very start of the line).
        if raw.starts_with('/') && start == 0 {
            let p = raw.trim_start_matches('/').to_lowercase();
            if p.is_empty() {
                return None;
            }
            for name in COMMANDS {
                if name.starts_with(&p) && name.len() > p.len() {
                    return Some(name[p.len()..].to_string());
                }
            }
            return None;
        }

        // Nick completion (allow a leading `@`).
        let stripped_offset = usize::from(raw.starts_with('@'));
        let stripped = &raw[stripped_offset..];
        if stripped.is_empty() {
            return None;
        }
        let p = stripped.to_lowercase();
        let members = self
            .active_net()
            .zip(self.active_buffer())
            .and_then(|(net, buf)| net.members.get(&buf.name.to_lowercase()));
        if let Some(members) = members
            && let Some(m) = members.iter().find(|m| {
                let nl = m.nick.to_lowercase();
                nl.starts_with(&p) && nl.len() > p.len()
            })
        {
            return Some(m.nick[stripped.len()..].to_string());
        }

        // Dictionary fallback: only for a plain alphabetic prefix (no `@`) of
        // at least 2 chars, when the channel has a configured language.
        if stripped_offset != 0 || stripped.len() < 2 {
            return None;
        }
        if !stripped.chars().all(|c| c.is_alphabetic()) {
            return None;
        }
        let bucket = self.dict_words.get(self.active_lang()?)?;
        let w = dict::find_completion(bucket, stripped)?;
        Some(w[p.len()..].to_string())
    }

    /// Byte ranges of misspelled words in the composer, or empty when spell
    /// checking doesn't apply (a `/command`, no `/lang`, or no `.aff` loaded).
    pub fn misspelled_ranges(&self) -> Vec<(usize, usize)> {
        if self.input.starts_with('/') || self.react_mode {
            return Vec::new();
        }
        let Some(dict) = self.active_lang().and_then(|l| self.spellers.get(l)) else {
            return Vec::new();
        };
        let nicks = self.active_channel_nicks();
        spell::misspelled_ranges(dict, &self.input, &nicks)
    }

    /// Replace the misspelled word nearest the cursor with its top suggestion
    /// (Alt+S). Remaining suggestions are shown in the status bar. No-op when
    /// there's no language set or nothing looks misspelled.
    pub fn spell_fix(&mut self) {
        if self.input.starts_with('/') || self.react_mode {
            return;
        }
        let Some(dict) = self.active_lang().and_then(|l| self.spellers.get(l)) else {
            self.set_status("no language set here — try /lang <code>");
            return;
        };
        // Pad with a space so the word under the cursor (normally skipped while
        // typing) is also checked — Alt+S is an explicit request.
        let padded = format!("{} ", self.input);
        let nicks = self.active_channel_nicks();
        let ranges = crate::spell::misspelled_ranges(dict, &padded, &nicks);
        // The misspelling whose end is closest to (and at/under) the cursor;
        // otherwise the last one in the line.
        let pick = ranges
            .iter()
            .filter(|(_, e)| *e <= self.cursor)
            .max_by_key(|(_, e)| *e)
            .or_else(|| ranges.last())
            .copied();
        let Some((s, e)) = pick else {
            self.set_status("nothing to correct");
            return;
        };
        let word = self.input[s..e].to_string();
        let sugg = crate::spell::suggestions(dict, &word, 5);
        let Some(first) = sugg.first().cloned() else {
            self.set_status(format!("no suggestions for '{word}'"));
            return;
        };
        self.input.replace_range(s..e, &first);
        // Keep the cursor sensible relative to the edited word.
        self.cursor = if self.cursor >= e {
            (self.cursor + first.len()).saturating_sub(e - s)
        } else if self.cursor > s {
            s + first.len()
        } else {
            self.cursor
        };
        self.cursor = self.cursor.min(self.input.len());
        let rest: Vec<&str> = sugg.iter().skip(1).map(String::as_str).collect();
        if rest.is_empty() {
            self.set_status(format!("{word} → {first}"));
        } else {
            self.set_status(format!("{word} → {first}  (also: {})", rest.join(", ")));
        }
    }

    /// Nicks of the active channel (empty for queries / status / no members).
    fn active_channel_nicks(&self) -> Vec<String> {
        let Some(net) = self.active_net() else {
            return Vec::new();
        };
        let Some(buf) = self.active_buffer() else {
            return Vec::new();
        };
        net.members
            .get(&buf.name.to_lowercase())
            .map(|ms| ms.iter().map(|m| m.nick.clone()).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, NetworkConfig};
    use crate::images::Images;
    use crate::irc::MemberEntry;
    use ratatui_image::picker::Picker;
    use std::collections::BTreeSet;
    use tokio::sync::mpsc;

    fn channel_app(members: &[&str]) -> App {
        let (img_tx, r) = mpsc::channel(1);
        std::mem::forget(r);
        let images = Images::new(Picker::from_fontsize((8, 16)), img_tx);
        let mut app = App::new(AppConfig::default(), images);
        let cfg = NetworkConfig {
            name: "t".into(), nickname: "me".into(), username: None, realname: None,
            server: "s".into(), port: 6697, use_tls: true, nick_password: None,
            sasl_username: None, sasl_password: None, client_cert_path: None,
            client_cert_pass: None, channels: vec![], buddies: vec![], autoconnect: true,
        };
        let (out, r2) = mpsc::channel(8);
        std::mem::forget(r2);
        app.networks.push(Network::new(0, cfg, out));
        let bi = app.networks[0].ensure_buffer("#rust", BufferKind::Channel);
        app.networks[0].members.insert(
            "#rust".into(),
            members
                .iter()
                .map(|n| MemberEntry { nick: (*n).into(), prefixes: String::new(), userhost: None, is_bot: false })
                .collect(),
        );
        app.active = ActiveBuffer { net: 0, buf: bi };
        app
    }

    fn set_input(app: &mut App, s: &str) {
        app.input = s.to_string();
        app.cursor = s.len();
    }

    #[test]
    fn ghost_completes_a_command() {
        let mut app = channel_app(&[]);
        set_input(&mut app, "/jo");
        assert_eq!(app.ghost_suggestion().as_deref(), Some("in"));
    }

    #[test]
    fn ghost_completes_a_channel_nick() {
        let mut app = channel_app(&["alice", "bob"]);
        set_input(&mut app, "hey al");
        assert_eq!(app.ghost_suggestion().as_deref(), Some("ice"));
    }

    #[test]
    fn ghost_uses_dictionary_only_with_a_language() {
        let mut app = channel_app(&[]);
        let mut words = BTreeSet::new();
        words.insert("hello".to_string());
        app.dict_words.insert("en".into(), words);
        set_input(&mut app, "hel");
        // No /lang set yet -> no dictionary ghost.
        assert_eq!(app.ghost_suggestion(), None);
        // After setting the language, the word completes.
        app.channel_langs.insert(app.lang_key().unwrap(), "en".into());
        assert_eq!(app.ghost_suggestion().as_deref(), Some("lo"));
    }

    #[test]
    fn no_ghost_when_cursor_not_at_end_or_after_space() {
        let mut app = channel_app(&["alice"]);
        set_input(&mut app, "hey al");
        app.cursor = 3; // mid-input
        assert_eq!(app.ghost_suggestion(), None);
        set_input(&mut app, "hey al ");
        assert_eq!(app.ghost_suggestion(), None); // trailing space
    }

    #[test]
    fn spell_check_off_without_a_language() {
        let mut app = channel_app(&[]);
        set_input(&mut app, "this haz a typo ");
        assert!(app.misspelled_ranges().is_empty());
    }
}
