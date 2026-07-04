//! Keyboard handling: translate crossterm key events into edits and actions
//! on [`App`].

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::state::*;

pub fn handle_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // Anything other than Tab cancels an in-progress completion.
    if !matches!(key.code, KeyCode::Tab) {
        app.completion = None;
    }
    // Typing clears a transient status message.
    app.status_msg = None;

    match key.code {
        KeyCode::Char('c') if ctrl => {
            for net in &app.networks {
                let _ = net.out.try_send(crate::irc::Outgoing::Quit(None));
            }
            app.should_quit = true;
        }
        KeyCode::Char('n') if ctrl => app.cycle_buffer(true),
        KeyCode::Char('p') if ctrl => app.cycle_buffer(false),
        KeyCode::Char('u') if ctrl => {
            app.input.clear();
            app.cursor = 0;
        }
        KeyCode::Char('w') if ctrl => delete_word(app),
        KeyCode::Char('a') if ctrl => app.cursor = 0,
        KeyCode::Char('e') if ctrl => app.cursor = app.input.len(),
        KeyCode::Char('b') if alt => app.show_sidebar = !app.show_sidebar,
        KeyCode::Char('m') if alt => app.show_members = !app.show_members,
        KeyCode::Char('s') if alt => app.spell_fix(),
        KeyCode::Char('r') if alt => {
            // Enter react mode: the next submitted line is an emoji reaction to
            // the selected (or last) message. Insert the emoji with your OS
            // emoji picker, then Enter.
            if app.selected_or_last_msgid().is_some() {
                app.react_mode = true;
                app.input.clear();
                app.cursor = 0;
            } else {
                app.set_status("no message to react to");
            }
        }
        KeyCode::Char(c) if alt && c.is_ascii_digit() => {
            // Alt+1..9 jumps to the Nth network.
            if let Some(d) = c.to_digit(10) {
                let idx = (d as usize).saturating_sub(1);
                if idx < app.networks.len() {
                    app.active = ActiveBuffer { net: idx, buf: 0 };
                    app.mark_active_read();
                }
            }
        }
        KeyCode::Char(c) => {
            app.input.insert(app.cursor, c);
            app.cursor += c.len_utf8();
        }
        KeyCode::Backspace => {
            if app.cursor > 0 {
                let prev = prev_boundary(&app.input, app.cursor);
                app.input.replace_range(prev..app.cursor, "");
                app.cursor = prev;
            }
        }
        KeyCode::Delete => {
            if app.cursor < app.input.len() {
                let next = next_boundary(&app.input, app.cursor);
                app.input.replace_range(app.cursor..next, "");
            }
        }
        KeyCode::Left => {
            if app.cursor > 0 {
                app.cursor = prev_boundary(&app.input, app.cursor);
            }
        }
        KeyCode::Right => {
            if app.cursor < app.input.len() {
                app.cursor = next_boundary(&app.input, app.cursor);
            }
        }
        KeyCode::Home => app.cursor = 0,
        KeyCode::End => app.cursor = app.input.len(),
        KeyCode::Up if alt => {
            if let Some(buf) = app.active_buffer_mut() {
                buf.move_selection(-1);
            }
        }
        KeyCode::Down if alt => {
            if let Some(buf) = app.active_buffer_mut() {
                buf.move_selection(1);
            }
        }
        KeyCode::Enter => app.submit_input(),
        KeyCode::Tab => apply_tab(app),
        KeyCode::PageUp => scroll(app, 10),
        KeyCode::PageDown => scroll(app, -10),
        KeyCode::Esc => {
            // Esc backs out one layer at a time: react mode, then a message
            // selection, then the input text.
            if app.react_mode {
                app.react_mode = false;
                return;
            }
            if let Some(buf) = app.active_buffer_mut() {
                if buf.selection.is_some() {
                    buf.selection = None;
                    return;
                }
            }
            app.input.clear();
            app.cursor = 0;
        }
        _ => {}
    }

    reconcile_typing(app, &key, ctrl, alt);
}

/// Reflect the composer state to the active channel via `draft/typing`: announce
/// `active` (throttled) while editing a real message, and `done` once the input
/// is empty, a `/command`, or we're in react mode.
fn reconcile_typing(app: &mut App, key: &KeyEvent, ctrl: bool, alt: bool) {
    let composing_msg = !app.input.is_empty() && !app.input.starts_with('/') && !app.react_mode;
    let editing = matches!(key.code, KeyCode::Backspace | KeyCode::Delete)
        || (matches!(key.code, KeyCode::Char(_)) && !ctrl && !alt);
    if composing_msg && editing {
        app.notify_typing();
    } else if !composing_msg {
        app.stop_typing();
    }
}

