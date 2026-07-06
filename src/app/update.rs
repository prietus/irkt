//! Applying worker events to [`App`] state, and turning user input into
//! outgoing commands.

use super::state::*;
use crate::irc::{Event, MsgMeta, Outgoing, TypingState};

/// How many recent lines back we look to decide whether a nick has "spoken
/// recently" before surfacing their nick-change in a channel.
const NICK_ACTIVITY_WINDOW: usize = 200;
/// How many messages to request per CHATHISTORY page (LATEST and BEFORE).
const HISTORY_LIMIT: u32 = 50;

/// True when an event arrived inside a chathistory batch — i.e. it is replayed
/// history (draft/chathistory + draft/event-playback), not a live event. Such
/// JOIN/PART/QUIT events are stale membership churn and must not mutate the live
/// roster; only real-time presence and the NAMES burst are authoritative.
fn is_playback(meta: &MsgMeta) -> bool {
    meta.batch_kind.as_deref() == Some("chathistory")
}

impl App {
    /// True if `body` mentions us or a highlight keyword (case-insensitive,
    /// rough word-boundary match).
    fn is_mention(&self, net_idx: usize, body: &str) -> bool {
        let lower = body.to_lowercase();
        let nick = self.networks[net_idx].nick.to_lowercase();
        let mut keys: Vec<&str> = vec![nick.as_str()];
        for k in &self.highlight_keywords {
            keys.push(k.as_str());
        }
        keys.iter().any(|k| {
            if k.is_empty() {
                return false;
            }
            lower.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
                .any(|w| w == k.to_lowercase())
        })
    }

    fn ignored(&self, nick: &str) -> bool {
        self.ignored_nicks
            .iter()
            .any(|n| n.eq_ignore_ascii_case(nick))
    }

    /// Push a line into a buffer, tracking unread/mention counts relative to
    /// the active buffer.
    fn push_to(&mut self, net_idx: usize, buf_idx: usize, line: Line) {
        let is_active = self.active.net == net_idx && self.active.buf == buf_idx;
        let mention = line.highlight;
        let counts = !is_active && !matches!(line.kind, LineKind::Self_);
        // Desktop notification for a highlight in a buffer you're not looking at.
        // Compute the title/body before `line` moves into the buffer.
        let notify_info = self.notification_for(net_idx, buf_idx, &line, is_active);
        let buf = &mut self.networks[net_idx].buffers[buf_idx];
        buf.push(line);
        if counts {
            buf.unread = buf.unread.saturating_add(1);
            if mention {
                buf.mentions = buf.mentions.saturating_add(1);
            }
        }
        if let Some((title, body)) = notify_info {
            crate::notify::show(title, body);
        }
    }

    /// Title/body for a desktop notification, or `None` when one shouldn't fire.
    /// Two things notify: a private message (it's addressed to you), and a
    /// highlight in a channel — your nick or a watched keyword. Either way we
    /// skip it when notifications are off, when you're already looking at that
    /// buffer, and for anything that isn't a real incoming message from someone
    /// else (our own echo is `Self_`; joins/system lines aren't `Message`).
    fn notification_for(
        &self,
        net_idx: usize,
        buf_idx: usize,
        line: &Line,
        is_active: bool,
    ) -> Option<(String, String)> {
        if !self.notifications || is_active {
            return None;
        }
        let buf = &self.networks[net_idx].buffers[buf_idx];
        let is_pm = matches!(buf.kind, BufferKind::Query)
            && matches!(line.kind, LineKind::Message | LineKind::Action);
        if !line.highlight && !is_pm {
            return None;
        }
        let title = match buf.kind {
            BufferKind::Query => format!("@{}", line.from),
            _ => format!("{} — {}", buf.name, line.from),
        };
        Some((title, line.text.clone()))
    }

