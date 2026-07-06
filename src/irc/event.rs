//! Wire-level event and command types exchanged between the IRC worker and
//! the UI. Ported from murmur's `irc_worker.rs` (logic unchanged); the only
//! difference is `Event::Ready` is gone — the `Outgoing` sender is returned
//! directly from `spawn_network`.

use std::collections::HashSet;

use tokio::sync::mpsc;

/// Subset of RPL_ISUPPORT (005) tokens that we act on.
#[derive(Clone, Default, Debug)]
pub struct ISupport {
    /// `MODES=<n>` — max mode changes per single MODE command.
    pub modes: Option<u8>,
    /// `CHANTYPES=<chars>` — characters that mark a channel name (`#&`).
    pub chantypes: String,
    /// `PREFIX=(modes)prefixes` — raw mapping (e.g. `(ohv)@%+`).
    pub prefix: String,
    /// `CASEMAPPING=<name>`.
    pub casemapping: String,
    /// `NETWORK=<name>` — human-readable network name.
    pub network: Option<String>,
    /// `soju.im/FILEHOST` / `draft/FILEHOST` — HTTP upload endpoint URI.
    pub filehost: Option<String>,
    /// `MONITOR=<n>` — max MONITOR targets. `Some(u32::MAX)` if no explicit cap.
    pub monitor_limit: Option<u32>,
    /// `CLIENTTAGDENY` parsing — server strips these client-only (`+`) tags.
    pub client_tag_deny_all: bool,
    pub client_tag_deny: HashSet<String>,
    pub client_tag_allow: HashSet<String>,
    /// `BOT=<letter>` — the user mode a client sets to flag itself as a bot
    /// (IRCv3 bot-mode). `None` if the server doesn't advertise it.
    pub bot_mode: Option<char>,
}

impl ISupport {
    /// True when the server strips the given client-only tag from outgoing
    /// PRIVMSG/TAGMSG. Accepts `draft/react` or `+draft/react`.
    pub fn client_tag_denied(&self, tag: &str) -> bool {
        let t = tag.strip_prefix('+').unwrap_or(tag);
        if self.client_tag_allow.contains(t) {
            return false;
        }
        if self.client_tag_deny_all {
            return true;
        }
        self.client_tag_deny.contains(t)
    }
}

/// One entry from a NAMES reply (or a JOIN), enriched with `multi-prefix`
/// + `userhost-in-names` data when those caps are acked.
#[derive(Clone, Debug)]
pub struct MemberEntry {
    pub nick: String,
    /// All channel prefixes for this member, highest-priority first.
    pub prefixes: String,
    /// `ident@host` without the nick, if `userhost-in-names` is acked.
    /// Retained for future WHOIS-on-hover; not yet surfaced in the UI.
    #[allow(dead_code)]
    pub userhost: Option<String>,
    /// IRCv3 bot-mode flag (set from a WHO reply's `B` flag). Drives a badge in
    /// the member panel. NAMES can't carry it, so it stays false until WHO fills it.
    pub is_bot: bool,
}

/// Per-message metadata extracted from IRCv3 tags.
#[derive(Clone, Default, Debug)]
pub struct MsgMeta {
    /// HH:MM extracted from the `time` tag (shifted to local zone).
    pub server_time_hhmm: Option<String>,
    /// Full ISO8601 value of the `time` tag, kept verbatim as a CHATHISTORY anchor.
    pub server_time_iso: Option<String>,
    /// Unique server-issued message id from the `msgid` tag.
    pub msgid: Option<String>,
    /// Batch reference tag if this message belongs to an open batch.
    pub batch: Option<String>,
    /// Lower-case batch kind looked up from the open-batch table.
    pub batch_kind: Option<String>,
    /// IRCv3 `account` tag — the sender's services account, if any.
    pub account: Option<String>,
    /// `+draft/reply=<msgid>` — message this is threaded as a reply to.
    pub reply_to_msgid: Option<String>,
}