/// Handle a left-click at screen cell `(x, y)`. The sidebar switches buffers, a
/// click on a member opens a PM with that nick, and a click on a chat message
/// selects it (clicking the selected one again clears the selection).
/// Coordinates outside any known target are ignored.
pub(crate) fn click(app: &mut App, x: u16, y: u16) {
    if x < app.sidebar_w {
        if let Some(&(_, net, buf)) = app.sidebar_rows.iter().find(|&&(ry, _, _)| ry == y) {
            app.select_buffer(net, buf);
        }
        return;
    }
    // A click on a member-list nick opens (or focuses) a private-message buffer.
    if x >= app.member_x.0
        && x < app.member_x.1
        && let Some((_, nick)) = app.member_rows.iter().find(|(ry, _)| *ry == y)
    {
        let nick = nick.clone();
        app.open_query(&nick);
        return;
    }
    // A click inside the chat area selects (or toggles off) that message.
    if x >= app.chat_x.0
        && x < app.chat_x.1
        && let Some((_, mid)) = app.chat_rows.iter().find(|(ry, _)| *ry == y)
    {
        let mid = mid.clone();
        if let Some(buf) = app.active_buffer_mut() {
            buf.selection = if buf.selection.as_deref() == Some(mid.as_str()) {
                None
            } else {
                Some(mid)
            };
        }
    }
}

pub(crate) fn scroll(app: &mut App, delta: i32) {
    if let Some(buf) = app.active_buffer_mut() {
        let max = buf.lines.len();
        let new = (buf.scroll as i32 + delta).clamp(0, max as i32);
        buf.scroll = new as usize;
    }
}

fn prev_boundary(s: &str, i: usize) -> usize {
    let mut j = i - 1;
    while j > 0 && !s.is_char_boundary(j) {
        j -= 1;
    }
    j
}

fn next_boundary(s: &str, i: usize) -> usize {
    let mut j = i + 1;
    while j < s.len() && !s.is_char_boundary(j) {
        j += 1;
    }
    j
}

fn delete_word(app: &mut App) {
    let mut i = app.cursor;
    // Skip trailing spaces, then the word.
    while i > 0 && app.input.as_bytes()[i - 1] == b' ' {
        i -= 1;
    }
    while i > 0 && app.input.as_bytes()[i - 1] != b' ' {
        i -= 1;
    }
    app.input.replace_range(i..app.cursor, "");
    app.cursor = i;
}

/// Tab-completion of slash commands and nicks.
fn apply_tab(app: &mut App) {
    if let Some(comp) = &mut app.completion {
        // Cycle to the next candidate.
        comp.index = (comp.index + 1) % comp.candidates.len();
        let cand = comp.candidates[comp.index].clone();
        let replacement = format!("{}{}", cand, comp.suffix);
        let start = comp.start;
        let end = app.cursor;
        app.input.replace_range(start..end, &replacement);
        app.cursor = start + replacement.len();
        return;
    }

    // Find the word under the cursor (from last space to cursor).
    let before = &app.input[..app.cursor];
    let start = before.rfind(' ').map(|i| i + 1).unwrap_or(0);
    let word = &app.input[start..app.cursor];

    // `/upload <path>` completes filesystem paths instead of nicks.
    let is_upload_arg = start != 0 && {
        let cmd = app
            .input
            .strip_prefix('/')
            .and_then(|s| s.split_whitespace().next())
            .unwrap_or("");
        cmd.eq_ignore_ascii_case("upload") || cmd.eq_ignore_ascii_case("up")
    };

    // An empty word is fine for path completion (lists the directory); for
    // nicks/commands it would match everything, so bail.
    if word.is_empty() && !app.input.is_empty() && !is_upload_arg {
        return;
    }

    let (candidates, suffix) = if is_upload_arg {
        (complete_path(word), String::new())
    } else if app.input.starts_with('/') && start == 0 {
        let prefix = word.strip_prefix('/').unwrap_or(word).to_lowercase();
        let cands: Vec<String> = COMMANDS
            .iter()
            .filter(|c| c.starts_with(&prefix))
            .map(|c| format!("/{c}"))
            .collect();
        (cands, String::new())
    } else {
        // Nick completion from the active channel's members.
        let lower = word.to_lowercase();
        let mut cands: Vec<String> = Vec::new();
        if let (Some(net), Some(buf)) = (app.active_net(), app.active_buffer()) {
            if let Some(members) = net.members.get(&buf.name.to_lowercase()) {
                for m in members {
                    if m.nick.to_lowercase().starts_with(&lower) {
                        cands.push(m.nick.clone());
                    }
                }
            }
        }
        cands.sort();
        // At the start of the line, nicks get a ": " suffix (IRC convention).
        let suffix = if start == 0 { ": ".to_string() } else { " ".to_string() };
        (cands, suffix)
    };

    if candidates.is_empty() {
        // No command / nick / path match — fall back to the inline ghost
        // suggestion (dictionary word for the channel's language). Accept it
        // by appending the rest; no cycling.
        if !is_upload_arg {
            if let Some(rest) = app.ghost_suggestion() {
                if !rest.is_empty() {
                    app.input.insert_str(app.cursor, &rest);
                    app.cursor += rest.len();
                    app.completion = None;
                }
            }
        }
        return;
    }
    let replacement = format!("{}{}", candidates[0], suffix);
    app.input.replace_range(start..app.cursor, &replacement);
    app.cursor = start + replacement.len();
    // For a unique path match, don't enter cycle mode: a second Tab should
    // descend into the just-completed directory (or no-op on a file), like zsh.
    if is_upload_arg && candidates.len() == 1 {
        app.completion = None;
    } else {
        app.completion = Some(Completion {
            start,
            candidates,
            index: 0,
            suffix,
        });
    }
}