    pub fn apply_event(&mut self, net_id: NetId, ev: Event) {
        let Some(ni) = self.net(net_id) else { return };
        match ev {
            Event::Connected => {
                self.networks[ni].conn = ConnState::Connected;
                self.networks[ni]
                    .status_mut()
                    .push(Line::system("connected"));
                // Register buddies for presence.
                let buddies = self.networks[ni].cfg.buddies.clone();
                if !buddies.is_empty() {
                    let _ = self.networks[ni]
                        .out
                        .try_send(Outgoing::Monitor(crate::irc::MonitorCmd::Add(buddies)));
                }
            }
            Event::CapsAcked(caps) => {
                self.networks[ni].caps = caps;
            }
            Event::ConnectError(e) => {
                self.networks[ni].conn = ConnState::Error;
                self.networks[ni]
                    .status_mut()
                    .push(Line::system(format!("connect error: {e}")));
            }
            Event::Disconnected => {
                self.networks[ni].conn = ConnState::Disconnected;
                // Membership is unknown while offline; clear it so the fresh NAMES
                // after reconnect rebuilds it instead of stacking on a stale roster.
                self.networks[ni].members.clear();
                self.networks[ni].status_mut().push(Line::system("disconnected"));
            }
            Event::Reconnecting { in_secs } => {
                self.networks[ni].conn = ConnState::Reconnecting;
                self.networks[ni]
                    .status_mut()
                    .push(Line::system(format!("reconnecting in {in_secs}s")));
            }
            Event::Privmsg { target, nick, body, meta } => {
                self.handle_msg(ni, &target, &nick, &body, &meta, false);
            }
            Event::Action { target, nick, body, meta } => {
                self.handle_msg(ni, &target, &nick, &body, &meta, true);
            }
            Event::Notice { from, text, meta } => {
                if self.ignored(&from) {
                    return;
                }
                let line = Line {
                    time: meta.server_time_hhmm.unwrap_or_default(),
                    kind: LineKind::Notice,
                    from,
                    text,
                    msgid: meta.msgid,
                    highlight: false,
                    reply_to: None,
                };
                // Server notices land in the status buffer.
                self.push_to(ni, 0, line);
            }
            Event::CtcpReply { from, query, args } => {
                self.networks[ni]
                    .status_mut()
                    .push(Line::system(format!("CTCP {query} reply from {from}: {args}")));
            }
            Event::UserJoined { channel, nick, meta, .. } => {
                // History playback (draft/event-playback inside a chathistory
                // batch) replays old JOIN/PART churn — including our own past
                // (re)joins, going back days. It is not live state: applying it
                // would corrupt the roster that NAMES authoritatively builds (a
                // replayed self-JOIN/PART could wipe every member down to us).
                // Drop it — the live roster stands and messages still render via
                // Event::Privmsg.
                if is_playback(&meta) {
                    return;
                }
                let net = &mut self.networks[ni];
                let key = channel.to_lowercase();
                let bi = net.ensure_buffer(&channel, BufferKind::Channel);
                let is_self = nick.eq_ignore_ascii_case(&net.nick);
                let roster = net.members.entry(key).or_default();
                // Never double-list: a netjoin replay or a JOIN that races NAMES
                // can repeat someone already present.
                if !roster.iter().any(|e| e.nick.eq_ignore_ascii_case(&nick)) {
                    roster.push(crate::irc::MemberEntry {
                        nick: nick.clone(),
                        prefixes: String::new(),
                        userhost: None,
                        is_bot: false,
                        is_away: false,
                    });
                }
                // Our own join (including bouncer reconnect): pull recent backlog
                // and WHO the whole channel so we learn which members are bots
                // (NAMES can't carry the bot flag; only WHO's flags field does).
                // A live join by someone else: WHO just the newcomer, so their bot
                // badge appears immediately instead of only after a rejoin.
                if is_self {
                    self.request_latest_history(ni, bi);
                    self.request_who(ni, &channel);
                } else {
                    self.request_who(ni, &nick);
                }
                let net = &mut self.networks[ni];
                if !is_self {
                    let line = Line {
                        time: String::new(),
                        kind: LineKind::Join,
                        from: nick.clone(),
                        text: format!("{nick} joined {channel}"),
                        msgid: None,
                        highlight: false,
                        reply_to: None,
                    };
                    net.buffers[bi].push(line);
                }
            }
            Event::UserLeft { channel, nick, meta } => {
                // Skip replayed history churn (see UserJoined).
                if is_playback(&meta) {
                    return;
                }
                let net = &mut self.networks[ni];
                let key = channel.to_lowercase();
                let is_self = nick.eq_ignore_ascii_case(&net.nick);
                if let Some(m) = net.members.get_mut(&key) {
                    if is_self {
                        // We really left (this is live, not playback): the whole
                        // roster is stale, so drop it and let a later rejoin
                        // rebuild cleanly from a fresh NAMES.
                        m.clear();
                    } else {
                        m.retain(|e| !e.nick.eq_ignore_ascii_case(&nick));
                    }
                }
                if let Some(bi) = net.find_buffer(&channel) {
                    net.buffers[bi].push(Line {
                        time: String::new(),
                        kind: LineKind::Part,
                        from: nick.clone(),
                        text: format!("{nick} left {channel}"),
                        msgid: None,
                        highlight: false,
                        reply_to: None,
                    });
                }
            }
            Event::UserQuit { nick, reason, meta } => {
                // Skip replayed history churn (see UserJoined).
                if is_playback(&meta) {
                    return;
                }
                let net = &mut self.networks[ni];
                let text = match &reason {
                    Some(r) => format!("{nick} quit ({r})"),
                    None => format!("{nick} quit"),
                };
                let mut touched = Vec::new();
                for (key, m) in net.members.iter_mut() {
                    if m.iter().any(|e| e.nick.eq_ignore_ascii_case(&nick)) {
                        m.retain(|e| !e.nick.eq_ignore_ascii_case(&nick));
                        touched.push(key.clone());
                    }
                }
                for key in touched {
                    if let Some(bi) = net.buffers.iter().position(|b| b.name.eq_ignore_ascii_case(&key)) {
                        net.buffers[bi].push(Line {
                            time: String::new(),
                            kind: LineKind::Quit,
                            from: nick.clone(),
                            text: text.clone(),
                            msgid: None,
                            highlight: false,
                            reply_to: None,
                        });
                    }
                }
            }
            Event::NickChanged { old, new, .. } => {
                let net = &mut self.networks[ni];
                let is_self = old.eq_ignore_ascii_case(&net.nick);
                if is_self {
                    net.nick = new.clone();
                }
                for m in net.members.values_mut() {
                    for e in m.iter_mut() {
                        if e.nick.eq_ignore_ascii_case(&old) {
                            e.nick = new.clone();
                        }
                    }
                }
                let text = format!("{old} is now known as {new}");
                for bi in 0..net.buffers.len() {
                    // Only surface a rename where it's actually meaningful: our
                    // own nick change, the query buffer with that person, or a
                    // channel where they've spoken recently. In a 1000-person
                    // channel this hides the constant churn from lurkers.
                    let relevant = is_self
                        || net.buffers[bi].name.eq_ignore_ascii_case(&old)
                        || net.buffers[bi].name.eq_ignore_ascii_case(&new)
                        || net.buffers[bi].spoke_recently(&old, NICK_ACTIVITY_WINDOW);
                    if !relevant {
                        continue;
                    }
                    net.buffers[bi].push(Line {
                        time: String::new(),
                        kind: LineKind::System,
                        from: "*".into(),
                        text: text.clone(),
                        msgid: None,
                        highlight: false,
                        reply_to: None,
                    });
                }
            }
            Event::Names { channel, members } => {
                let net = &mut self.networks[ni];
                net.ensure_buffer(&channel, BufferKind::Channel);
                // NAMES arrives split across many 353 lines, each its own event, so
                // append and dedup rather than replace — otherwise only the last
                // slice would survive. NAMES is authoritative for prefixes, so it
                // upgrades the blank prefix a live JOIN left behind.
                let roster = net.members.entry(channel.to_lowercase()).or_default();
                for m in members {
                    if let Some(existing) =
                        roster.iter_mut().find(|e| e.nick.eq_ignore_ascii_case(&m.nick))
                    {
                        existing.prefixes = m.prefixes;
                        if m.userhost.is_some() {
                            existing.userhost = m.userhost;
                        }
                    } else {
                        roster.push(m);
                    }
                }
            }
            Event::WhoReply { channel, nick, is_bot, is_away } => {
                // Annotate an existing member only — WHO doesn't define membership
                // (NAMES does), it just enriches it. Fast path: a channel WHO's
                // reply names its channel, so update that roster directly.
                let net = &mut self.networks[ni];
                if let Some(roster) = net.members.get_mut(&channel.to_lowercase())
                    && let Some(m) = roster.iter_mut().find(|e| e.nick.eq_ignore_ascii_case(&nick))
                {
                    m.is_bot = is_bot;
                    m.is_away = is_away;
                    return;
                }
                // A `WHO <nick>` reply can carry `*` or a channel we don't track for
                // its channel field. Bot-mode and away are global user properties, so
                // set the flags wherever that nick appears in this network's rosters.
                for roster in net.members.values_mut() {
                    if let Some(m) = roster.iter_mut().find(|e| e.nick.eq_ignore_ascii_case(&nick)) {
                        m.is_bot = is_bot;
                        m.is_away = is_away;
                    }
                }
            }
            Event::Topic { channel, topic } => {
                let net = &mut self.networks[ni];
                let bi = net.ensure_buffer(&channel, BufferKind::Channel);
                net.buffers[bi].topic = Some(topic.clone());
                net.buffers[bi].push(Line::system(format!("topic: {topic}")));
            }
            Event::ISupport(is) => {
                self.networks[ni].isupport = is;
            }
            Event::TypingChanged { target, nick, state } => {
                let net = &mut self.networks[ni];
                // Ignore our own typing echoed back (echo-message networks).
                if nick.eq_ignore_ascii_case(&net.nick) {
                    return;
                }
                let bname = if net.is_channel(&target) { target } else { nick.clone() };
                if let Some(bi) = net.find_buffer(&bname) {
                    let typing = &mut net.buffers[bi].typing;
                    typing.retain(|(n, _)| !n.eq_ignore_ascii_case(&nick));
                    if matches!(state, TypingState::Active) {
                        typing.push((nick, std::time::Instant::now()));
                    }
                }
            }
            Event::AccountChanged { nick, account, .. } => {
                let text = match account {
                    Some(a) => format!("{nick} is now logged in as {a}"),
                    None => format!("{nick} logged out of services"),
                };
                self.push_to_member_channels(ni, &nick, &text);
            }
            Event::HostChanged { nick, ident, host, .. } => {
                let text = format!("{nick} changed host to {ident}@{host}");
                self.push_to_member_channels(ni, &nick, &text);
            }
            Event::AwayChanged { nick, message, .. } => {
                // away-notify is intentionally not rendered as chat lines — it is
                // far too chatty in busy channels. Instead we track presence: flip
                // the member's away flag (a non-empty message means gone) across
                // every roster this nick appears in, so the panel dims them live.
                let away = message.is_some();
                for roster in self.networks[ni].members.values_mut() {
                    if let Some(m) = roster.iter_mut().find(|e| e.nick.eq_ignore_ascii_case(&nick)) {
                        m.is_away = away;
                    }
                }
            }
            Event::Redacted { target, msgid, by_nick, reason } => {
                let net = &mut self.networks[ni];
                if let Some(bi) = net.find_buffer(&target) {
                    let buf = &mut net.buffers[bi];
                    // A redact of a reaction message is an "unreact": drop that
                    // reaction and stop — it never had a content line to blank.
                    if buf.remove_reaction_by_msgid(&msgid) {
                        return;
                    }
                    if let Some(line) = buf.lines.iter_mut().find(|l| l.msgid.as_deref() == Some(msgid.as_str())) {
                        let who = if by_nick == line.from { "(deleted)".to_string() } else { format!("(deleted by {by_nick})") };
                        line.text = match &reason {
                            Some(r) => format!("{who}: {r}"),
                            None => who,
                        };
                        line.kind = LineKind::System;
                        line.highlight = false;
                    }
                }
            }
            Event::Reaction { target, target_msgid, nick, emoji, msgid } => {
                let net = &mut self.networks[ni];
                let bname = if net.is_channel(&target) { target.clone() } else { nick.clone() };
                if let Some(bi) = net.find_buffer(&bname) {
                    // Attach the reaction to its message; rendered as a badge. The
                    // reaction message's own msgid is indexed so a later REDACT
                    // ("unreact") can undo exactly this reaction.
                    net.buffers[bi].add_reaction(target_msgid, emoji, nick, msgid);
                }
            }
            Event::ReadMarker { .. } => {
                // No dedicated UI yet; safely ignored.
            }
            Event::ChatHistoryBatchEnd { target } => self.finish_history_batch(ni, &target),
            Event::Presence { nicks, online } => {
                let net = &mut self.networks[ni];
                for n in nicks {
                    if online {
                        net.online_buddies.insert(n.to_lowercase());
                    } else {
                        net.online_buddies.remove(&n.to_lowercase());
                    }
                }
            }
        }
    }

    /// Push a subtle system line to every channel buffer where `nick` is a
    /// member (used for account/host changes).
    fn push_to_member_channels(&mut self, ni: usize, nick: &str, text: &str) {
        let net = &mut self.networks[ni];
        let chans: Vec<String> = net
            .members
            .iter()
            .filter(|(_, m)| m.iter().any(|e| e.nick.eq_ignore_ascii_case(nick)))
            .map(|(k, _)| k.clone())
            .collect();
        for key in chans {
            if let Some(bi) = net.buffers.iter().position(|b| b.name.eq_ignore_ascii_case(&key)) {
                net.buffers[bi].push(Line::system(text.to_string()));
            }
        }
    }