#[derive(Clone)]
#[allow(dead_code)] // full IRCv3 command surface; some variants are wired incrementally
pub enum Outgoing {
    Privmsg { target: String, text: String },
    PrivmsgReply { target: String, text: String, reply_to_msgid: String },
    Action { target: String, text: String },
    Ctcp { target: String, query: String },
    Join(String),
    Part { channel: String, reason: Option<String> },
    Nick(String),
    ChatHistoryLatest { target: String, limit: u32 },
    ChatHistoryBefore { target: String, before_ts: String, limit: u32 },
    ChatHistoryTargets { from_ts: String, to_ts: String, limit: u32 },
    Whois(String),
    Away(Option<String>),
    Topic { channel: String, topic: Option<String> },
    Raw { cmd: String, args: Vec<String> },
    Kick { channel: String, nick: String, reason: Option<String> },
    Invite { nick: String, channel: String },
    Mode { target: String, modes: String, args: Vec<String> },
    Typing { target: String, state: TypingState },
    MarkRead { target: String, timestamp: Option<String> },
    Redact { target: String, msgid: String, reason: Option<String> },
    React { target: String, msgid: String, emoji: String },
    SetName(String),
    Monitor(MonitorCmd),
    /// Cleanly close the connection (QUIT) and stop the worker.
    Quit(Option<String>),
}

#[derive(Clone, Debug)]
pub enum MonitorCmd {
    Add(Vec<String>),
    Del(Vec<String>),
    Clear,
}

#[derive(Clone, Copy, Debug)]
pub enum TypingState {
    Active,
    Paused,
    Done,
}

impl TypingState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            TypingState::Active => "active",
            TypingState::Paused => "paused",
            TypingState::Done => "done",
        }
    }
}

#[derive(Clone)]
#[allow(dead_code)]
pub enum Event {
    Connected,
    /// Caps the server actually ACKed during negotiation. Lowercase names.
    CapsAcked(Vec<String>),
    ConnectError(String),
    Disconnected,
    /// Recoverable disconnect; another attempt is scheduled in `in_secs`.
    Reconnecting { in_secs: u64 },
    Privmsg { target: String, nick: String, body: String, meta: MsgMeta },
    Action { target: String, nick: String, body: String, meta: MsgMeta },
    UserJoined {
        channel: String,
        nick: String,
        userhost: Option<String>,
        account: Option<String>,
        realname: Option<String>,
        meta: MsgMeta,
    },
    UserLeft { channel: String, nick: String, meta: MsgMeta },
    UserQuit { nick: String, reason: Option<String>, meta: MsgMeta },
    ChatHistoryBatchEnd { target: String },
    NickChanged { old: String, new: String, meta: MsgMeta },
    Names { channel: String, members: Vec<MemberEntry> },
    /// One RPL_WHOREPLY (352) row: annotates a known member with their bot flag.
    WhoReply { channel: String, nick: String, is_bot: bool },
    Topic { channel: String, topic: String },
    Notice { from: String, text: String, meta: MsgMeta },
    CtcpReply { from: String, query: String, args: String },
    AccountChanged { nick: String, account: Option<String>, meta: MsgMeta },
    AwayChanged { nick: String, message: Option<String>, meta: MsgMeta },
    HostChanged { nick: String, ident: String, host: String, meta: MsgMeta },
    ISupport(ISupport),
    TypingChanged { target: String, nick: String, state: TypingState },
    ReadMarker { target: String, timestamp: Option<String> },
    Redacted { target: String, msgid: String, by_nick: String, reason: Option<String> },
    Reaction { target: String, target_msgid: String, nick: String, emoji: String },
    Presence { nicks: Vec<String>, online: bool },
}

/// Sender half handed to the UI for issuing commands to a network worker.
pub type OutgoingTx = mpsc::Sender<Outgoing>;