/// Filesystem path completion for `/upload`. Returns full replacements for the
/// partial `word`, preserving any `~`/`./` prefix and appending `/` to
/// directories. Hidden entries appear only when the prefix itself starts with `.`.
fn complete_path(word: &str) -> Vec<String> {
    // Split into the directory part (keeping its trailing '/') and the name
    // prefix being completed.
    let (dir_part, prefix) = match word.rfind('/') {
        Some(i) => (&word[..=i], &word[i + 1..]),
        None => ("", word),
    };
    let real_dir = if dir_part.is_empty() {
        std::path::PathBuf::from(".")
    } else {
        crate::app::commands::expand_tilde(dir_part)
    };
    let Ok(entries) = std::fs::read_dir(&real_dir) else {
        return Vec::new();
    };
    let show_hidden = prefix.starts_with('.');
    let prefix_lc = prefix.to_lowercase();
    let mut out: Vec<String> = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        if !name.to_lowercase().starts_with(&prefix_lc) {
            continue;
        }
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let mut cand = format!("{dir_part}{name}");
        if is_dir {
            cand.push('/');
        }
        out.push(cand);
    }
    // Directories first, then case-insensitive alphabetical.
    out.sort_by(|a, b| {
        let ad = a.ends_with('/');
        let bd = b.ends_with('/');
        bd.cmp(&ad).then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
    });
    out
}

pub(crate) const COMMANDS: &[&str] = &[
    "join", "part", "cycle", "msg", "notice", "query", "me", "nick", "topic", "whois", "whowas",
    "away", "back", "mode", "kick",
    "op", "deop", "voice", "devoice", "ban", "unban",
    "invite", "raw", "ctcp", "names", "monitor", "buddy", "setname", "close", "clear", "clearall",
    "ns", "cs", "ms", "identify", "server", "images",
    "unfurl", "joins", "notify", "theme", "lang", "highlight", "ignore", "dim", "upload", "react", "reply", "redact", "quit",
    "help",
];

#[cfg(test)]
mod tests {
    use super::complete_path;

    // `cargo test` runs with the crate root (where Cargo.toml lives) as cwd, so
    // we can complete against the repo's own files.
    #[test]
    fn completes_files_in_cwd() {
        assert!(complete_path("Carg").iter().any(|c| c == "Cargo.toml"));
    }

    #[test]
    fn directories_get_a_slash_and_sort_first() {
        let cands = complete_path("sr");
        assert!(cands.iter().any(|c| c == "src/"), "src should complete with a trailing slash");
        assert!(cands[0].ends_with('/'), "directories sort before files");
    }

    #[test]
    fn descends_into_a_directory() {
        assert!(complete_path("src/key").iter().any(|c| c == "src/keys.rs"));
    }

    #[test]
    fn hides_dotfiles_unless_prefix_asks() {
        assert!(!complete_path("").iter().any(|c| c.starts_with(".git")));
        // The repo has a .github/ dir; an explicit dot prefix reveals it.
        assert!(complete_path(".gith").iter().any(|c| c.starts_with(".github")));
    }
}
