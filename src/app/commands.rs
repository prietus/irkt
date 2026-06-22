//! Slash-command parsing and dispatch. Commands map onto the worker's
//! [`Outgoing`] variants, or mutate UI state directly.

use super::state::*;
use crate::irc::{MonitorCmd, Outgoing};

impl App {
    pub fn run_command(&mut self, raw: &str) {
        let raw = raw.trim();
        let (cmd, rest) = match raw.split_once(char::is_whitespace) {
            Some((c, r)) => (c, r.trim()),
            None => (raw, ""),
        };
        let cmd_lc = cmd.to_lowercase();
        let ni = self.active.net;
        if self.networks.is_empty() {
            self.set_status("no network connected");
            return;
        }

        match cmd_lc.as_str() {
            "join" | "j" => {
                if rest.is_empty() {
                    self.set_status("usage: /join #channel [key]");
                    return;
                }
                let mut parts = rest.split_whitespace();
                let chan = parts.next().unwrap().to_string();
                let args: Vec<String> = std::iter::once(chan.clone())
                    .chain(parts.map(str::to_string))
                    .collect();
                let _ = self.networks[ni].out.try_send(Outgoing::Raw {
                    cmd: "JOIN".into(),
                    args,
                });
                let bi = self.networks[ni].ensure_buffer(&chan, BufferKind::Channel);
                self.active = ActiveBuffer { net: ni, buf: bi };
            }
            "part" | "leave" => {
                let (chan, reason) = self.parse_part(rest);
                let Some(chan) = chan else {
                    self.set_status("not in a channel");
                    return;
                };
                let _ = self.networks[ni].out.try_send(Outgoing::Part {
                    channel: chan.clone(),
                    reason: reason.filter(|s| !s.is_empty()),
                });
                if let Some(bi) = self.networks[ni].find_buffer(&chan) {
                    if bi != 0 {
                        self.networks[ni].buffers.remove(bi);
                        self.networks[ni].members.remove(&chan.to_lowercase());
                        self.clamp_active();
                    }
                }
            }
            "msg" | "m" => {
                let Some((target, text)) = rest.split_once(char::is_whitespace) else {
                    self.set_status("usage: /msg <target> <text>");
                    return;
                };
                let target = target.to_string();
                let text = text.trim().to_string();
                let _ = self.networks[ni].out.try_send(Outgoing::Privmsg {
                    target: target.clone(),
                    text: text.clone(),
                });
                let kind = if self.networks[ni].is_channel(&target) {
                    BufferKind::Channel
                } else {
                    BufferKind::Query
                };
                let bi = self.networks[ni].ensure_buffer(&target, kind);
                let nick = self.networks[ni].nick.clone();
                if !self.networks[ni].caps.iter().any(|c| c == "echo-message") {
                    self.networks[ni].buffers[bi].push(Line {
                        time: String::new(),
                        kind: LineKind::Self_,
                        from: nick,
                        text,
                        msgid: None,
                        highlight: false,
                        reply_to: None,
                    });
                }
            }
            "query" | "q" => {
                if rest.is_empty() {
                    self.set_status("usage: /query <nick>");
                    return;
                }
                let nick = rest.split_whitespace().next().unwrap().to_string();
                let bi = self.networks[ni].ensure_buffer(&nick, BufferKind::Query);
                self.active = ActiveBuffer { net: ni, buf: bi };
            }
            "me" => {
                let Some(target) = self.active_target() else { return };
                if matches!(self.active_buffer().map(|b| b.kind), Some(BufferKind::Status)) {
                    self.set_status("cannot /me in the status buffer");
                    return;
                }
                let _ = self.networks[ni].out.try_send(Outgoing::Action {
                    target: target.clone(),
                    text: rest.to_string(),
                });
                if !self.networks[ni].caps.iter().any(|c| c == "echo-message") {
                    let nick = self.networks[ni].nick.clone();
                    let bi = self.active.buf;
                    self.networks[ni].buffers[bi].push(Line {
                        time: String::new(),
                        kind: LineKind::Action,
                        from: nick,
                        text: rest.to_string(),
                        msgid: None,
                        highlight: false,
                        reply_to: None,
                    });
                }
            }
            "nick" => {
                if rest.is_empty() {
                    self.set_status("usage: /nick <newnick>");
                    return;
                }
                let _ = self.networks[ni].out.try_send(Outgoing::Nick(rest.to_string()));
            }
            "topic" => {
                let Some(chan) = self.active_channel() else {
                    self.set_status("not in a channel");
                    return;
                };
                let topic = if rest.is_empty() { None } else { Some(rest.to_string()) };
                let _ = self.networks[ni].out.try_send(Outgoing::Topic { channel: chan, topic });
            }
            "whois" | "w" => {
                if rest.is_empty() {
                    self.set_status("usage: /whois <nick>");
                    return;
                }
                let _ = self.networks[ni]
                    .out
                    .try_send(Outgoing::Whois(rest.split_whitespace().next().unwrap().to_string()));
            }
            "away" => {
                let msg = if rest.is_empty() { None } else { Some(rest.to_string()) };
                let _ = self.networks[ni].out.try_send(Outgoing::Away(msg));
            }
            "mode" => {
                let mut parts = rest.split_whitespace();
                let Some(target) = parts.next() else {
                    self.set_status("usage: /mode <target> <modes> [args]");
                    return;
                };
                let modes = parts.next().unwrap_or("").to_string();
                let args: Vec<String> = parts.map(str::to_string).collect();
                let _ = self.networks[ni].out.try_send(Outgoing::Mode {
                    target: target.to_string(),
                    modes,
                    args,
                });
            }
            "kick" => {
                let Some(chan) = self.active_channel() else {
                    self.set_status("not in a channel");
                    return;
                };
                let mut parts = rest.splitn(2, char::is_whitespace);
                let Some(nick) = parts.next().filter(|s| !s.is_empty()) else {
                    self.set_status("usage: /kick <nick> [reason]");
                    return;
                };
                let reason = parts.next().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
                let _ = self.networks[ni].out.try_send(Outgoing::Kick {
                    channel: chan,
                    nick: nick.to_string(),
                    reason,
                });
            }
            "invite" => {
                let mut parts = rest.split_whitespace();
                let Some(nick) = parts.next() else {
                    self.set_status("usage: /invite <nick> [#channel]");
                    return;
                };
                let chan = parts
                    .next()
                    .map(str::to_string)
                    .or_else(|| self.active_channel());
                let Some(chan) = chan else {
                    self.set_status("no channel to invite to");
                    return;
                };
                let _ = self.networks[ni].out.try_send(Outgoing::Invite {
                    nick: nick.to_string(),
                    channel: chan,
                });
            }
            "raw" | "quote" => {
                if rest.is_empty() {
                    self.set_status("usage: /raw <line>");
                    return;
                }
                let mut parts = rest.split_whitespace();
                let cmd = parts.next().unwrap().to_string();
                let args = parts.map(str::to_string).collect();
                let _ = self.networks[ni].out.try_send(Outgoing::Raw { cmd, args });
            }
            "names" => {
                if let Some(chan) = self.active_channel() {
                    let _ = self.networks[ni].out.try_send(Outgoing::Raw {
                        cmd: "NAMES".into(),
                        args: vec![chan],
                    });
                }
            }
            "monitor" | "buddy" => self.cmd_monitor(ni, rest),
            "setname" => {
                let _ = self.networks[ni].out.try_send(Outgoing::SetName(rest.to_string()));
            }
            "close" | "wc" => {
                let bi = self.active.buf;
                if bi == 0 {
                    self.set_status("cannot close the status buffer");
                    return;
                }
                let name = self.networks[ni].buffers[bi].name.clone();
                if self.networks[ni].is_channel(&name) {
                    let _ = self.networks[ni].out.try_send(Outgoing::Part {
                        channel: name.clone(),
                        reason: None,
                    });
                    self.networks[ni].members.remove(&name.to_lowercase());
                }
                self.networks[ni].buffers.remove(bi);
                self.clamp_active();
            }
            "server" | "net" => {
                if rest.is_empty() {
                    self.set_status("usage: /server <name>");
                    return;
                }
                if let Some(idx) = self
                    .networks
                    .iter()
                    .position(|n| n.cfg.name.eq_ignore_ascii_case(rest))
                {
                    self.active = ActiveBuffer { net: idx, buf: 0 };
                } else {
                    self.set_status(format!("no network named '{rest}'"));
                }
            }
            "images" => {
                self.inline_images = match rest {
                    "on" => true,
                    "off" => false,
                    _ => !self.inline_images,
                };
                let s = if self.inline_images { "on" } else { "off" };
                self.set_status(format!("inline images {s}"));
            }
            "theme" => {
                if rest.is_empty() {
                    let names = crate::theme::Theme::NAMES.join(", ");
                    self.set_status(format!("theme: {} (available: {names})", self.theme.name));
                    return;
                }
                let name = rest.split_whitespace().next().unwrap();
                if !crate::theme::Theme::NAMES.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                    self.set_status(format!("unknown theme '{name}' (try: {})", crate::theme::Theme::NAMES.join(", ")));
                    return;
                }
                self.theme = crate::theme::Theme::by_name(name);
                if let Err(e) = crate::config::state::save_theme(self.theme.name) {
                    self.set_status(format!("theme set (not saved: {e})"));
                } else {
                    self.set_status(format!("theme: {}", self.theme.name));
                }
            }
            "unfurl" => {
                self.link_previews = match rest {
                    "on" => true,
                    "off" => false,
                    _ => !self.link_previews,
                };
                let s = if self.link_previews { "on" } else { "off" };
                self.set_status(format!("link previews {s}"));
            }
            "react" => {
                if rest.is_empty() {
                    self.set_status("usage: /react <emoji> — reacts to the selected/last message");
                    return;
                }
                if self.networks[ni].isupport.client_tag_denied("draft/react") {
                    self.set_status("this server does not allow reactions");
                    return;
                }
                let Some(target) = self.active_target() else { return };
                let Some(msgid) = self.selected_or_last_msgid() else {
                    self.set_status("no message with a msgid to react to");
                    return;
                };
                let _ = self.networks[ni].out.try_send(Outgoing::React {
                    target,
                    msgid,
                    emoji: rest.to_string(),
                });
            }
            "reply" => {
                if rest.is_empty() {
                    self.set_status("usage: /reply <text> — threads a reply to the selected/last message");
                    return;
                }
                if matches!(self.active_buffer().map(|b| b.kind), Some(BufferKind::Status)) {
                    self.set_status("cannot reply in the status buffer");
                    return;
                }
                let Some(parent) = self.selected_or_last_msgid() else {
                    self.set_status("no message with a msgid to reply to");
                    return;
                };
                self.send_reply(parent, rest.to_string());
            }
            "redact" => {
                let Some(target) = self.active_target() else { return };
                let mut parts = rest.splitn(2, char::is_whitespace);
                let Some(msgid) = parts.next().filter(|s| !s.is_empty()) else {
                    self.set_status("usage: /redact <msgid> [reason]");
                    return;
                };
                let reason = parts.next().map(|s| s.trim().to_string());
                let _ = self.networks[ni].out.try_send(Outgoing::Redact {
                    target,
                    msgid: msgid.to_string(),
                    reason,
                });
            }
            "quit" | "exit" => {
                let reason = if rest.is_empty() { None } else { Some(rest.to_string()) };
                for net in &self.networks {
                    let _ = net.out.try_send(Outgoing::Quit(reason.clone()));
                }
                self.should_quit = true;
            }
            "help" => {
                self.networks[ni].status_mut().push(Line::system(
                    "commands: /join /part /msg /query /me /nick /topic /whois /away /mode \
                     /kick /invite /raw /names /monitor /setname /close /server /images \
                     /unfurl /react /reply /redact /quit",
                ));
                self.active = ActiveBuffer { net: ni, buf: 0 };
            }
            other => {
                self.set_status(format!("unknown command: /{other}"));
            }
        }
    }

    fn cmd_monitor(&mut self, ni: usize, rest: &str) {
        let (sub, args) = match rest.split_once(char::is_whitespace) {
            Some((s, a)) => (s.to_lowercase(), a.trim()),
            None => (rest.to_lowercase(), ""),
        };
        match sub.as_str() {
            "add" | "+" => {
                let nicks: Vec<String> = args.split_whitespace().map(str::to_string).collect();
                if nicks.is_empty() {
                    self.set_status("usage: /monitor add <nick>...");
                    return;
                }
                for n in &nicks {
                    if !self.networks[ni].cfg.buddies.iter().any(|b| b.eq_ignore_ascii_case(n)) {
                        self.networks[ni].cfg.buddies.push(n.clone());
                    }
                }
                let _ = self.networks[ni].out.try_send(Outgoing::Monitor(MonitorCmd::Add(nicks)));
                self.persist_buddies(ni);
            }
            "del" | "-" | "rm" => {
                let nicks: Vec<String> = args.split_whitespace().map(str::to_string).collect();
                self.networks[ni]
                    .cfg
                    .buddies
                    .retain(|b| !nicks.iter().any(|n| n.eq_ignore_ascii_case(b)));
                let _ = self.networks[ni].out.try_send(Outgoing::Monitor(MonitorCmd::Del(nicks)));
                self.persist_buddies(ni);
            }
            "list" | "" => {
                let buddies = self.networks[ni].cfg.buddies.join(", ");
                self.networks[ni]
                    .status_mut()
                    .push(Line::system(format!("buddies: {buddies}")));
                self.active = ActiveBuffer { net: ni, buf: 0 };
            }
            "clear" => {
                self.networks[ni].cfg.buddies.clear();
                let _ = self.networks[ni].out.try_send(Outgoing::Monitor(MonitorCmd::Clear));
                self.persist_buddies(ni);
            }
            _ => self.set_status("usage: /monitor add|del|list|clear"),
        }
    }

    /// Persist live buddy edits to the sidecar `state.toml`. The user's
    /// `config.toml` is never rewritten, so its comments survive.
    fn persist_buddies(&mut self, ni: usize) {
        let name = self.networks[ni].cfg.name.clone();
        let buddies = self.networks[ni].cfg.buddies.clone();
        if let Err(e) = crate::config::state::save_buddies(&name, &buddies) {
            self.status_msg = Some(format!("state save failed: {e}"));
        }
    }

    /// The msgid to act on: the selected message if one is selected, else the
    /// most recent message in the active buffer that carries a msgid.
    pub fn selected_or_last_msgid(&self) -> Option<String> {
        let buf = self.active_buffer()?;
        if let Some(sel) = &buf.selection {
            return Some(sel.clone());
        }
        buf.lines.iter().rev().find_map(|l| l.msgid.clone())
    }

    /// The active buffer's channel name, if it is a channel.
    fn active_channel(&self) -> Option<String> {
        let b = self.active_buffer()?;
        if matches!(b.kind, BufferKind::Channel) {
            Some(b.name.clone())
        } else {
            None
        }
    }

    /// Parse `/part` arguments: an optional leading channel, then a reason.
    fn parse_part(&self, rest: &str) -> (Option<String>, Option<String>) {
        let first = rest.split_whitespace().next();
        if let Some(first) = first {
            if self.networks[self.active.net].is_channel(first) {
                let reason = rest[first.len()..].trim();
                return (Some(first.to_string()), Some(reason.to_string()));
            }
        }
        // No explicit channel: use the active one, treat rest as the reason.
        let chan = self.active_channel();
        let reason = if rest.is_empty() { None } else { Some(rest.to_string()) };
        (chan, reason)
    }
}