    fn handle_msg(
        &mut self,
        ni: usize,
        target: &str,
        nick: &str,
        body: &str,
        meta: &MsgMeta,
        action: bool,
    ) {
        if self.ignored(nick) {
            return;
        }
        let our_nick = self.networks[ni].nick.clone();
        let is_self = nick.eq_ignore_ascii_case(&our_nick);
        // Determine the buffer: a channel target maps to itself; a DM to us
        // maps to the sender; our own echo to a DM maps to the recipient.
        let bname = if self.networks[ni].is_channel(target) {
            target.to_string()
        } else if target.eq_ignore_ascii_case(&our_nick) {
            nick.to_string()
        } else {
            target.to_string()
        };
        let kind = if self.networks[ni].is_channel(&bname) {
            BufferKind::Channel
        } else {
            BufferKind::Query
        };
        let bi = self.networks[ni].ensure_buffer(&bname, kind);
        let highlight = !is_self && self.is_mention(ni, body);
        // If this is the echo of one of our own replies and the server didn't
        // echo the +draft/reply tag back, re-apply it from the pending list.
        let mut reply_to = meta.reply_to_msgid.clone();
        if is_self && reply_to.is_none() {
            let tgt_lc = bname.to_lowercase();
            if let Some(pos) = self.networks[ni]
                .pending_replies
                .iter()
                .position(|(t, txt, _)| *t == tgt_lc && txt == body)
            {
                let (_, _, parent) = self.networks[ni].pending_replies.remove(pos);
                reply_to = Some(parent);
            }
        }
        let line = Line {
            time: meta.server_time_hhmm.clone().unwrap_or_default(),
            kind: if is_self {
                LineKind::Self_
            } else if action {
                LineKind::Action
            } else {
                LineKind::Message
            },
            from: nick.to_string(),
            text: body.to_string(),
            msgid: meta.msgid.clone(),
            highlight,
            reply_to,
        };
        // Replayed history (an open `chathistory` batch): stage it for a
        // chronological prepend when the batch closes, deduped by msgid. It must
        // not bump unread, clear typing, or count as a live mention.
        if meta.batch_kind.as_deref() == Some("chathistory") {
            let buf = &mut self.networks[ni].buffers[bi];
            if let Some(id) = &line.msgid {
                let dup = buf.lines.iter().any(|l| l.msgid.as_deref() == Some(id))
                    || buf.history_stage.iter().any(|l| l.msgid.as_deref() == Some(id));
                if dup {
                    return;
                }
            }
            if buf.history_stage.is_empty() {
                buf.history_stage_oldest_ts = meta.server_time_iso.clone();
            }
            buf.history_stage.push(line);
            return;
        }
        // Live message: drop duplicates of one we already have (history overlap
        // or a double echo). msgids are temporally local, so a recent window
        // is enough.
        if let Some(id) = &line.msgid {
            let buf = &self.networks[ni].buffers[bi];
            if buf.lines.iter().rev().take(400).any(|l| l.msgid.as_deref() == Some(id)) {
                return;
            }
        }
        // Clear the sender from the typing list.
        let typing = &mut self.networks[ni].buffers[bi].typing;
        typing.retain(|(n, _)| !n.eq_ignore_ascii_case(nick));
        self.push_to(ni, bi, line);
    }

    /// Request the latest backlog for a buffer once (from the bouncer/server via
    /// `CHATHISTORY LATEST`). No-op for the status buffer, when already loaded /
    /// loading, or when the network didn't negotiate `draft/chathistory`.
    fn request_latest_history(&mut self, ni: usize, bi: usize) {
        if ni >= self.networks.len() || bi == 0 {
            return;
        }
        let net = &mut self.networks[ni];
        if bi >= net.buffers.len() || !net.caps.iter().any(|c| c == "draft/chathistory") {
            return;
        }
        let buf = &mut net.buffers[bi];
        if buf.history_loaded
            || buf.history_loading
            || !matches!(buf.kind, BufferKind::Channel | BufferKind::Query)
        {
            return;
        }
        buf.history_loaded = true;
        buf.history_loading = true;
        let target = buf.name.clone();
        let _ = net.out.try_send(Outgoing::ChatHistoryLatest {
            target,
            limit: HISTORY_LIMIT,
        });
    }

    /// Ask the server for a WHO of `target` (a channel or a single nick) so member
    /// bot and away flags can be filled in. Fire-and-forget: the replies come back
    /// as `Event::WhoReply` and annotate the roster NAMES built. Skipped when
    /// neither consumer is active — no bot-mode advertised and no `away-notify`
    /// acked — since the reply would tell us nothing we render.
    fn request_who(&mut self, ni: usize, target: &str) {
        if ni >= self.networks.len() {
            return;
        }
        let net = &self.networks[ni];
        let wants_who = net.isupport.bot_mode.is_some()
            || net.caps.iter().any(|c| c == "away-notify");
        if !wants_who {
            return;
        }
        let _ = self.networks[ni].out.try_send(Outgoing::Raw {
            cmd: "WHO".into(),
            args: vec![target.to_string()],
        });
    }

    /// Ensure the active buffer has requested its initial backlog. Called from
    /// the main loop so it covers channels, queries, and manual buffer switches.
    pub fn maybe_load_active_history(&mut self) {
        self.request_latest_history(self.active.net, self.active.buf);
    }

    /// Act on the renderer's "scrolled to the top" flag by fetching the next
    /// older page (`CHATHISTORY BEFORE` the oldest message we hold).
    pub fn request_older_history(&mut self) {
        let (ni, bi) = (self.active.net, self.active.buf);
        if ni >= self.networks.len() {
            return;
        }
        let net = &mut self.networks[ni];
        if bi >= net.buffers.len() {
            return;
        }
        let buf = &mut net.buffers[bi];
        if !std::mem::take(&mut buf.request_older)
            || buf.history_loading
            || buf.history_exhausted
            || !buf.history_loaded
        {
            return;
        }
        let Some(before_ts) = buf.oldest_history_ts.clone() else {
            return;
        };
        if !net.caps.iter().any(|c| c == "draft/chathistory") {
            return;
        }
        buf.history_loading = true;
        let target = buf.name.clone();
        let _ = net.out.try_send(Outgoing::ChatHistoryBefore {
            target,
            before_ts,
            limit: HISTORY_LIMIT,
        });
    }

    /// Finalize an open `chathistory` batch: prepend the staged messages (in
    /// chronological order) ahead of the buffer's existing lines, advance the
    /// paging anchor, and mark exhaustion when the server returned a short page.
    fn finish_history_batch(&mut self, ni: usize, target: &str) {
        let net = &mut self.networks[ni];
        let Some(bi) = net.find_buffer(target) else {
            return;
        };
        let buf = &mut net.buffers[bi];
        buf.history_loading = false;
        let staged = std::mem::take(&mut buf.history_stage);
        let oldest = buf.history_stage_oldest_ts.take();
        if staged.len() < HISTORY_LIMIT as usize {
            buf.history_exhausted = true;
        }
        if staged.is_empty() {
            return;
        }
        if oldest.is_some() {
            buf.oldest_history_ts = oldest;
        }
        // Prepend: staged (older) first, then the existing lines. Scroll is left
        // untouched — it's measured from the bottom, so the visible region stays
        // put, and the user simply gains scrollback above.
        let mut combined = staged;
        combined.append(&mut buf.lines);
        buf.lines = combined;
    }

    /// Submit the current input line. Returns false on no-op.
    pub fn submit_input(&mut self) {
        let text = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.completion = None;
        let text = text.trim_end_matches('\n');
        // React mode: the line is an emoji reaction to the selected/last message.
        if self.react_mode {
            self.react_mode = false;
            let emoji = text.trim();
            if !emoji.is_empty() {
                self.react_to_target(emoji.to_string());
            }
            return;
        }
        if text.is_empty() {
            return;
        }
        if let Some(cmd) = text.strip_prefix('/') {
            self.run_command(cmd);
        } else if let Some(parent) = self.active_buffer().and_then(|b| b.selection.clone()) {
            // A message is selected: Enter sends the text as a reply to it.
            self.send_reply(parent, text.to_string());
        } else {
            self.send_message(text.to_string());
        }
    }

    /// Send `text` as a threaded reply to the message `parent` (a msgid) in the
    /// active buffer. Clears the selection afterwards.
    pub fn send_reply(&mut self, parent: String, text: String) {
        let Some(target) = self.active_target() else { return };
        let ni = self.active.net;
        if matches!(self.active_buffer().map(|b| b.kind), Some(BufferKind::Status)) {
            self.set_status("cannot reply in the status buffer");
            return;
        }
        let _ = self.networks[ni].out.try_send(Outgoing::PrivmsgReply {
            target: target.clone(),
            text: text.clone(),
            reply_to_msgid: parent.clone(),
        });
        let bi = self.active.buf;
        if self.networks[ni].caps.iter().any(|c| c == "echo-message") {
            self.networks[ni]
                .pending_replies
                .push((target.to_lowercase(), text, parent));
        } else {
            let nick = self.networks[ni].nick.clone();
            self.networks[ni].buffers[bi].push(Line {
                time: String::new(),
                kind: LineKind::Self_,
                from: nick,
                text,
                msgid: None,
                highlight: false,
                reply_to: Some(parent),
            });
        }
        // Drop the selection once we've replied.
        if let Some(b) = self.active_buffer_mut() {
            b.selection = None;
        }
    }

