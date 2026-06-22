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
}

fn scroll(app: &mut App, delta: i32) {
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
    if word.is_empty() && !app.input.is_empty() {
        return;
    }

    let (candidates, suffix) = if app.input.starts_with('/') && start == 0 {
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
        return;
    }
    let replacement = format!("{}{}", candidates[0], suffix);
    app.input.replace_range(start..app.cursor, &replacement);
    app.cursor = start + replacement.len();
    app.completion = Some(Completion {
        start,
        candidates,
        index: 0,
        suffix,
    });
}

const COMMANDS: &[&str] = &[
    "join", "part", "msg", "query", "me", "nick", "topic", "whois", "away", "mode", "kick",
    "invite", "raw", "names", "monitor", "buddy", "setname", "close", "server", "images",
    "unfurl", "react", "reply", "redact", "quit", "help",
];
