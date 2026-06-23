//! Application state: the set of networks, their buffers, and UI focus.

use crate::config::{AppConfig, NetworkConfig};
use crate::images::Images;
use crate::irc::{ISupport, MemberEntry, OutgoingTx};
use crate::theme::Theme;

pub type NetId = usize;

/// Which buffer the user is currently looking at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActiveBuffer {
    pub net: usize,
    pub buf: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Connected,
    Reconnecting,
    Disconnected,
    Error,
}

impl ConnState {
    pub fn glyph(self) -> char {
        match self {
            ConnState::Connected => '●',
            ConnState::Connecting | ConnState::Reconnecting => '◐',
            ConnState::Disconnected => '○',
            ConnState::Error => '✕',
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferKind {
    Status,
    Channel,
    Query,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Message,
    Action,
    Notice,
    System,
    Join,
    Part,
    Quit,
    Self_,
}

#[derive(Clone, Debug)]
pub struct Line {
    pub time: String,
    pub kind: LineKind,
    pub from: String,
    pub text: String,
    pub msgid: Option<String>,
    /// Set when `from`/text mentions us — used to tint the line.
    pub highlight: bool,
    /// `+draft/reply` parent msgid, when this message is a threaded reply.
    pub reply_to: Option<String>,
}

impl Line {
    pub fn system(text: impl Into<String>) -> Self {
        Line {
            time: String::new(),
            kind: LineKind::System,
            from: "*".into(),
            text: text.into(),
            msgid: None,
            highlight: false,
            reply_to: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Buffer {
    pub name: String,
    pub kind: BufferKind,
    pub lines: Vec<Line>,
    /// Rows scrolled up from the bottom. 0 = pinned to the latest line.
    pub scroll: usize,
    pub unread: u32,
    pub mentions: u32,
    pub topic: Option<String>,
    /// Nicks currently typing (draft/typing), most-recent last.
    pub typing: Vec<String>,
    /// Reactions keyed by the reacted message's msgid: emoji -> nicks.
    pub reactions: std::collections::HashMap<String, Vec<(String, Vec<String>)>>,
    /// The msgid of the currently selected message (for reply/react), if any.
    /// Stored by msgid (not index) so it survives buffer truncation.
    pub selection: Option<String>,
}

impl Buffer {
    pub fn new(name: impl Into<String>, kind: BufferKind) -> Self {
        Buffer {
            name: name.into(),
            kind,
            lines: Vec::new(),
            scroll: 0,
            unread: 0,
            mentions: 0,
            topic: None,
            typing: Vec::new(),
            reactions: std::collections::HashMap::new(),
            selection: None,
        }
    }

    /// Line indices of selectable messages (those carrying a server msgid),
    /// in chronological order.
    fn selectable(&self) -> Vec<usize> {
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.msgid.is_some())
            .map(|(i, _)| i)
            .collect()
    }

    /// The line index of the currently selected message, if it's still present.
    pub fn selected_index(&self) -> Option<usize> {
        let sel = self.selection.as_deref()?;
        self.lines.iter().position(|l| l.msgid.as_deref() == Some(sel))
    }

    /// The author of the selected message, if any.
    pub fn selected_from(&self) -> Option<&str> {
        self.selected_index().map(|i| self.lines[i].from.as_str())
    }

    /// Move the selection. `delta < 0` moves to older messages (Alt+Up),
    /// `delta > 0` to newer (Alt+Down). From no selection, Up grabs the most
    /// recent message; moving past the newest clears the selection.
    pub fn move_selection(&mut self, delta: i32) {
        let sel = self.selectable();
        if sel.is_empty() {
            self.selection = None;
            return;
        }
        let set = |b: &mut Buffer, idx: usize| {
            b.selection = b.lines[idx].msgid.clone();
        };
        match self.selected_index() {
            None => {
                if delta < 0 {
                    set(self, *sel.last().unwrap());
                }
            }
            Some(cur) => {
                let pos = sel.iter().position(|&i| i == cur).unwrap_or(0);
                let np = pos as i32 + delta;
                if np < 0 {
                    set(self, sel[0]);
                } else if np as usize >= sel.len() {
                    self.selection = None; // moved past the newest -> deselect
                } else {
                    set(self, sel[np as usize]);
                }
            }
        }
    }

    /// Record a reaction `emoji` from `nick` to the message `msgid` (deduped).
    pub fn add_reaction(&mut self, msgid: String, emoji: String, nick: String) {
        let entry = self.reactions.entry(msgid).or_default();
        if let Some((_, nicks)) = entry.iter_mut().find(|(e, _)| *e == emoji) {
            if !nicks.iter().any(|n| n.eq_ignore_ascii_case(&nick)) {
                nicks.push(nick);
            }
        } else {
            entry.push((emoji, vec![nick]));
        }
    }

    /// True if `nick` authored a message or action within the last `window`
    /// lines of this buffer. Used to decide whether a nick-change is worth
    /// surfacing here (people who never talk generate pure noise).
    pub fn spoke_recently(&self, nick: &str, window: usize) -> bool {
        self.lines.iter().rev().take(window).any(|l| {
            matches!(l.kind, LineKind::Message | LineKind::Action | LineKind::Self_)
                && l.from.eq_ignore_ascii_case(nick)
        })
    }

    pub fn push(&mut self, line: Line) {
        // Keep the view pinned to the bottom unless the user scrolled up.
        self.lines.push(line);
        if self.lines.len() > 5000 {
            let drop = self.lines.len() - 5000;
            self.lines.drain(0..drop);
        }
    }
}

pub struct Network {
    pub id: NetId,
    pub cfg: NetworkConfig,
    pub out: OutgoingTx,
    pub conn: ConnState,
    /// Our current nick on this network.
    pub nick: String,
    pub isupport: ISupport,
    pub caps: Vec<String>,
    /// buffers[0] is always the status buffer.
    pub buffers: Vec<Buffer>,
    /// Channel (lowercased) -> member list.
    pub members: std::collections::HashMap<String, Vec<MemberEntry>>,
    /// Buddies currently online (from MONITOR).
    pub online_buddies: std::collections::HashSet<String>,
    /// Replies we've sent that are awaiting their echo, so we can re-apply the
    /// `reply_to` locally if the server doesn't echo the `+draft/reply` tag
    /// back (some networks strip client-only tags). (target_lc, text, parent_msgid).
    pub pending_replies: Vec<(String, String, String)>,
}

impl Network {
    pub fn new(id: NetId, cfg: NetworkConfig, out: OutgoingTx) -> Self {
        let nick = cfg.nickname.clone();
        let status = Buffer::new(format!("({})", cfg.name), BufferKind::Status);
        Network {
            id,
            cfg,
            out,
            conn: ConnState::Connecting,
            nick,
            isupport: ISupport::default(),
            caps: Vec::new(),
            buffers: vec![status],
            members: std::collections::HashMap::new(),
            online_buddies: std::collections::HashSet::new(),
            pending_replies: Vec::new(),
        }
    }

    pub fn status_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[0]
    }

    /// Find a buffer index by case-insensitive name, or create it.
    pub fn ensure_buffer(&mut self, name: &str, kind: BufferKind) -> usize {
        if let Some(i) = self
            .buffers
            .iter()
            .position(|b| b.name.eq_ignore_ascii_case(name))
        {
            return i;
        }
        self.buffers.push(Buffer::new(name, kind));
        self.buffers.len() - 1
    }

    pub fn find_buffer(&self, name: &str) -> Option<usize> {
        self.buffers
            .iter()
            .position(|b| b.name.eq_ignore_ascii_case(name))
    }

    pub fn is_channel(&self, name: &str) -> bool {
        let chantypes = if self.isupport.chantypes.is_empty() {
            "#&"
        } else {
            &self.isupport.chantypes
        };
        name.chars().next().map(|c| chantypes.contains(c)).unwrap_or(false)
    }
}

pub struct App {
    pub config: AppConfig,
    pub networks: Vec<Network>,
    pub active: ActiveBuffer,
    pub input: String,
    /// Cursor position as a byte offset into `input`.
    pub cursor: usize,
    pub should_quit: bool,
    pub show_members: bool,
    pub show_sidebar: bool,
    pub inline_images: bool,
    pub link_previews: bool,
    /// Hide join/part/quit lines in channel buffers (toggle with `/joins`).
    pub hide_join_part: bool,
    /// File-upload backend config (`/upload`).
    pub upload_cfg: crate::config::UploadConfig,
    /// True while an upload is in flight (prevents overlapping uploads).
    pub uploading: bool,
    /// Channel an upload task reports its result back on. Set by `main` after
    /// construction; `None` in tests (uploads become no-ops).
    pub up_tx: Option<tokio::sync::mpsc::Sender<crate::upload::UploadMsg>>,
    /// When true, the composer's next submission is an emoji reaction to the
    /// selected (or last) message rather than a normal message.
    pub react_mode: bool,
    /// Last time we sent a `+typing=active` notification, to throttle to the
    /// spec's "at most every 3s". `None` means we're not currently typing.
    pub typing_throttle: Option<std::time::Instant>,
    /// Transient status-bar message.
    pub status_msg: Option<String>,
    /// Tab-completion state: (anchor byte offset, candidates, index).
    pub completion: Option<Completion>,
    /// Inline-image fetch cache + terminal-graphics picker.
    pub images: Images,
    /// Active color theme.
    pub theme: Theme,
    /// Dictionary words for ghost-text completion, keyed by lang code ("en").
    /// Loaded once at startup; empty when no `.dic` files are found.
    pub dict_words: crate::dict::Words,
    /// Hunspell spell checkers keyed by lang code. Only langs with a matching
    /// `.aff`+`.dic` pair get one.
    pub spellers: crate::spell::Spellers,
    /// Per-buffer language for spell-check/autocomplete, set with `/lang`.
    /// Keyed by `"<network>/<channel-lowercased>"` (see `App::lang_key`).
    pub channel_langs: std::collections::HashMap<String, String>,
    /// Words (besides your nick) that trigger a mention highlight. Seeded from
    /// `config.highlight_keywords`, edited live with `/highlight`.
    pub highlight_keywords: Vec<String>,
}

pub struct Completion {
    pub start: usize,
    pub candidates: Vec<String>,
    pub index: usize,
    pub suffix: String,
}

impl App {
    pub fn new(config: AppConfig, images: Images) -> Self {
        let inline_images = config.inline_images;
        let link_previews = config.link_previews;
        let hide_join_part = config.hide_join_part;
        let upload_cfg = config.upload.clone();
        let highlight_keywords = config.highlight_keywords.clone();
        let theme = Theme::by_name(config.theme.as_deref().unwrap_or("dark"));
        App {
            config,
            networks: Vec::new(),
            active: ActiveBuffer { net: 0, buf: 0 },
            input: String::new(),
            cursor: 0,
            should_quit: false,
            show_members: true,
            show_sidebar: true,
            inline_images,
            link_previews,
            hide_join_part,
            upload_cfg,
            uploading: false,
            up_tx: None,
            react_mode: false,
            typing_throttle: None,
            status_msg: None,
            completion: None,
            images,
            theme,
            dict_words: std::collections::HashMap::new(),
            spellers: std::collections::HashMap::new(),
            channel_langs: std::collections::HashMap::new(),
            highlight_keywords,
        }
    }

    pub fn net(&self, id: NetId) -> Option<usize> {
        self.networks.iter().position(|n| n.id == id)
    }

    pub fn active_net(&self) -> Option<&Network> {
        self.networks.get(self.active.net)
    }

    pub fn active_net_mut(&mut self) -> Option<&mut Network> {
        self.networks.get_mut(self.active.net)
    }

    pub fn active_buffer(&self) -> Option<&Buffer> {
        self.active_net().and_then(|n| n.buffers.get(self.active.buf))
    }

    pub fn active_buffer_mut(&mut self) -> Option<&mut Buffer> {
        let b = self.active.buf;
        self.active_net_mut().and_then(|n| n.buffers.get_mut(b))
    }

    /// The display name of the active buffer (channel, nick, or status).
    pub fn active_target(&self) -> Option<String> {
        self.active_buffer().map(|b| b.name.clone())
    }

    /// Clamp `active` to a valid (net, buf) pair, e.g. after closing a buffer.
    pub fn clamp_active(&mut self) {
        if self.networks.is_empty() {
            self.active = ActiveBuffer { net: 0, buf: 0 };
            return;
        }
        if self.active.net >= self.networks.len() {
            self.active.net = self.networks.len() - 1;
        }
        let nbuf = self.networks[self.active.net].buffers.len();
        if self.active.buf >= nbuf {
            self.active.buf = nbuf.saturating_sub(1);
        }
    }

    /// Mark the active buffer read.
    pub fn mark_active_read(&mut self) {
        if let Some(b) = self.active_buffer_mut() {
            b.unread = 0;
            b.mentions = 0;
        }
    }

    /// Move to the next/previous buffer, flattened across all networks.
    pub fn cycle_buffer(&mut self, forward: bool) {
        let flat = self.flat_buffers();
        if flat.is_empty() {
            return;
        }
        let cur = flat
            .iter()
            .position(|&(n, b)| n == self.active.net && b == self.active.buf)
            .unwrap_or(0);
        let next = if forward {
            (cur + 1) % flat.len()
        } else {
            (cur + flat.len() - 1) % flat.len()
        };
        let (n, b) = flat[next];
        self.active = ActiveBuffer { net: n, buf: b };
        self.mark_active_read();
    }

    fn flat_buffers(&self) -> Vec<(usize, usize)> {
        let mut v = Vec::new();
        for (ni, net) in self.networks.iter().enumerate() {
            for bi in 0..net.buffers.len() {
                v.push((ni, bi));
            }
        }
        v
    }
}