    fn send_message(&mut self, text: String) {
        let Some(target) = self.active_target() else { return };
        let ni = self.active.net;
        if matches!(self.active_buffer().map(|b| b.kind), Some(BufferKind::Status)) {
            self.set_status("cannot send to the status buffer; /join a channel first");
            return;
        }
        let our_nick = self.networks[ni].nick.clone();
        let _ = self.networks[ni]
            .out
            .try_send(Outgoing::Privmsg { target: target.clone(), text: text.clone() });
        // If the server echoes our message (echo-message), it'll arrive as a
        // PRIVMSG from us; otherwise render it locally now.
        if !self.networks[ni].caps.iter().any(|c| c == "echo-message") {
            let bi = self.active.buf;
            self.networks[ni].buffers[bi].push(Line {
                time: String::new(),
                kind: LineKind::Self_,
                from: our_nick,
                text,
                msgid: None,
                highlight: false,
                reply_to: None,
            });
        }
    }

    /// React with `emoji` to the selected message (or the last one if none is
    /// selected). The selection is kept so further reactions are quick.
    pub fn react_to_target(&mut self, emoji: String) {
        let ni = self.active.net;
        if self.networks[ni].isupport.client_tag_denied("draft/react") {
            self.set_status("this server does not allow reactions");
            return;
        }
        let Some(target) = self.active_target() else { return };
        let Some(msgid) = self.selected_or_last_msgid() else {
            self.set_status("no message to react to");
            return;
        };
        let _ = self.networks[ni].out.try_send(Outgoing::React { target, msgid, emoji });
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_msg = Some(msg.into());
    }

    /// Announce to the active channel/query that we're composing a message
    /// (`draft/typing` `+typing=active`), throttled to once per 3s per the
    /// spec. No-op in the status buffer, or when the network can't carry it.
    pub fn notify_typing(&mut self) {
        let ni = self.active.net;
        let Some(net) = self.networks.get(ni) else { return };
        // Typing tags ride on message-tags; respect CLIENTTAGDENY.
        if !net.caps.iter().any(|c| c == "message-tags") {
            return;
        }
        if net.isupport.client_tag_denied("typing") {
            return;
        }
        let Some(buf) = self.active_buffer() else { return };
        if matches!(buf.kind, BufferKind::Status) {
            return;
        }
        let target = buf.name.clone();
        let now = std::time::Instant::now();
        let fresh = self
            .typing_throttle
            .map(|t| now.duration_since(t).as_secs() >= 3)
            .unwrap_or(true);
        if !fresh {
            return;
        }
        self.typing_throttle = Some(now);
        let _ = self.networks[ni].out.try_send(Outgoing::Typing {
            target,
            state: TypingState::Active,
        });
    }

    /// Tell the active target we've stopped typing (`+typing=done`). No-op
    /// unless we'd previously announced active.
    pub fn stop_typing(&mut self) {
        if self.typing_throttle.take().is_none() {
            return;
        }
        let ni = self.active.net;
        let Some(buf) = self.active_buffer() else { return };
        if matches!(buf.kind, BufferKind::Status) {
            return;
        }
        let target = buf.name.clone();
        let _ = self.networks[ni].out.try_send(Outgoing::Typing {
            target,
            state: TypingState::Done,
        });
    }

    /// When the soonest-due visible animation frame should be shown, if any
    /// animated image is currently on screen. The main loop waits until then (or
    /// the next event, whichever comes first) so animations advance on their own.
    pub fn next_anim_due(&self) -> Option<std::time::Instant> {
        self.visible_anims
            .iter()
            .filter_map(|url| match self.images.map.get(url) {
                Some(crate::images::ImageState::Anim { next_due, .. }) => Some(*next_due),
                _ => None,
            })
            .min()
    }

    /// Advance every visible animation whose current frame is due, wrapping at the
    /// end of the loop. Returns true if any frame changed, so the caller redraws.
    pub fn advance_anims(&mut self) -> bool {
        let now = std::time::Instant::now();
        let mut changed = false;
        for url in &self.visible_anims {
            if let Some(crate::images::ImageState::Anim { frames, delays, idx, next_due, .. }) =
                self.images.map.get_mut(url)
                && now >= *next_due
                && !frames.is_empty()
            {
                *idx = (*idx + 1) % frames.len();
                *next_due = now + delays[*idx];
                changed = true;
            }
        }
        changed
    }

    /// Drop typing indicators whose last `+typing=active` is older than the
    /// spec's window. A sender refreshes `active` every 3s while typing, so 6s
    /// without a refresh means they stopped (or the `done`/peer was lost) and
    /// the "… is typing" line would otherwise stick forever. Returns true if
    /// anything was removed, so the caller knows a redraw is warranted.
    pub fn expire_typing(&mut self) -> bool {
        let now = std::time::Instant::now();
        let mut changed = false;
        for net in &mut self.networks {
            for buf in &mut net.buffers {
                let before = buf.typing.len();
                buf.typing
                    .retain(|(_, since)| now.duration_since(*since).as_secs() < 6);
                changed |= buf.typing.len() != before;
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, NetworkConfig};
    use crate::images::Images;
    use ratatui_image::picker::Picker;
    use tokio::sync::mpsc;

    fn test_app() -> App {
        let (img_tx, _r) = mpsc::channel(1);
        let images = Images::new(Picker::from_fontsize((8, 16)), img_tx);
        let mut app = App::new(AppConfig::default(), images);
        let cfg = NetworkConfig {
            name: "test".into(),
            nickname: "me".into(),
            username: None,
            realname: None,
            server: "irc.test".into(),
            port: 6697,
            use_tls: true,
            nick_password: None,
            sasl_username: None,
            sasl_password: None,
            client_cert_path: None,
            client_cert_pass: None,
            channels: vec![],
            buddies: vec![],
            autoconnect: true,
        };
        let (out, _r2) = mpsc::channel(8);
        // Leak the receivers so senders stay open for the test's lifetime.
        std::mem::forget(_r);
        std::mem::forget(_r2);
        app.networks.push(Network::new(0, cfg, out));
        // Default ISUPPORT has empty chantypes; is_channel falls back to "#&".
        app
    }

    fn meta() -> MsgMeta {
        MsgMeta::default()
    }

    #[test]
    fn join_creates_channel_buffer() {
        let mut app = test_app();
        app.apply_event(0, Event::UserJoined {
            channel: "#rust".into(),
            nick: "alice".into(),
            userhost: None,
            account: None,
            realname: None,
            meta: meta(),
        });
        let net = &app.networks[0];
        assert!(net.find_buffer("#rust").is_some(), "channel buffer should exist");
        assert!(net.members.get("#rust").map(|m| m.len()).unwrap_or(0) >= 1);
    }

    fn names(channel: &str, nicks: &[&str]) -> Event {
        Event::Names {
            channel: channel.into(),
            members: nicks
                .iter()
                .map(|n| crate::irc::MemberEntry {
                    nick: (*n).to_string(),
                    prefixes: String::new(),
                    userhost: None,
                    is_bot: false,
                    is_away: false,
                })
                .collect(),
        }
    }

    fn joins(app: &mut App, channel: &str, nick: &str) {
        app.apply_event(0, Event::UserJoined {
            channel: channel.into(),
            nick: nick.into(),
            userhost: None,
            account: None,
            realname: None,
            meta: meta(),
        });
    }

    // A big channel's NAMES arrives across many 353 lines, each its own event;
    // the roster must accumulate them rather than keep only the last slice.
    #[test]
    fn names_accumulate_across_multiple_353_events() {
        let mut app = test_app();
        joins(&mut app, "#c", "me"); // our own join opens the buffer
        app.apply_event(0, names("#c", &["me", "a", "b"]));
        app.apply_event(0, names("#c", &["c", "d", "e"]));
        let roster = &app.networks[0].members["#c"];
        assert_eq!(roster.len(), 6, "both 353 slices plus us are kept");
        for n in ["me", "a", "b", "c", "d", "e"] {
            assert!(roster.iter().any(|m| m.nick == n), "{n} present");
        }
    }

    // A WHO reply annotates an existing member's bot flag, ignores unknown nicks,
    // and never adds membership (NAMES is authoritative for that).
    #[test]
    fn who_reply_sets_bot_flag_on_known_member_only() {
        let mut app = test_app();
        joins(&mut app, "#c", "me");
        app.apply_event(0, names("#c", &["me", "botnick", "alice"]));
        app.apply_event(0, Event::WhoReply { channel: "#c".into(), nick: "botnick".into(), is_bot: true, is_away: false });
        app.apply_event(0, Event::WhoReply { channel: "#c".into(), nick: "alice".into(), is_bot: false, is_away: false });
        // A WHO row for a nick NAMES never listed must not appear in the roster.
        app.apply_event(0, Event::WhoReply { channel: "#c".into(), nick: "ghost".into(), is_bot: true, is_away: false });

        let roster = &app.networks[0].members["#c"];
        assert_eq!(roster.len(), 3, "WHO never adds membership");
        assert!(roster.iter().find(|m| m.nick == "botnick").unwrap().is_bot);
        assert!(!roster.iter().find(|m| m.nick == "alice").unwrap().is_bot);
        assert!(!roster.iter().any(|m| m.nick == "ghost"));
    }

    // A `WHO <nick>` reply may carry `*` as its channel; the global fallback must
    // still set the bot flag wherever the nick is a member.
    #[test]
    fn who_reply_with_star_channel_sets_bot_via_fallback() {
        let mut app = test_app();
        joins(&mut app, "#c", "me");
        app.apply_event(0, names("#c", &["me", "botnick"]));
        app.apply_event(0, Event::WhoReply { channel: "*".into(), nick: "botnick".into(), is_bot: true, is_away: false });
        assert!(app.networks[0].members["#c"].iter().find(|m| m.nick == "botnick").unwrap().is_bot);
    }

    // NAMES repeating a nick, or a JOIN racing NAMES, must not double-list anyone.
    #[test]
    fn roster_never_double_lists_a_nick() {
        let mut app = test_app();
        joins(&mut app, "#c", "me");
        app.apply_event(0, names("#c", &["alice", "bob"]));
        app.apply_event(0, names("#c", &["alice"])); // duplicate slice
        joins(&mut app, "#c", "bob"); // JOIN that races the roster
        let roster = &app.networks[0].members["#c"];
        assert_eq!(roster.iter().filter(|m| m.nick == "alice").count(), 1);
        assert_eq!(roster.iter().filter(|m| m.nick == "bob").count(), 1);
    }

    // A reconnect (or bouncer re-attach) must not leave a stale, doubled roster:
    // disconnect clears it and our own re-JOIN starts it fresh for the new NAMES.
    #[test]
    fn reconnect_rebuilds_roster_without_duplicates() {
        let mut app = test_app();
        joins(&mut app, "#c", "me");
        app.apply_event(0, names("#c", &["me", "alice", "bob"]));
        assert_eq!(app.networks[0].members["#c"].len(), 3);

        app.apply_event(0, Event::Disconnected);
        assert!(app.networks[0].members.get("#c").map(|m| m.is_empty()).unwrap_or(true));

        // Reconnect: we rejoin and the server resends NAMES.
        joins(&mut app, "#c", "me");
        app.apply_event(0, names("#c", &["me", "alice", "bob"]));
        let roster = &app.networks[0].members["#c"];
        assert_eq!(roster.len(), 3, "roster rebuilt cleanly, nobody doubled");
        assert_eq!(roster.iter().filter(|m| m.nick == "me").count(), 1);
    }

    // Meta for a JOIN/PART/QUIT replayed inside a chathistory batch (bouncer
    // event-playback) — the exact shape that corrupted the roster.
    fn playback_meta() -> MsgMeta {
        hist_meta("2026-07-01T20:00:00Z", "pb")
    }

    fn pb_join(app: &mut App, channel: &str, nick: &str) {
        app.apply_event(0, Event::UserJoined {
            channel: channel.into(),
            nick: nick.into(),
            userhost: None,
            account: None,
            realname: None,
            meta: playback_meta(),
        });
    }

    fn pb_part(app: &mut App, channel: &str, nick: &str) {
        app.apply_event(0, Event::UserLeft {
            channel: channel.into(),
            nick: nick.into(),
            meta: playback_meta(),
        });
    }

    // A bouncer replays our historical JOIN/PART churn via event-playback, long
    // after the live NAMES filled the roster. Those replayed events must not
    // touch the roster — the 13-vs-1400 / "only me" bug.
    #[test]
    fn playback_join_part_never_mutates_live_roster() {
        let mut app = test_app();
        joins(&mut app, "#c", "me"); // real join
        app.apply_event(0, names("#c", &["me", "alice", "bob", "carol", "dave"]));
        assert_eq!(app.networks[0].members["#c"].len(), 5);

        // The whole replayed history burst: our own join, our own part (this is
        // what used to wipe it to 0/1), plus a present member's part.
        pb_join(&mut app, "#c", "me");
        pb_part(&mut app, "#c", "me");
        pb_join(&mut app, "#c", "me");
        pb_part(&mut app, "#c", "alice"); // alice is really here — must not vanish

        let roster = &app.networks[0].members["#c"];
        assert_eq!(roster.len(), 5, "playback must leave the live roster untouched");
        assert!(roster.iter().any(|m| m.nick == "alice"), "present member kept");
        assert_eq!(roster.iter().filter(|m| m.nick == "me").count(), 1);
    }

    // A *live* self-PART (not playback) drops the roster, so a later rejoin
    // rebuilds from a fresh NAMES instead of stacking ghosts.
    #[test]
    fn live_self_part_clears_roster() {
        let mut app = test_app();
        joins(&mut app, "#c", "me");
        app.apply_event(0, names("#c", &["me", "alice", "bob"]));
        assert_eq!(app.networks[0].members["#c"].len(), 3);

        app.apply_event(0, Event::UserLeft {
            channel: "#c".into(),
            nick: "me".into(),
            meta: meta(), // batch_kind None → live
        });
        assert!(
            app.networks[0].members["#c"].is_empty(),
            "our own live PART clears the whole roster"
        );
    }

    fn hist_meta(iso: &str, msgid: &str) -> MsgMeta {
        MsgMeta {
            server_time_hhmm: Some("00:00".into()),
            server_time_iso: Some(iso.into()),
            msgid: Some(msgid.into()),
            batch_kind: Some("chathistory".into()),
            ..MsgMeta::default()
        }
    }

    fn hist_msg(app: &mut App, target: &str, nick: &str, body: &str, iso: &str, id: &str) {
        app.apply_event(0, Event::Privmsg {
            target: target.into(),
            nick: nick.into(),
            body: body.into(),
            meta: hist_meta(iso, id),
        });
    }

    #[test]
    fn chathistory_batch_prepends_in_order_and_dedupes() {
        let mut app = test_app();
        let bi = app.networks[0].ensure_buffer("#rust", BufferKind::Channel);
        // A live message arrives first.
        app.apply_event(0, Event::Privmsg {
            target: "#rust".into(), nick: "carol".into(), body: "live".into(),
            meta: MsgMeta { msgid: Some("C".into()), ..MsgMeta::default() },
        });
        // A chathistory batch (older, chronological) — including a dup of the
        // live message, which must be dropped.
        hist_msg(&mut app, "#rust", "alice", "first", "t1", "A");
        hist_msg(&mut app, "#rust", "bob", "second", "t2", "B");
        hist_msg(&mut app, "#rust", "carol", "live", "t3", "C"); // dup msgid
        // Nothing committed until the batch closes.
        assert_eq!(app.networks[0].buffers[bi].lines.len(), 1);
        app.apply_event(0, Event::ChatHistoryBatchEnd { target: "#rust".into() });

        let buf = &app.networks[0].buffers[bi];
        let texts: Vec<&str> = buf.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["first", "second", "live"]); // history prepended, dup gone
        assert_eq!(buf.oldest_history_ts.as_deref(), Some("t1"));
        assert!(!buf.history_loading);
        assert!(buf.history_exhausted); // 2 staged < HISTORY_LIMIT
    }

    #[test]
    fn older_page_prepends_before_existing_history() {
        let mut app = test_app();
        let bi = app.networks[0].ensure_buffer("#rust", BufferKind::Channel);
        hist_msg(&mut app, "#rust", "a", "mid", "t5", "M");
        app.apply_event(0, Event::ChatHistoryBatchEnd { target: "#rust".into() });
        // A BEFORE page brings an older message.
        hist_msg(&mut app, "#rust", "a", "old", "t1", "O");
        app.apply_event(0, Event::ChatHistoryBatchEnd { target: "#rust".into() });

        let buf = &app.networks[0].buffers[bi];
        let texts: Vec<&str> = buf.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["old", "mid"]);
        assert_eq!(buf.oldest_history_ts.as_deref(), Some("t1"));
    }

    #[test]
    fn self_join_requests_latest_history() {
        let (mut app, mut out_rx) = app_with_outbox();
        app.networks[0].caps = vec!["draft/chathistory".into()];
        app.networks[0].nick = "me".into();
        app.apply_event(0, Event::UserJoined {
            channel: "#rust".into(), nick: "me".into(),
            userhost: None, account: None, realname: None, meta: meta(),
        });
        match out_rx.try_recv() {
            Ok(Outgoing::ChatHistoryLatest { target, limit }) => {
                assert_eq!(target, "#rust");
                assert_eq!(limit, HISTORY_LIMIT);
            }
            _ => panic!("expected a CHATHISTORY LATEST on self-join"),
        }
    }

    #[test]
    fn no_history_request_without_the_cap() {
        let (mut app, mut out_rx) = app_with_outbox();
        app.networks[0].nick = "me".into();
        // caps empty -> no draft/chathistory
        app.apply_event(0, Event::UserJoined {
            channel: "#rust".into(), nick: "me".into(),
            userhost: None, account: None, realname: None, meta: meta(),
        });
        // A self-join still WHOs the channel (to learn bot flags), but with no
        // chathistory cap it must never request backlog.
        while let Ok(msg) = out_rx.try_recv() {
            assert!(
                !matches!(msg, Outgoing::ChatHistoryLatest { .. }),
                "must not request history without the cap"
            );
        }
    }

    #[test]
    fn channel_message_routes_to_channel_buffer() {
        let mut app = test_app();
        app.apply_event(0, Event::Privmsg {
            target: "#rust".into(),
            nick: "alice".into(),
            body: "hello world".into(),
            meta: meta(),
        });
        let bi = app.networks[0].find_buffer("#rust").expect("buffer");
        let lines = &app.networks[0].buffers[bi].lines;
        assert_eq!(lines.last().unwrap().text, "hello world");
        assert_eq!(lines.last().unwrap().from, "alice");
    }

    #[test]
    fn dm_routes_to_sender_buffer() {
        let mut app = test_app();
        // A privmsg addressed to us (our nick) opens a query keyed by sender.
        app.apply_event(0, Event::Privmsg {
            target: "me".into(),
            nick: "bob".into(),
            body: "hi".into(),
            meta: meta(),
        });
        assert!(app.networks[0].find_buffer("bob").is_some());
    }

    #[test]
    fn mention_sets_highlight() {
        let mut app = test_app();
        app.apply_event(0, Event::Privmsg {
            target: "#rust".into(),
            nick: "alice".into(),
            body: "me: ping".into(),
            meta: meta(),
        });
        let bi = app.networks[0].find_buffer("#rust").unwrap();
        assert!(app.networks[0].buffers[bi].lines.last().unwrap().highlight);
    }

    #[test]
    fn highlight_keyword_triggers_mention() {
        let mut app = test_app();
        app.highlight_keywords = vec!["irkt".into()];
        app.apply_event(0, Event::Privmsg {
            target: "#rust".into(),
            nick: "alice".into(),
            body: "have you tried IRKT yet?".into(), // case-insensitive, word-boundary
            meta: meta(),
        });
        let bi = app.networks[0].find_buffer("#rust").unwrap();
        assert!(app.networks[0].buffers[bi].lines.last().unwrap().highlight);

        // A keyword embedded inside another word must NOT trigger.
        app.apply_event(0, Event::Privmsg {
            target: "#rust".into(),
            nick: "alice".into(),
            body: "quirktastic".into(),
            meta: meta(),
        });
        assert!(!app.networks[0].buffers[bi].lines.last().unwrap().highlight);
    }

    #[test]
    fn notifies_only_on_inactive_highlight() {
        let mut app = test_app();
        app.notifications = true;
        // Channel buffer (index 1); active buffer is the status buffer (0).
        let bi = app.networks[0].ensure_buffer("#rust", BufferKind::Channel);
        let hl = Line { highlight: true, from: "alice".into(), text: "me: ping".into(), ..Line::system("") };
        // Inactive + highlight → notify, titled "<channel> — <nick>".
        let n = app.notification_for(0, bi, &hl, false);
        assert_eq!(n, Some(("#rust — alice".into(), "me: ping".into())));
        // Same line but the buffer is active → no notification.
        assert!(app.notification_for(0, bi, &hl, true).is_none());
        // Non-highlight line in an inactive buffer → no notification.
        let plain = Line { highlight: false, ..hl.clone() };
        assert!(app.notification_for(0, bi, &plain, false).is_none());
        // Master switch off → nothing, even on an inactive highlight.
        app.notifications = false;
        assert!(app.notification_for(0, bi, &hl, false).is_none());
    }

    #[test]
    fn query_notification_titles_with_nick() {
        let mut app = test_app();
        app.notifications = true;
        let bi = app.networks[0].ensure_buffer("alice", BufferKind::Query);
        let hl = Line { highlight: true, from: "alice".into(), text: "hey".into(), ..Line::system("") };
        let n = app.notification_for(0, bi, &hl, false);
        assert_eq!(n, Some(("@alice".into(), "hey".into())));
    }

    #[test]
    fn plain_pm_notifies_without_a_mention() {
        let mut app = test_app();
        app.notifications = true;
        let bi = app.networks[0].ensure_buffer("alice", BufferKind::Query);
        // A private message that never names you still notifies — it's a PM.
        let pm = Line {
            kind: LineKind::Message,
            highlight: false,
            from: "alice".into(),
            text: "you around?".into(),
            ..Line::system("")
        };
        assert_eq!(
            app.notification_for(0, bi, &pm, false),
            Some(("@alice".into(), "you around?".into()))
        );
        // Our own echo into the query (Self_) must not notify.
        let echo = Line { kind: LineKind::Self_, from: "me".into(), ..pm.clone() };
        assert!(app.notification_for(0, bi, &echo, false).is_none());
        // A system line in the query (e.g. a notice) doesn't either.
        let sys = Line { from: "alice".into(), ..Line::system("alice is away") };
        assert!(app.notification_for(0, bi, &sys, false).is_none());
    }

    #[test]
    fn urls_extracted_from_message() {
        // Extension-less URLs are extracted too; image-ness is decided by MIME.
        let body = "shots https://e.com/a.png and https://i.imgur.com/abc123 ok";
        assert_eq!(
            crate::images::extract_urls(body),
            vec![
                "https://e.com/a.png".to_string(),
                "https://i.imgur.com/abc123".to_string()
            ]
        );
    }

    #[test]
    fn reply_meta_stored_on_line() {
        let mut app = test_app();
        let mut m = meta();
        m.reply_to_msgid = Some("parent123".into());
        app.apply_event(0, Event::Privmsg {
            target: "#rust".into(),
            nick: "alice".into(),
            body: "agreed".into(),
            meta: m,
        });
        let bi = app.networks[0].find_buffer("#rust").unwrap();
        assert_eq!(
            app.networks[0].buffers[bi].lines.last().unwrap().reply_to.as_deref(),
            Some("parent123")
        );
    }

    #[test]
    fn own_reply_threads_even_if_server_strips_tag() {
        let mut app = test_app();
        // Pretend the server negotiated echo-message.
        app.networks[0].caps = vec!["echo-message".into()];
        let bi = app.networks[0].ensure_buffer("#rust", BufferKind::Channel);
        app.networks[0].buffers[bi].push(Line {
            time: "12:00".into(), kind: LineKind::Message, from: "alice".into(),
            text: "question?".into(), msgid: Some("p1".into()), highlight: false, reply_to: None,
        });
        app.active = ActiveBuffer { net: 0, buf: bi };

        // We reply; echo-message means we don't render locally, we wait.
        app.run_command("reply my answer");
        assert_eq!(app.networks[0].pending_replies.len(), 1);

        // The echo comes back as our own PRIVMSG WITHOUT the +draft/reply tag.
        app.apply_event(0, Event::Privmsg {
            target: "#rust".into(),
            nick: "me".into(),
            body: "my answer".into(),
            meta: meta(), // no reply_to_msgid
        });

        let last = app.networks[0].buffers[bi].lines.last().unwrap();
        assert_eq!(last.text, "my answer");
        assert_eq!(last.reply_to.as_deref(), Some("p1"), "reply_to re-applied from pending");
        assert!(app.networks[0].pending_replies.is_empty(), "pending entry consumed");
    }

    #[test]
    fn react_mode_reacts_to_selected_message() {
        let (img_tx, _r) = mpsc::channel(1);
        let images = Images::new(Picker::from_fontsize((8, 16)), img_tx);
        let mut app = App::new(AppConfig::default(), images);
        let cfg = NetworkConfig {
            name: "t".into(), nickname: "me".into(), username: None, realname: None,
            server: "s".into(), port: 6697, use_tls: true, nick_password: None,
            sasl_username: None, sasl_password: None, client_cert_path: None,
            client_cert_pass: None, channels: vec![], buddies: vec![], autoconnect: true,
        };
        let (out, mut out_rx) = mpsc::channel(8);
        std::mem::forget(_r);
        app.networks.push(Network::new(0, cfg, out));
        let bi = app.networks[0].ensure_buffer("#c", BufferKind::Channel);
        for id in ["p1", "p2"] {
            app.networks[0].buffers[bi].push(Line {
                time: String::new(), kind: LineKind::Message, from: "x".into(),
                text: id.into(), msgid: Some(id.into()), highlight: false, reply_to: None,
            });
        }
        app.active = ActiveBuffer { net: 0, buf: bi };
        // Select the older message and react in react mode.
        app.networks[0].buffers[bi].selection = Some("p1".into());
        app.react_mode = true;
        app.input = "👍".into();
        app.cursor = app.input.len();
        app.submit_input();

        assert!(!app.react_mode, "react mode exits after submit");
        // Selection is kept so further reactions are quick.
        assert_eq!(app.networks[0].buffers[bi].selection.as_deref(), Some("p1"));
        match out_rx.try_recv() {
            Ok(Outgoing::React { msgid, emoji, .. }) => {
                assert_eq!(msgid, "p1", "reacts to the selected (older) message");
                assert_eq!(emoji, "👍");
            }
            _ => panic!("expected a React command on the wire"),
        }
    }

    #[test]
    fn message_selection_navigation() {
        let mut app = test_app();
        let bi = app.networks[0].ensure_buffer("#c", BufferKind::Channel);
        for (i, id) in ["a", "b", "c"].iter().enumerate() {
            app.networks[0].buffers[bi].push(Line {
                time: String::new(), kind: LineKind::Message, from: "x".into(),
                text: format!("m{i}"), msgid: Some(id.to_string()), highlight: false, reply_to: None,
            });
        }
        let buf = &mut app.networks[0].buffers[bi];
        assert_eq!(buf.selection, None);
        buf.move_selection(-1); // Alt+Up from nothing -> most recent
        assert_eq!(buf.selection.as_deref(), Some("c"));
        buf.move_selection(-1);
        assert_eq!(buf.selection.as_deref(), Some("b"));
        buf.move_selection(-1);
        assert_eq!(buf.selection.as_deref(), Some("a"));
        buf.move_selection(-1); // clamp at the oldest
        assert_eq!(buf.selection.as_deref(), Some("a"));
        buf.move_selection(1);
        assert_eq!(buf.selection.as_deref(), Some("b"));
        buf.move_selection(1);
        assert_eq!(buf.selection.as_deref(), Some("c"));
        buf.move_selection(1); // past the newest -> deselect
        assert_eq!(buf.selection, None);
    }

    #[test]
    fn enter_replies_to_selected_message() {
        let mut app = test_app();
        app.networks[0].caps = vec!["echo-message".into()];
        let bi = app.networks[0].ensure_buffer("#c", BufferKind::Channel);
        app.networks[0].buffers[bi].push(Line {
            time: String::new(), kind: LineKind::Message, from: "alice".into(),
            text: "older".into(), msgid: Some("p1".into()), highlight: false, reply_to: None,
        });
        app.networks[0].buffers[bi].push(Line {
            time: String::new(), kind: LineKind::Message, from: "bob".into(),
            text: "newer".into(), msgid: Some("p2".into()), highlight: false, reply_to: None,
        });
        app.active = ActiveBuffer { net: 0, buf: bi };
        // Select the OLDER message, then type + Enter.
        app.networks[0].buffers[bi].selection = Some("p1".into());
        app.input = "my reply".into();
        app.cursor = app.input.len();
        app.submit_input();
        // It must thread to the selected (older) message, not the last one,
        // and clear the selection.
        assert_eq!(app.networks[0].pending_replies.last().map(|(_, _, p)| p.as_str()), Some("p1"));
        assert_eq!(app.networks[0].buffers[bi].selection, None);
    }

    #[test]
    fn nick_change_only_shown_where_nick_spoke_recently() {
        let mut app = test_app();
        // Two channels; the renamer spoke only in #spoke.
        let spoke = app.networks[0].ensure_buffer("#spoke", BufferKind::Channel);
        let quiet = app.networks[0].ensure_buffer("#quiet", BufferKind::Channel);
        for ch in [spoke, quiet] {
            let name = app.networks[0].buffers[ch].name.to_lowercase();
            app.networks[0].members.insert(
                name,
                vec![crate::irc::MemberEntry { nick: "alice".into(), prefixes: String::new(), userhost: None, is_bot: false, is_away: false }],
            );
        }
        app.apply_event(0, Event::Privmsg {
            target: "#spoke".into(),
            nick: "alice".into(),
            body: "hi".into(),
            meta: meta(),
        });

        let before_spoke = app.networks[0].buffers[spoke].lines.len();
        let before_quiet = app.networks[0].buffers[quiet].lines.len();
        app.apply_event(0, Event::NickChanged {
            old: "alice".into(),
            new: "alice_".into(),
            meta: meta(),
        });

        // The rename appears where she spoke, but not in the channel she lurked.
        assert_eq!(app.networks[0].buffers[spoke].lines.len(), before_spoke + 1);
        assert_eq!(app.networks[0].buffers[quiet].lines.len(), before_quiet);
        assert!(app.networks[0].buffers[spoke].lines.last().unwrap().text.contains("now known as"));
        // The member list is renamed in both, regardless of visibility.
        assert!(app.networks[0].members["#quiet"].iter().any(|m| m.nick == "alice_"));
    }

    #[test]
    fn own_nick_change_shown_everywhere() {
        let mut app = test_app();
        let quiet = app.networks[0].ensure_buffer("#quiet", BufferKind::Channel);
        let before = app.networks[0].buffers[quiet].lines.len();
        // We never "spoke" in #quiet, but our own rename should still appear.
        app.apply_event(0, Event::NickChanged {
            old: "me".into(),
            new: "me2".into(),
            meta: meta(),
        });
        assert_eq!(app.networks[0].nick, "me2");
        assert_eq!(app.networks[0].buffers[quiet].lines.len(), before + 1);
    }

    /// Build an app whose single network's outgoing channel we can read.
    fn app_with_outbox() -> (App, mpsc::Receiver<Outgoing>) {
        let (img_tx, _r) = mpsc::channel(1);
        let images = Images::new(Picker::from_fontsize((8, 16)), img_tx);
        let mut app = App::new(AppConfig::default(), images);
        let cfg = NetworkConfig {
            name: "t".into(), nickname: "me".into(), username: None, realname: None,
            server: "s".into(), port: 6697, use_tls: true, nick_password: None,
            sasl_username: None, sasl_password: None, client_cert_path: None,
            client_cert_pass: None, channels: vec![], buddies: vec![], autoconnect: true,
        };
        let (out, out_rx) = mpsc::channel(8);
        std::mem::forget(_r);
        app.networks.push(Network::new(0, cfg, out));
        (app, out_rx)
    }

    #[test]
    fn typing_sends_active_then_done() {
        let (mut app, mut out_rx) = app_with_outbox();
        app.networks[0].caps = vec!["message-tags".into()];
        let bi = app.networks[0].ensure_buffer("#c", BufferKind::Channel);
        app.active = ActiveBuffer { net: 0, buf: bi };

        app.notify_typing();
        match out_rx.try_recv() {
            Ok(Outgoing::Typing { target, state }) => {
                assert_eq!(target, "#c");
                assert_eq!(state.as_str(), "active");
            }
            _ => panic!("expected active typing"),
        }
        // Throttled: an immediate second notify stays quiet (spec: ≤ once/3s).
        app.notify_typing();
        assert!(out_rx.try_recv().is_err(), "second notify within 3s is throttled");

        app.stop_typing();
        match out_rx.try_recv() {
            Ok(Outgoing::Typing { state, .. }) => assert_eq!(state.as_str(), "done"),
            _ => panic!("expected done typing"),
        }
        // Already stopped: nothing more on the wire.
        app.stop_typing();
        assert!(out_rx.try_recv().is_err());
    }

    #[test]
    fn no_typing_without_message_tags() {
        let (mut app, mut out_rx) = app_with_outbox();
        // No message-tags cap negotiated.
        let bi = app.networks[0].ensure_buffer("#c", BufferKind::Channel);
        app.active = ActiveBuffer { net: 0, buf: bi };
        app.notify_typing();
        assert!(out_rx.try_recv().is_err(), "no typing tag without message-tags");
    }

    #[test]
    fn no_typing_in_status_buffer() {
        let (mut app, mut out_rx) = app_with_outbox();
        app.networks[0].caps = vec!["message-tags".into()];
        // Active buffer is the status buffer (index 0).
        app.active = ActiveBuffer { net: 0, buf: 0 };
        app.notify_typing();
        assert!(out_rx.try_recv().is_err(), "never announce typing in the status buffer");
    }

    #[test]
    fn own_typing_echo_is_ignored() {
        let (mut app, _out) = app_with_outbox();
        app.networks[0].nick = "me".into();
        let bi = app.networks[0].ensure_buffer("#c", BufferKind::Channel);
        // A peer typing shows up.
        app.apply_event(0, Event::TypingChanged {
            target: "#c".into(),
            nick: "alice".into(),
            state: TypingState::Active,
        });
        let names: Vec<_> = app.networks[0].buffers[bi]
            .typing
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        assert_eq!(names, vec!["alice".to_string()]);
        // Our own typing, echoed back by the server, does not.
        app.apply_event(0, Event::TypingChanged {
            target: "#c".into(),
            nick: "me".into(),
            state: TypingState::Active,
        });
        let names: Vec<_> = app.networks[0].buffers[bi]
            .typing
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        assert_eq!(names, vec!["alice".to_string()]);
    }

    #[test]
    fn stale_typing_indicator_expires() {
        let (mut app, _out) = app_with_outbox();
        let bi = app.networks[0].ensure_buffer("#c", BufferKind::Channel);
        // A fresh active indicator survives a sweep.
        app.apply_event(0, Event::TypingChanged {
            target: "#c".into(),
            nick: "alice".into(),
            state: TypingState::Active,
        });
        assert!(!app.expire_typing());
        assert_eq!(app.networks[0].buffers[bi].typing.len(), 1);
        // Backdate it past the 6s window: the next sweep reaps it and reports
        // the change so the caller knows to redraw.
        let old = std::time::Instant::now() - std::time::Duration::from_secs(7);
        app.networks[0].buffers[bi].typing[0].1 = old;
        assert!(app.expire_typing());
        assert!(app.networks[0].buffers[bi].typing.is_empty());
    }

    #[test]
    fn join_command_switches_active_buffer() {
        let mut app = test_app();
        // Active starts on the status buffer (index 0).
        assert_eq!(app.active.buf, 0);
        app.run_command("join #rust");
        // The active buffer should now be the new channel (index 1).
        assert_eq!(app.active.net, 0);
        assert_eq!(app.networks[0].buffers[app.active.buf].name, "#rust");
    }

    #[test]
    fn clear_wipes_active_buffer_scrollback() {
        let (mut app, _out) = app_with_outbox();
        let bi = app.networks[0].ensure_buffer("#c", BufferKind::Channel);
        app.active = ActiveBuffer { net: 0, buf: bi };
        for _ in 0..5 {
            app.networks[0].buffers[bi].push(Line::system("noise"));
        }
        app.networks[0].buffers[bi].scroll = 3;
        app.run_command("clear");
        assert!(app.networks[0].buffers[bi].lines.is_empty());
        assert_eq!(app.networks[0].buffers[bi].scroll, 0);
    }

    #[test]
    fn notice_sends_notice_on_wire_and_echoes_locally() {
        let (mut app, mut out_rx) = app_with_outbox();
        app.run_command("notice #c hello world");
        // Multi-word text must ride as a single trailing parameter.
        match out_rx.try_recv() {
            Ok(Outgoing::Raw { cmd, args }) => {
                assert_eq!(cmd, "NOTICE");
                assert_eq!(args, vec!["#c".to_string(), "hello world".to_string()]);
            }
            _ => panic!("expected NOTICE raw"),
        }
        // Without echo-message the notice is echoed into the target buffer.
        let bi = app.networks[0].find_buffer("#c").expect("notice opened the buffer");
        let last = app.networks[0].buffers[bi].lines.last().expect("echoed line");
        assert!(matches!(last.kind, LineKind::Notice));
        assert_eq!(last.text, "hello world");
    }

    #[test]
    fn ctcp_uppercases_verb_but_not_argument() {
        let (mut app, mut out_rx) = app_with_outbox();
        app.run_command("ctcp bob ping 12345");
        match out_rx.try_recv() {
            Ok(Outgoing::Ctcp { target, query }) => {
                assert_eq!(target, "bob");
                assert_eq!(query, "PING 12345");
            }
            _ => panic!("expected CTCP"),
        }
    }

    #[test]
    fn cycle_parts_then_rejoins_active_channel() {
        let (mut app, mut out_rx) = app_with_outbox();
        let bi = app.networks[0].ensure_buffer("#c", BufferKind::Channel);
        app.active = ActiveBuffer { net: 0, buf: bi };
        app.run_command("cycle");
        assert!(matches!(out_rx.try_recv(), Ok(Outgoing::Part { channel, .. }) if channel == "#c"));
        assert!(matches!(out_rx.try_recv(), Ok(Outgoing::Join(channel)) if channel == "#c"));
        // The buffer stays open across the cycle.
        assert!(app.networks[0].find_buffer("#c").is_some());
    }

    #[test]
    fn identify_prefixes_nickserv_verb() {
        let (mut app, mut out_rx) = app_with_outbox();
        app.run_command("identify hunter2");
        match out_rx.try_recv() {
            Ok(Outgoing::Privmsg { target, text }) => {
                assert_eq!(target, "NickServ");
                assert_eq!(text, "IDENTIFY hunter2");
            }
            _ => panic!("expected PRIVMSG to NickServ"),
        }
    }

    #[test]
    fn back_clears_away() {
        let (mut app, mut out_rx) = app_with_outbox();
        app.run_command("back");
        assert!(matches!(out_rx.try_recv(), Ok(Outgoing::Away(None))));
    }

    #[test]
    fn live_join_whos_the_newcomer_for_bot_flag() {
        let (mut app, mut out_rx) = app_with_outbox();
        app.networks[0].nick = "me".into();
        app.networks[0].isupport.bot_mode = Some('B');
        app.networks[0].ensure_buffer("#c", BufferKind::Channel);
        app.apply_event(0, Event::UserJoined {
            channel: "#c".into(), nick: "botnick".into(),
            userhost: None, account: None, realname: None, meta: meta(),
        });
        let mut saw_who = false;
        while let Ok(msg) = out_rx.try_recv() {
            if let Outgoing::Raw { cmd, args } = msg {
                if cmd == "WHO" && args == vec!["botnick".to_string()] {
                    saw_who = true;
                }
            }
        }
        assert!(saw_who, "a live join should WHO the newcomer for their bot flag");
    }

    #[test]
    fn no_who_when_server_lacks_bot_mode() {
        let (mut app, mut out_rx) = app_with_outbox();
        app.networks[0].nick = "me".into();
        // bot_mode defaults to None -> WHO is pointless and must be skipped.
        app.networks[0].ensure_buffer("#c", BufferKind::Channel);
        app.apply_event(0, Event::UserJoined {
            channel: "#c".into(), nick: "someone".into(),
            userhost: None, account: None, realname: None, meta: meta(),
        });
        while let Ok(msg) = out_rx.try_recv() {
            assert!(
                !matches!(msg, Outgoing::Raw { ref cmd, .. } if cmd == "WHO"),
                "no WHO without bot-mode support"
            );
        }
    }

    // A WHO reply's `G` flag seeds a member's away status just like `B` seeds bot.
    #[test]
    fn who_reply_seeds_away_flag() {
        let mut app = test_app();
        joins(&mut app, "#c", "me");
        app.apply_event(0, names("#c", &["me", "alice"]));
        app.apply_event(0, Event::WhoReply { channel: "#c".into(), nick: "alice".into(), is_bot: false, is_away: true });
        let roster = &app.networks[0].members["#c"];
        assert!(roster.iter().find(|m| m.nick == "alice").unwrap().is_away);
    }

    // away-notify AWAY messages flip a member's presence live: a message means gone,
    // an empty AWAY means back. The flag updates in every roster the nick is in.
    #[test]
    fn away_notify_toggles_member_presence() {
        let mut app = test_app();
        joins(&mut app, "#c", "me");
        joins(&mut app, "#d", "me");
        app.apply_event(0, names("#c", &["me", "alice"]));
        app.apply_event(0, names("#d", &["me", "alice"]));

        app.apply_event(0, Event::AwayChanged { nick: "alice".into(), message: Some("brb".into()), meta: meta() });
        assert!(app.networks[0].members["#c"].iter().find(|m| m.nick == "alice").unwrap().is_away);
        assert!(app.networks[0].members["#d"].iter().find(|m| m.nick == "alice").unwrap().is_away);

        app.apply_event(0, Event::AwayChanged { nick: "alice".into(), message: None, meta: meta() });
        assert!(!app.networks[0].members["#c"].iter().find(|m| m.nick == "alice").unwrap().is_away);
        assert!(!app.networks[0].members["#d"].iter().find(|m| m.nick == "alice").unwrap().is_away);
    }

    // WHO is worth sending for its away flag even on a server without bot-mode, as
    // long as away-notify is acked — the reply seeds initial presence.
    #[test]
    fn who_fires_with_away_notify_even_without_bot_mode() {
        let (mut app, mut out_rx) = app_with_outbox();
        app.networks[0].nick = "me".into();
        // No bot_mode, but away-notify is negotiated.
        app.networks[0].caps = vec!["away-notify".into()];
        app.networks[0].ensure_buffer("#c", BufferKind::Channel);
        app.apply_event(0, Event::UserJoined {
            channel: "#c".into(), nick: "someone".into(),
            userhost: None, account: None, realname: None, meta: meta(),
        });
        let mut saw_who = false;
        while let Ok(msg) = out_rx.try_recv() {
            if let Outgoing::Raw { cmd, args } = msg
                && cmd == "WHO" && args == vec!["someone".to_string()]
            {
                saw_who = true;
            }
        }
        assert!(saw_who, "away-notify alone should justify WHOing a newcomer");
    }

    // A reaction is attached live and, when its reaction message is redacted
    // ("unreact"), removed live — no part/rejoin needed.
    #[test]
    fn unreact_removes_the_reaction_live() {
        let mut app = test_app();
        joins(&mut app, "#c", "me");
        app.apply_event(0, Event::Reaction {
            target: "#c".into(), target_msgid: "m1".into(),
            nick: "alice".into(), emoji: "♥".into(), msgid: Some("react1".into()),
        });
        let bi = app.networks[0].find_buffer("#c").unwrap();
        assert_eq!(app.networks[0].buffers[bi].reactions["m1"][0].1, vec!["alice".to_string()]);

        // The unreact: a REDACT of the reaction message's own id.
        app.apply_event(0, Event::Redacted {
            target: "#c".into(), msgid: "react1".into(), by_nick: "alice".into(), reason: None,
        });
        assert!(
            !app.networks[0].buffers[bi].reactions.contains_key("m1"),
            "unreact should drop the reaction entry entirely"
        );
    }

    // One of two reactors unreacting leaves the other's reaction intact.
    #[test]
    fn unreact_prunes_only_the_redacted_reactor() {
        let mut app = test_app();
        joins(&mut app, "#c", "me");
        app.apply_event(0, Event::Reaction {
            target: "#c".into(), target_msgid: "m1".into(),
            nick: "alice".into(), emoji: "♥".into(), msgid: Some("ra".into()),
        });
        app.apply_event(0, Event::Reaction {
            target: "#c".into(), target_msgid: "m1".into(),
            nick: "bob".into(), emoji: "♥".into(), msgid: Some("rb".into()),
        });
        app.apply_event(0, Event::Redacted {
            target: "#c".into(), msgid: "ra".into(), by_nick: "alice".into(), reason: None,
        });
        let bi = app.networks[0].find_buffer("#c").unwrap();
        let hearts = &app.networks[0].buffers[bi].reactions["m1"][0].1;
        assert_eq!(hearts, &vec!["bob".to_string()], "only alice's heart should be pruned");
    }
}
