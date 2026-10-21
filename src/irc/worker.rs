//! Per-network IRC worker. Ported from murmur's `irc_worker.rs`: the protocol
//! logic (SASL, CAP negotiation, batch/multiline, ISUPPORT, the full
//! `translate` table) is unchanged. The only adaptation is the transport —
//! murmur drove this from an `iced::stream::channel`; here it's a plain
//! `tokio::spawn` wired with tokio mpsc channels, and the `Outgoing` sender is
//! returned directly from `spawn_network` instead of via an `Event::Ready`.

use std::collections::{HashMap, HashSet};

use futures::StreamExt;
use irc::client::prelude::*;
use irc::proto::CapSubCommand;
use irc::proto::message::Tag;
use tokio::sync::mpsc;

use super::event::*;
use crate::config::{AuthMode, NetworkConfig};

const WANT_EXTRA_CAPS: &[&str] = &[
    "message-tags",
    "server-time",
    "batch",
    "invite-notify",
    "draft/chathistory",
    // Tier-1 identity / presence:
    "account-tag",
    "extended-join",
    "account-notify",
    "away-notify",
    "chghost",
    "echo-message",
    "setname",
    // Tier-2 NAMES enrichment:
    "multi-prefix",
    "userhost-in-names",
    // No NAMES burst on JOIN; the roster is fetched when a channel is first
    // viewed (see App::request_active_members). Ratified name + soju's vendor one.
    "no-implicit-names",
    "soju.im/no-implicit-names",
    // Tier-2 protocol plumbing:
    "cap-notify",
    // AWAY/ACCOUNT/CHGHOST for MONITORed nicks we share no channel with.
    "extended-monitor",
    "draft/extended-monitor",
    "labeled-response",
    // FAIL/WARN/NOTE replies (shown as notices; FAIL CHATHISTORY ends loading).
    "standard-replies",
    // NB: never REQ `sts` — the spec forbids it, and since CAP REQ is atomic a
    // server that NAKs it would refuse every other cap in the request too. The
    // policy is read from the LS value instead.
    // Tier-3 drafts:
    "draft/multiline",
    "draft/typing",
    "draft/read-marker",
    "draft/message-redaction",
    "draft/event-playback",
    "draft/sasl-ir",
    // Per-target soju flags (blocked/muted/pinned) and user profiles.
    "draft/metadata-2",
];

/// Append a line to `irkt.log` next to `config.toml`, timestamped in local
/// time. Every connect/disconnect funnels through here so the exact reason and
/// cadence of connection drops are on disk to read back later.
pub fn diag_log(net: &str, msg: &str) {
    let Some(dir) = crate::config::config_path().and_then(|p| p.parent().map(|d| d.to_path_buf()))
    else {
        return;
    };
    let off = time::UtcOffset::from_whole_seconds(crate::local_offset_secs() as i32)
        .unwrap_or(time::UtcOffset::UTC);
    let t = time::OffsetDateTime::now_utc().to_offset(off);
    let ts = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year(),
        t.month() as u8,
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
    );
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("irkt.log"))
    {
        let _ = writeln!(f, "{ts} [{net}] {msg}");
    }
}

#[derive(Debug, Clone)]
struct BatchInfo {
    kind: String,
    params: Vec<String>,
    chunks: Vec<MultilineChunk>,
    /// The opening BATCH carried `draft/chathistory-end`: this page reaches
    /// the start of the target's history.
    history_end: bool,
    /// Messages the server sent inside this batch (chathistory only), before
    /// any client-side dedup — a full page means there may be more.
    count: u32,
    /// Latest `time` tag seen in this batch: the anchor for the next
    /// `CHATHISTORY AFTER` page.
    newest_ts: Option<String>,
}

#[derive(Debug, Clone)]
struct MultilineChunk {
    target: String,
    nick: String,
    body: String,
    is_action: bool,
    concat: bool,
    meta: MsgMeta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthPhase {
    AwaitingCapLs,
    AwaitingCapAck,
    AwaitingChallenge,
    AwaitingResult,
    Done,
}

#[derive(Debug, Default)]
struct CapState {
    available: HashSet<String>,
    values: HashMap<String, String>,
    ls_complete: bool,
    acked: HashSet<String>,
    /// Set once a NAKed REQ has been retried with `sasl` alone.
    nak_retried: bool,
    /// SCRAM-SHA-256 exchange in progress (PLAIN credentials, upgraded when
    /// the server lists the mechanism).
    scram: Option<super::scram::ScramClient>,
    /// A base64 SASL challenge split over 400-byte AUTHENTICATE lines.
    sasl_buf: String,
    /// SCRAM was refused (904) and we retried with PLAIN; don't loop.
    scram_fell_back: bool,
}

/// Spawn a worker for one network. Returns the sender the UI uses to issue
/// commands; the worker pushes [`Event`]s to `tx` for the lifetime of the
/// connection (including reconnects).
pub fn spawn_network(cfg: &NetworkConfig, tx: mpsc::UnboundedSender<Event>) -> OutgoingTx {
    let (otx, orx) = mpsc::channel::<Outgoing>(512);
    let cfg = cfg.clone();
    tokio::spawn(run(cfg, tx, orx));
    otx
}

// `out` is unbounded on purpose: the worker must NEVER block on the UI. If it
// did (bounded channel full during a burst), it would stop polling the socket,
// miss the server's PING, and get dropped for a ping timeout — which is exactly
// the "random disconnect + lost messages" failure mode. The UI drains in
// batches, so the queue stays short in practice.
async fn run(mut cfg: NetworkConfig, out: mpsc::UnboundedSender<Event>, mut orx: mpsc::Receiver<Outgoing>) {
    // Apply a persisted IRCv3 STS policy if there is one.
    let sts_applied = if let Some(policy) = crate::config::sts::get_active(&cfg.server) {
        let mut changed = false;
        if !cfg.use_tls {
            cfg.use_tls = true;
            changed = true;
        }
        if cfg.port != policy.port {
            cfg.port = policy.port;
            changed = true;
        }
        changed.then_some(policy.port)
    } else {
        None
    };

    if let Some(port) = sts_applied {
        let _ = out
            .send(Event::Notice {
                from: "*".into(),
                text: format!("STS policy active: forcing TLS on port {port} for {}", cfg.server),
                meta: MsgMeta::default(),
            });
    }

    let auth_mode = cfg.auth_mode();
    let use_sasl = matches!(auth_mode, AuthMode::SaslPlain | AuthMode::SaslExternal);

    // Reconnect loop. Each iteration is one connection attempt.
    let mut attempt: u32 = 0;
    // `/rawlog`: mirror wire traffic to the UI (survives reconnects).
    let mut raw_log = false;
    loop {
        diag_log(
            &cfg.name,
            &format!(
                "connecting to {}:{} (tls={}, attempt={})",
                cfg.server, cfg.port, cfg.use_tls, attempt
            ),
        );
        if use_sasl {
            let mech = match auth_mode {
                AuthMode::SaslExternal => "EXTERNAL",
                _ => "PLAIN",
            };
            let _ = out
                .send(Event::Notice {
                    from: "*".into(),
                    text: format!("authenticating with SASL {mech}…"),
                    meta: MsgMeta::default(),
                });
        }

        let irc_cfg = Config {
            nickname: Some(cfg.nickname.clone()),
            username: cfg.username.clone(),
            realname: cfg.realname.clone(),
            server: Some(cfg.server.clone()),
            port: Some(cfg.port),
            use_tls: Some(cfg.use_tls),
            channels: cfg.channels.clone(),
            nick_password: if use_sasl { None } else { cfg.nick_password.clone() },
            client_cert_path: cfg.client_cert_path.clone(),
            client_cert_pass: cfg.client_cert_pass.clone(),
            // CTCP VERSION / SOURCE auto-replies (handled by the crate): the
            // client name + version and the project's GitHub URL.
            version: Some(format!(
                "irkt {} — terminal IRC client — https://github.com/prietus/irkt",
                env!("CARGO_PKG_VERSION")
            )),
            source: Some("https://github.com/prietus/irkt".into()),
            // The crate sends its own PING every `ping_time`s and drops the
            // link if no PONG arrives within `ping_timeout`s — but it only
            // checks while we're polling the socket. The PING/PONG round-trip
            // is also our NAT keepalive: long-lived IRC links sit idle, and an
            // aggressive NAT/firewall/CGNAT on the path silently drops an idle
            // flow, then RSTs it (observed as "connection reset by peer" at the
            // ingress and "server closed connection" here). 45s keeps the NAT
            // mapping warm well under typical idle windows and detects a truly
            // dead link sooner. The `ping_timeout` grace stays wide so a brief
            // UI hiccup isn't misread as a dead connection (the real fix for
            // that is coalescing redraws in the UI loop).
            ping_time: Some(45),
            ping_timeout: Some(60),
            ..Config::default()
        };

        let outcome: AttemptOutcome = 'attempt: {
            let mut client = match Client::from_config(irc_cfg).await {
                Ok(c) => c,
                Err(e) => break 'attempt AttemptOutcome::Recoverable(e.to_string()),
            };

            let sender = client.sender();
            let mut stream = match client.stream() {
                Ok(s) => s,
                Err(e) => break 'attempt AttemptOutcome::Fatal(e.to_string()),
            };

            // Pause registration with CAP LS 302 until we've seen the full LS.
            if let Err(e) = sender.send(Command::CAP(None, CapSubCommand::LS, None, Some("302".to_string()))) {
                break 'attempt AttemptOutcome::Recoverable(e.to_string());
            }
            let mut auth_phase = AuthPhase::AwaitingCapLs;
            let mut cap_state = CapState::default();
            let mut batches: HashMap<String, BatchInfo> = HashMap::new();
            let mut isupport = ISupport::default();
            let mut registered = false;

            loop {
                tokio::select! {
                    incoming = stream.next() => match incoming {
                        Some(Ok(msg)) => {
                            if raw_log {
                                let _ = out.send(Event::Wire { outgoing: false, line: redact_wire(&msg.to_string()) });
                            }
                            if auth_phase == AuthPhase::Done {
                                if let Some(updated) = handle_cap_notify(&msg, &sender, &mut cap_state) {
                                    let _ = out.send(Event::CapsAcked(updated));
                                }
                                if matches!(&msg.command, Command::CAP(..)) {
                                    continue;
                                }
                            }
                            if auth_phase != AuthPhase::Done {
                                match handle_auth_msg(&msg, &sender, &mut auth_phase, auth_mode, &cfg, &mut cap_state) {
                                    AuthOutcome::Pending => {}
                                    AuthOutcome::NeedIdentify => {
                                        if let Err(e) = client.identify() {
                                            break 'attempt AttemptOutcome::Fatal(e.to_string());
                                        }
                                        auth_phase = AuthPhase::Done;
                                        let acked: Vec<String> = cap_state.acked.iter().cloned().collect();
                                        let _ = out.send(Event::CapsAcked(acked));
                                        diag_log(&cfg.name, "connected");
                                        let _ = out.send(Event::Connected);
                                        attempt = 0;
                                    }
                                    AuthOutcome::Done => {
                                        let acked: Vec<String> = cap_state.acked.iter().cloned().collect();
                                        let _ = out.send(Event::CapsAcked(acked));
                                        let mech = match auth_mode {
                                            AuthMode::SaslExternal => "EXTERNAL",
                                            AuthMode::SaslPlain if cap_state.scram.is_some() => "SCRAM-SHA-256",
                                            AuthMode::SaslPlain => "PLAIN",
                                            _ => "",
                                        };
                                        if !mech.is_empty() {
                                            let _ = out.send(Event::Notice {
                                                from: "*".into(),
                                                text: format!("SASL {mech} authentication successful"),
                                                meta: MsgMeta::default(),
                                            });
                                        }
                                        diag_log(&cfg.name, "connected");
                                        let _ = out.send(Event::Connected);
                                        attempt = 0;
                                    }
                                    AuthOutcome::Failed(reason) => {
                                        break 'attempt AttemptOutcome::Fatal(reason);
                                    }
                                }
                                if is_auth_wire(&msg) {
                                    continue;
                                }
                            }
                            // RPL_METADATASYNCLATER <me> <target> [<retry-after>]:
                            // ask again later (bounded so a bad value can't stall).
                            if let Command::Raw(ref cmd, ref args) = msg.command {
                                if cmd == "774" && args.len() >= 2 {
                                    let target = args[1].clone();
                                    let secs = args.get(2).and_then(|s| s.parse::<u64>().ok()).unwrap_or(5).min(60);
                                    let s = sender.clone();
                                    tokio::spawn(async move {
                                        tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
                                        let _ = s.send(Command::Raw("METADATA".into(), vec![target, "SYNC".into()]));
                                    });
                                }
                            }
                            // End of MOTD (or no MOTD) marks registration done:
                            // the point where MONITOR/METADATA may be sent.
                            // `Connected` fires earlier, right after CAP END.
                            if !registered
                                && matches!(msg.command, Command::Response(Response::RPL_ENDOFMOTD | Response::ERR_NOMOTD, _))
                            {
                                registered = true;
                                let _ = out.send(Event::Registered);
                            }
                            // Count every message the server puts in a chathistory
                            // batch (events too, under event-playback): the page
                            // limit applies to those, not to what survives dedup.
                            if let Some(id) = msg.tags.as_ref().and_then(|tags| {
                                tags.iter().find(|t| t.0 == "batch").and_then(|t| t.1.clone())
                            }) {
                                if let Some(info) = batches.get_mut(&id) {
                                    if info.kind == "chathistory" {
                                        info.count += 1;
                                        let time = msg.tags.as_ref().and_then(|tags| {
                                            tags.iter().find(|t| t.0 == "time").and_then(|t| t.1.clone())
                                        });
                                        if let Some(t) = time {
                                            if info.newest_ts.as_deref().is_none_or(|n| t.as_str() > n) {
                                                info.newest_ts = Some(t);
                                            }
                                        }
                                    }
                                }
                            }
                            // Intercept BATCH for netsplit/netjoin grouping + multiline.
                            if let Command::BATCH(ref tag_with_sign, ref sub, ref params) = msg.command {
                                if let Some(id) = tag_with_sign.strip_prefix('+') {
                                    let kind = sub.as_ref().map(|s| s.to_str().to_ascii_lowercase()).unwrap_or_default();
                                    let history_end = msg.tags.as_ref().is_some_and(|tags| {
                                        tags.iter().any(|t| t.0 == "draft/chathistory-end")
                                    });
                                    batches.insert(id.to_string(), BatchInfo {
                                        kind,
                                        params: params.clone().unwrap_or_default(),
                                        chunks: Vec::new(),
                                        history_end,
                                        count: 0,
                                        newest_ts: None,
                                    });
                                } else if let Some(id) = tag_with_sign.strip_prefix('-') {
                                    if let Some(info) = batches.remove(id) {
                                        if info.kind == "chathistory" {
                                            if let Some(target) = info.params.first() {
                                                let _ = out.send(Event::ChatHistoryBatchEnd {
                                                    target: target.clone(),
                                                    end_of_history: info.history_end,
                                                    count: info.count,
                                                    newest_ts: info.newest_ts.clone(),
                                                });
                                            }
                                        } else if let Some(ev) = finalize_multiline(&info) {
                                            if out.send(ev).is_err() { return; }
                                        } else if let Some(text) = batch_summary(&info) {
                                            let _ = out.send(Event::Notice { from: "*".into(), text, meta: MsgMeta::default() });
                                        }
                                    }
                                }
                                continue;
                            }
                            if accumulate_multiline_chunk(&msg, &mut batches) {
                                continue;
                            }
                            for ev in translate(msg, &batches, &mut isupport) {
                                if out.send(ev).is_err() { return; }
                            }
                        }
                        Some(Err(e)) => break 'attempt AttemptOutcome::Recoverable(e.to_string()),
                        None => break 'attempt AttemptOutcome::Recoverable("server closed connection".into()),
                    },
                    outgoing = orx.recv() => {
                        // Everything below sends through this, so `/rawlog`
                        // sees the user's traffic. (Registration and SASL
                        // don't: they carry credentials.)
                        let sender = WireSender { inner: &sender, log: raw_log.then_some(&out) };
                        match outgoing {
                            Some(Outgoing::RawLog(on)) => raw_log = on,
                            Some(Outgoing::Privmsg { target, text }) => { let _ = sender.send_privmsg(&target, &text); }
                            Some(Outgoing::PrivmsgReply { target, text, reply_to_msgid }) => {
                                let _ = sender.send(Message {
                                    tags: Some(vec![Tag("+draft/reply".into(), Some(reply_to_msgid))]),
                                    prefix: None,
                                    command: Command::PRIVMSG(target, text),
                                });
                            }
                            Some(Outgoing::Action { target, text }) => {
                                let _ = sender.send_privmsg(&target, &format!("\x01ACTION {text}\x01"));
                            }
                            Some(Outgoing::Ctcp { target, query }) => {
                                let _ = sender.send_privmsg(&target, &format!("\x01{query}\x01"));
                            }
                            Some(Outgoing::Join(channel)) => { let _ = sender.send_join(&channel); }
                            Some(Outgoing::Part { channel, reason }) => { let _ = sender.send(Command::PART(channel, reason)); }
                            Some(Outgoing::Nick(new_nick)) => { let _ = sender.send(Command::NICK(new_nick)); }
                            Some(Outgoing::ChatHistoryLatest { target, limit }) => {
                                let _ = sender.send(Command::Raw("CHATHISTORY".into(), vec!["LATEST".into(), target, "*".into(), limit.to_string()]));
                            }
                            Some(Outgoing::ChatHistoryBefore { target, before_ts, limit }) => {
                                let _ = sender.send(Command::Raw("CHATHISTORY".into(), vec!["BEFORE".into(), target, format!("timestamp={before_ts}"), limit.to_string()]));
                            }
                            Some(Outgoing::ChatHistoryAfter { target, after_ts, limit }) => {
                                let _ = sender.send(Command::Raw("CHATHISTORY".into(), vec!["AFTER".into(), target, format!("timestamp={after_ts}"), limit.to_string()]));
                            }
                            Some(Outgoing::ChatHistoryTargets { from_ts, to_ts, limit }) => {
                                let _ = sender.send(Command::Raw("CHATHISTORY".into(), vec!["TARGETS".into(), format!("timestamp={from_ts}"), format!("timestamp={to_ts}"), limit.to_string()]));
                            }
                            Some(Outgoing::Whois(target)) => { let _ = sender.send(Command::WHOIS(None, target)); }
                            Some(Outgoing::Away(msg)) => { let _ = sender.send(Command::AWAY(msg)); }
                            Some(Outgoing::Topic { channel, topic }) => { let _ = sender.send(Command::TOPIC(channel, topic)); }
                            Some(Outgoing::Raw { cmd, args }) => { let _ = sender.send(Command::Raw(cmd, args)); }
                            Some(Outgoing::Kick { channel, nick, reason }) => { let _ = sender.send(Command::KICK(channel, nick, reason)); }
                            Some(Outgoing::Invite { nick, channel }) => { let _ = sender.send(Command::INVITE(nick, channel)); }
                            Some(Outgoing::Mode { target, modes, args }) => {
                                let mut argv = Vec::with_capacity(2 + args.len());
                                argv.push(target);
                                argv.push(modes);
                                argv.extend(args);
                                let _ = sender.send(Command::Raw("MODE".into(), argv));
                            }
                            Some(Outgoing::Typing { target, state }) => {
                                let _ = sender.send(Message {
                                    tags: Some(vec![Tag("+typing".into(), Some(state.as_str().into()))]),
                                    prefix: None,
                                    command: Command::Raw("TAGMSG".into(), vec![target]),
                                });
                            }
                            Some(Outgoing::MarkRead { target, timestamp }) => {
                                let mut argv = vec![target];
                                if let Some(ts) = timestamp { argv.push(format!("timestamp={ts}")); }
                                let _ = sender.send(Command::Raw("MARKREAD".into(), argv));
                            }
                            Some(Outgoing::Redact { target, msgid, reason }) => {
                                let mut argv = vec![target, msgid];
                                if let Some(r) = reason { argv.push(r); }
                                let _ = sender.send(Command::Raw("REDACT".into(), argv));
                            }
                            Some(Outgoing::SetName(realname)) => {
                                let _ = sender.send(Command::Raw("SETNAME".into(), vec![realname]));
                            }
                            Some(Outgoing::Monitor(cmd)) => {
                                let (op, targets) = match cmd {
                                    MonitorCmd::Add(t) => ("+", t),
                                    MonitorCmd::Del(t) => ("-", t),
                                };
                                // One line could overflow 512 bytes with a long
                                // list: split the targets across several.
                                for list in monitor_chunks(&targets) {
                                    let _ = sender.send(Command::Raw("MONITOR".into(), vec![op.to_string(), list]));
                                }
                            }
                            Some(Outgoing::React { target, msgid, emoji }) => {
                                let _ = sender.send(Message {
                                    tags: Some(vec![
                                        Tag("+draft/reply".into(), Some(msgid)),
                                        Tag("+draft/react".into(), Some(emoji)),
                                    ]),
                                    prefix: None,
                                    command: Command::Raw("TAGMSG".into(), vec![target]),
                                });
                            }
                            Some(Outgoing::Quit(reason)) => {
                                diag_log(&cfg.name, &format!("QUIT requested (reason={reason:?})"));
                                let _ = sender.send(Command::QUIT(reason));
                                let _ = out.send(Event::Disconnected);
                                return;
                            }
                            None => {
                                diag_log(&cfg.name, "outgoing channel closed — worker ending");
                                return;
                            }
                        }
                    }
                }
            }
        };

        match outcome {
            AttemptOutcome::Fatal(e) => {
                diag_log(&cfg.name, &format!("FATAL: {e}"));
                let _ = out.send(Event::ConnectError(e));
                return;
            }
            AttemptOutcome::Recoverable(e) => {
                let secs = backoff_secs(attempt);
                diag_log(&cfg.name, &format!("RECOVERABLE disconnect: {e} (reconnect in {secs}s)"));
                attempt = attempt.saturating_add(1);
                let _ = out
                    .send(Event::Notice {
                        from: "*".into(),
                        text: format!("disconnected: {e} — reconnecting in {secs}s"),
                        meta: MsgMeta::default(),
                    });
                let _ = out.send(Event::Reconnecting { in_secs: secs });
                tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
            }
        }
    }
}

enum AuthOutcome {
    Pending,
    NeedIdentify,
    Done,
    Failed(String),
}

enum AttemptOutcome {
    Recoverable(String),
    Fatal(String),
}

fn backoff_secs(attempt: u32) -> u64 {
    match attempt {
        0 => 2,
        1 => 4,
        2 => 8,
        3 => 16,
        4 => 30,
        _ => 60,
    }
}

fn handle_cap_notify(msg: &Message, sender: &Sender, caps: &mut CapState) -> Option<Vec<String>> {
    let Command::CAP(_, sub, third, fourth) = &msg.command else {
        return None;
    };
    let listed = fourth.as_deref().or(third.as_deref()).unwrap_or("");
    match *sub {
        CapSubCommand::NEW => {
            let mut to_req: Vec<String> = Vec::new();
            for token in listed.split_whitespace() {
                let (name_raw, value) = match token.split_once('=') {
                    Some((n, v)) => (n, Some(v.to_string())),
                    None => (token, None),
                };
                let name = name_raw.to_ascii_lowercase();
                caps.available.insert(name.clone());
                if let Some(v) = value {
                    caps.values.insert(name.clone(), v);
                }
                if WANT_EXTRA_CAPS.contains(&name.as_str()) && !caps.acked.contains(&name) {
                    to_req.push(name);
                }
            }
            if !to_req.is_empty() {
                let _ = sender.send(Command::CAP(None, CapSubCommand::REQ, None, Some(to_req.join(" "))));
            }
            None
        }
        CapSubCommand::ACK => {
            let mut changed = false;
            for cap in listed.split_whitespace() {
                if caps.acked.insert(cap.to_ascii_lowercase()) {
                    changed = true;
                }
            }
            changed.then(|| caps.acked.iter().cloned().collect())
        }
        CapSubCommand::DEL => {
            let mut changed = false;
            for cap in listed.split_whitespace() {
                let lower = cap.to_ascii_lowercase();
                caps.available.remove(&lower);
                caps.values.remove(&lower);
                if caps.acked.remove(&lower) {
                    changed = true;
                }
            }
            changed.then(|| caps.acked.iter().cloned().collect())
        }
        _ => None,
    }
}

fn handle_auth_msg(
    msg: &Message,
    sender: &Sender,
    phase: &mut AuthPhase,
    mode: AuthMode,
    cfg: &NetworkConfig,
    caps: &mut CapState,
) -> AuthOutcome {
    let use_sasl = matches!(mode, AuthMode::SaslPlain | AuthMode::SaslExternal);
    match &msg.command {
        Command::CAP(_, sub, third, fourth) if *sub == CapSubCommand::LS => {
            if *phase != AuthPhase::AwaitingCapLs {
                return AuthOutcome::Pending;
            }
            let (more, listed) = match (third.as_deref(), fourth.as_deref()) {
                (Some("*"), Some(list)) => (true, list),
                (Some(list), None) => (false, list),
                (_, Some(list)) => (false, list),
                _ => (false, ""),
            };
            for token in listed.split_whitespace() {
                let (name_raw, value) = match token.split_once('=') {
                    Some((n, v)) => (n, Some(v.to_string())),
                    None => (token, None),
                };
                let name = name_raw.to_ascii_lowercase();
                caps.available.insert(name.clone());
                if let Some(v) = value {
                    caps.values.insert(name, v);
                }
            }
            if more {
                return AuthOutcome::Pending;
            }
            caps.ls_complete = true;

            if let Some(value) = caps.values.get("sts").cloned() {
                if let Some((port, duration)) = parse_sts_value(&value) {
                    if cfg.use_tls && duration > 0 {
                        let _ = crate::config::sts::upsert(&cfg.server, port, duration);
                    }
                }
            }

            let mut wanted: Vec<&str> = WANT_EXTRA_CAPS
                .iter()
                .copied()
                .filter(|c| caps.available.contains(*c))
                .collect();
            if use_sasl {
                if !caps.available.contains("sasl") {
                    return AuthOutcome::Failed("server does not support SASL".into());
                }
                wanted.push("sasl");
            }
            if wanted.is_empty() {
                return AuthOutcome::NeedIdentify;
            }
            if use_sasl {
                if let Err(e) = sender.send(Command::NICK(cfg.nickname.clone())) {
                    return AuthOutcome::Failed(format!("send NICK: {e}"));
                }
                let username = cfg.username.clone().unwrap_or_else(|| cfg.nickname.clone());
                let realname = cfg.realname.clone().unwrap_or_else(|| cfg.nickname.clone());
                if let Err(e) = sender.send(Command::USER(username, "0".into(), realname)) {
                    return AuthOutcome::Failed(format!("send USER: {e}"));
                }
            }
            let req_str = wanted.join(" ");
            if let Err(e) = sender.send(Command::CAP(None, CapSubCommand::REQ, None, Some(req_str))) {
                return AuthOutcome::Failed(format!("send CAP REQ: {e}"));
            }
            *phase = AuthPhase::AwaitingCapAck;
            AuthOutcome::Pending
        }
        Command::CAP(_, sub, third, fourth) if *sub == CapSubCommand::ACK => {
            if *phase != AuthPhase::AwaitingCapAck {
                return AuthOutcome::Pending;
            }
            let listed = fourth.as_deref().or(third.as_deref()).unwrap_or("");
            for cap in listed.split_whitespace() {
                caps.acked.insert(cap.to_ascii_lowercase());
            }
            if use_sasl {
                if !caps.acked.contains("sasl") {
                    return AuthOutcome::Failed("server ACKed CAP without sasl".into());
                }
                if wants_scram(mode, cfg, caps) {
                    let pass = cfg.sasl_password.as_deref().unwrap_or("");
                    caps.scram = Some(super::scram::ScramClient::new(cfg.sasl_user(), pass));
                    if let Err(e) = sender.send(Command::AUTHENTICATE("SCRAM-SHA-256".into())) {
                        return AuthOutcome::Failed(format!("send AUTHENTICATE: {e}"));
                    }
                    *phase = AuthPhase::AwaitingChallenge;
                    return AuthOutcome::Pending;
                }
                let mech = match mode {
                    AuthMode::SaslExternal => "EXTERNAL",
                    _ => "PLAIN",
                };
                let payload = match mode {
                    AuthMode::SaslExternal => "+".to_string(),
                    _ => {
                        let user = cfg.sasl_user();
                        let pass = cfg.sasl_password.as_deref().unwrap_or("");
                        let raw = build_plain_payload(user, pass);
                        if raw.is_empty() { "+".to_string() } else { b64_encode(&raw) }
                    }
                };
                let use_ir = caps.acked.contains("draft/sasl-ir") && payload.len() < 400;
                if use_ir {
                    if let Err(e) = sender.send(Command::Raw("AUTHENTICATE".into(), vec![mech.to_string(), payload])) {
                        return AuthOutcome::Failed(format!("send AUTHENTICATE: {e}"));
                    }
                    *phase = AuthPhase::AwaitingResult;
                } else {
                    if let Err(e) = sender.send(Command::AUTHENTICATE(mech.to_string())) {
                        return AuthOutcome::Failed(format!("send AUTHENTICATE: {e}"));
                    }
                    *phase = AuthPhase::AwaitingChallenge;
                }
                AuthOutcome::Pending
            } else {
                AuthOutcome::NeedIdentify
            }
        }
        Command::CAP(_, sub, _, _) if *sub == CapSubCommand::NAK => {
            if use_sasl {
                // CAP REQ is atomic: one cap the server dislikes refuses the
                // whole set. Retry once with just `sasl` so authentication
                // still works (losing the extras) instead of failing outright.
                if !caps.nak_retried && *phase == AuthPhase::AwaitingCapAck {
                    caps.nak_retried = true;
                    if let Err(e) = sender.send(Command::CAP(None, CapSubCommand::REQ, None, Some("sasl".into()))) {
                        return AuthOutcome::Failed(format!("send CAP REQ: {e}"));
                    }
                    return AuthOutcome::Pending;
                }
                AuthOutcome::Failed("server refused SASL capability".into())
            } else {
                AuthOutcome::NeedIdentify
            }
        }
        Command::AUTHENTICATE(data) if *phase == AuthPhase::AwaitingChallenge && caps.scram.is_some() => {
            scram_step(data, &mut |c| sender.send(c).is_ok(), phase, caps)
        }
        Command::AUTHENTICATE(data) if *phase == AuthPhase::AwaitingChallenge => {
            if data != "+" {
                let _ = sender.send(Command::AUTHENTICATE("*".to_string()));
                return AuthOutcome::Failed(format!("unexpected AUTHENTICATE challenge: {data}"));
            }
            let payload = match mode {
                AuthMode::SaslExternal => "+".to_string(),
                _ => {
                    let user = cfg.sasl_user();
                    let pass = cfg.sasl_password.as_deref().unwrap_or("");
                    let raw = build_plain_payload(user, pass);
                    if raw.is_empty() { "+".to_string() } else { b64_encode(&raw) }
                }
            };
            for chunk in chunked_400(&payload) {
                if let Err(e) = sender.send(Command::AUTHENTICATE(chunk)) {
                    return AuthOutcome::Failed(format!("send AUTHENTICATE payload: {e}"));
                }
            }
            *phase = AuthPhase::AwaitingResult;
            AuthOutcome::Pending
        }
        Command::Response(code, args) => match *code {
            // Success before we checked the server's SCRAM signature: the
            // server never proved it knows our credentials. Don't trust it.
            Response::RPL_SASLSUCCESS if caps.scram.is_some() && *phase != AuthPhase::AwaitingResult => {
                AuthOutcome::Failed("SASL SCRAM: server reported success without proving its identity".into())
            }
            // The server refused SCRAM (some auth backends can't verify it
            // even when the mechanism is listed): retry once with PLAIN.
            Response::ERR_SASLFAIL if caps.scram.is_some() && !caps.scram_fell_back => {
                caps.scram = None;
                caps.scram_fell_back = true;
                caps.sasl_buf.clear();
                if let Err(e) = sender.send(Command::AUTHENTICATE("PLAIN".into())) {
                    return AuthOutcome::Failed(format!("send AUTHENTICATE: {e}"));
                }
                *phase = AuthPhase::AwaitingChallenge;
                AuthOutcome::Pending
            }
            Response::RPL_SASLSUCCESS if *phase == AuthPhase::AwaitingResult => {
                if let Err(e) = sender.send(Command::CAP(None, CapSubCommand::END, None, None)) {
                    return AuthOutcome::Failed(format!("send CAP END: {e}"));
                }
                *phase = AuthPhase::Done;
                AuthOutcome::Done
            }
            Response::ERR_NICKLOCKED
            | Response::ERR_SASLFAIL
            | Response::ERR_SASLTOOLONG
            | Response::ERR_SASLABORT
            | Response::ERR_SASLALREADY => {
                let detail = args.last().cloned().unwrap_or_else(|| format!("SASL error {code:?}"));
                AuthOutcome::Failed(format!("SASL failed: {detail}"))
            }
            _ => AuthOutcome::Pending,
        },
        _ => AuthOutcome::Pending,
    }
}

/// `Sender` with the subset of its API the outgoing loop uses, mirroring each
/// message to `/rawlog` when `log` is set.
struct WireSender<'a> {
    inner: &'a Sender,
    log: Option<&'a mpsc::UnboundedSender<Event>>,
}

impl WireSender<'_> {
    fn send<M: Into<Message>>(&self, msg: M) -> irc::error::Result<()> {
        let msg: Message = msg.into();
        if let Some(out) = self.log {
            let _ = out.send(Event::Wire { outgoing: true, line: redact_wire(&msg.to_string()) });
        }
        self.inner.send(msg)
    }

    fn send_privmsg(&self, target: &str, text: &str) -> irc::error::Result<()> {
        self.send(Command::PRIVMSG(target.to_string(), text.to_string()))
    }

    fn send_join(&self, channel: &str) -> irc::error::Result<()> {
        self.send(Command::JOIN(channel.to_string(), None, None))
    }
}

/// A wire line for `/rawlog`, minus its line ending and with secrets masked:
/// PASS/OPER/AUTHENTICATE parameters and NickServ IDENTIFY/REGISTER.
fn redact_wire(line: &str) -> String {
    let line = line.trim_end_matches(['\r', '\n']);
    // Skip tags and prefix to find the command word.
    let mut rest = line;
    let mut head_len = 0;
    for _ in 0..2 {
        if rest.starts_with('@') || rest.starts_with(':') {
            let cut = rest.find(' ').map_or(rest.len(), |i| i + 1);
            head_len += cut;
            rest = &rest[cut..];
        }
    }
    let cmd = rest.split(' ').next().unwrap_or("").to_ascii_uppercase();
    let upper = rest.to_ascii_uppercase();
    let secret = matches!(cmd.as_str(), "PASS" | "OPER" | "AUTHENTICATE")
        || (cmd == "PRIVMSG"
            && (upper.contains(" :IDENTIFY") || upper.contains(" :REGISTER") || upper.contains(" :GHOST")));
    if secret {
        format!("{}{} <redacted>", &line[..head_len], rest.split(' ').take(if cmd == "PRIVMSG" { 2 } else { 1 }).collect::<Vec<_>>().join(" "))
    } else {
        line.to_string()
    }
}

/// Comma-joined MONITOR target lists, each short enough (~400 bytes) to keep
/// `MONITOR + <list>` well inside the 512-byte line limit.
fn monitor_chunks(targets: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in targets.iter().filter(|t| !t.is_empty()) {
        match out.last_mut() {
            Some(cur) if cur.len() + 1 + t.len() <= 400 => {
                cur.push(',');
                cur.push_str(t);
            }
            _ => out.push(t.clone()),
        }
    }
    out
}

/// Upgrade SASL PLAIN to SCRAM-SHA-256 when the server lists it (CAP LS 302
/// `sasl=` value) and we have a password: the password then never crosses
/// the wire, and the server must prove it knows our credentials.
fn wants_scram(mode: AuthMode, cfg: &NetworkConfig, caps: &CapState) -> bool {
    mode == AuthMode::SaslPlain
        && !caps.scram_fell_back
        && cfg.sasl_password.as_deref().is_some_and(|p| !p.is_empty())
        && caps
            .values
            .get("sasl")
            .is_some_and(|v| v.split(',').any(|m| m.eq_ignore_ascii_case("SCRAM-SHA-256")))
}

/// One SCRAM-SHA-256 round: answer the server's `AUTHENTICATE` challenge with
/// the next client message, or check its final signature.
/// `send` queues a message and reports whether it could.
fn scram_step(
    data: &str,
    send: &mut dyn FnMut(Command) -> bool,
    phase: &mut AuthPhase,
    caps: &mut CapState,
) -> AuthOutcome {
    use super::scram::Step;
    use base64::Engine;
    let mut abort = |why: &str| {
        send(Command::AUTHENTICATE("*".to_string()));
        AuthOutcome::Failed(format!("SASL SCRAM: {why}"))
    };
    // A challenge may span several lines: a full 400-byte chunk means more follows.
    if data != "+" {
        caps.sasl_buf.push_str(data);
        if data.len() == 400 {
            return AuthOutcome::Pending;
        }
    }
    let raw = std::mem::take(&mut caps.sasl_buf);
    let challenge = if raw.is_empty() {
        String::new()
    } else {
        match base64::engine::general_purpose::STANDARD.decode(&raw).ok().and_then(|b| String::from_utf8(b).ok()) {
            Some(c) => c,
            None => return abort("malformed challenge"),
        }
    };
    let Some(scram) = caps.scram.as_mut() else {
        return abort("no exchange in progress");
    };
    let reply = match scram.step {
        Step::Initial => scram.client_first(),
        Step::SentFirst => match scram.client_final(&challenge) {
            Some(m) => m,
            None => return abort("malformed challenge"),
        },
        Step::SentFinal => {
            // A wrong signature means the server doesn't know our
            // credentials: possibly an impostor. Never fall back to PLAIN.
            if !scram.verify(&challenge) {
                caps.scram_fell_back = true;
                return abort("server signature mismatch");
            }
            if !send(Command::AUTHENTICATE("+".into())) {
                return AuthOutcome::Failed("send AUTHENTICATE failed".into());
            }
            *phase = AuthPhase::AwaitingResult;
            return AuthOutcome::Pending;
        }
    };
    for chunk in chunked_400(&b64_encode(reply.as_bytes())) {
        if !send(Command::AUTHENTICATE(chunk)) {
            return AuthOutcome::Failed("send AUTHENTICATE payload failed".into());
        }
    }
    AuthOutcome::Pending
}

fn is_auth_wire(msg: &Message) -> bool {
    matches!(
        &msg.command,
        Command::CAP(..)
            | Command::AUTHENTICATE(_)
            | Command::Response(
                Response::RPL_LOGGEDIN
                    | Response::RPL_LOGGEDOUT
                    | Response::ERR_NICKLOCKED
                    | Response::RPL_SASLSUCCESS
                    | Response::ERR_SASLFAIL
                    | Response::ERR_SASLTOOLONG
                    | Response::ERR_SASLABORT
                    | Response::ERR_SASLALREADY
                    | Response::RPL_SASLMECHS,
                _,
            )
    )
}

fn parse_sts_value(s: &str) -> Option<(u16, u64)> {
    let mut port: Option<u16> = None;
    let mut duration: u64 = 0;
    for part in s.split(',') {
        let (k, v) = match part.split_once('=') {
            Some(kv) => kv,
            None => (part, ""),
        };
        match k.trim() {
            "port" => port = v.trim().parse().ok(),
            "duration" => duration = v.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    port.map(|p| (p, duration))
}

fn build_plain_payload(user: &str, pass: &str) -> Vec<u8> {
    if user.is_empty() && pass.is_empty() {
        return Vec::new();
    }
    let mut v = Vec::with_capacity(user.len() * 2 + pass.len() + 2);
    v.push(0);
    v.extend_from_slice(user.as_bytes());
    v.push(0);
    v.extend_from_slice(pass.as_bytes());
    v
}

fn chunked_400(payload: &str) -> Vec<String> {
    if payload.len() < 400 {
        return vec![payload.to_string()];
    }
    let bytes = payload.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let end = (i + 400).min(bytes.len());
        out.push(std::str::from_utf8(&bytes[i..end]).unwrap_or("").to_string());
        i = end;
    }
    if payload.len() % 400 == 0 {
        out.push("+".to_string());
    }
    out
}

fn b64_encode(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut chunks = input.chunks_exact(3);
    for c in &mut chunks {
        let n = ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | c[2] as u32;
        out.push(T[((n >> 18) & 0x3f) as usize] as char);
        out.push(T[((n >> 12) & 0x3f) as usize] as char);
        out.push(T[((n >> 6) & 0x3f) as usize] as char);
        out.push(T[(n & 0x3f) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        1 => {
            let n = (rem[0] as u32) << 16;
            out.push(T[((n >> 18) & 0x3f) as usize] as char);
            out.push(T[((n >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
            out.push(T[((n >> 18) & 0x3f) as usize] as char);
            out.push(T[((n >> 12) & 0x3f) as usize] as char);
            out.push(T[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

fn extract_meta(tags: &Option<Vec<Tag>>, batches: &HashMap<String, BatchInfo>) -> MsgMeta {
    let mut m = MsgMeta::default();
    let Some(list) = tags.as_ref() else { return m };
    for Tag(k, v) in list {
        match k.as_str() {
            "time" => {
                if let Some(val) = v.as_deref() {
                    m.server_time_hhmm = parse_iso_hhmm(val);
                    m.server_time_iso = Some(val.to_string());
                }
            }
            "msgid" => m.msgid = v.clone(),
            "draft/msgid" => {
                if m.msgid.is_none() {
                    m.msgid = v.clone();
                }
            }
            "account" => {
                m.account = v.clone().filter(|s| !s.is_empty() && s != "*");
            }
            "batch" => {
                m.batch = v.clone();
                if let Some(id) = v.as_deref() {
                    if let Some(info) = batches.get(id) {
                        m.batch_kind = Some(info.kind.clone());
                    }
                }
            }
            "+draft/reply" => {
                m.reply_to_msgid = v.clone().filter(|s| !s.is_empty());
            }
            _ => {}
        }
    }
    m
}

fn accumulate_multiline_chunk(msg: &Message, batches: &mut HashMap<String, BatchInfo>) -> bool {
    let Command::PRIVMSG(target, body) = &msg.command else {
        return false;
    };
    let Some(tags) = &msg.tags else { return false };
    let mut batch_id: Option<&str> = None;
    let mut concat = false;
    for Tag(k, v) in tags {
        match k.as_str() {
            "batch" => batch_id = v.as_deref(),
            "draft/multiline-concat" => concat = true,
            _ => {}
        }
    }
    let Some(id) = batch_id else { return false };
    if batches.get(id).map(|i| i.kind.as_str()) != Some("draft/multiline") {
        return false;
    }
    let chunk_meta = extract_meta(&msg.tags, batches);
    let nick = match &msg.prefix {
        Some(Prefix::Nickname(n, _, _)) => n.clone(),
        Some(Prefix::ServerName(s)) => s.clone(),
        None => "*".into(),
    };
    let (clean_body, is_action) = match unwrap_ctcp_action(body) {
        Some(action) => (action, true),
        None => (body.clone(), false),
    };
    let Some(info) = batches.get_mut(id) else { return false };
    info.chunks.push(MultilineChunk {
        target: target.clone(),
        nick,
        body: strip_irc_formatting(&clean_body),
        is_action,
        concat,
        meta: chunk_meta,
    });
    true
}

fn finalize_multiline(info: &BatchInfo) -> Option<Event> {
    if info.kind != "draft/multiline" || info.chunks.is_empty() {
        return None;
    }
    let mut body = String::new();
    for (i, chunk) in info.chunks.iter().enumerate() {
        if i > 0 && !chunk.concat {
            body.push('\n');
        }
        body.push_str(&chunk.body);
    }
    let first = &info.chunks[0];
    let is_action = info.chunks.iter().all(|c| c.is_action);
    let target = first.target.clone();
    let nick = first.nick.clone();
    let meta = first.meta.clone();
    if is_action {
        Some(Event::Action { target, nick, body, meta })
    } else {
        Some(Event::Privmsg { target, nick, body, meta })
    }
}

fn batch_summary(info: &BatchInfo) -> Option<String> {
    let s1 = info.params.first().map(String::as_str).unwrap_or("?");
    let s2 = info.params.get(1).map(String::as_str).unwrap_or("?");
    match info.kind.as_str() {
        "netsplit" => Some(format!("netsplit: {s1} ↮ {s2}")),
        "netjoin" => Some(format!("netjoin: {s1} ↔ {s2}")),
        _ => None,
    }
}

fn parse_iso_hhmm(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if bytes.len() >= 16 && bytes[10] == b'T' {
        let h: i64 = s[11..13].parse().ok()?;
        let m: i64 = s[14..16].parse().ok()?;
        let total = (h * 60 + m + crate::local_offset_secs() / 60).rem_euclid(1440);
        let h = total / 60;
        let m = total % 60;
        Some(format!("{h:02}:{m:02}"))
    } else {
        None
    }
}

fn translate(
    msg: Message,
    batches: &HashMap<String, BatchInfo>,
    isupport: &mut ISupport,
) -> Vec<Event> {
    let (nick, sender_userhost) = match &msg.prefix {
        Some(Prefix::Nickname(n, ident, host)) => {
            let uh = if !ident.is_empty() && !host.is_empty() {
                Some(format!("{ident}@{host}"))
            } else {
                None
            };
            (n.clone(), uh)
        }
        Some(Prefix::ServerName(s)) => (s.clone(), None),
        None => ("*".into(), None),
    };
    let meta = extract_meta(&msg.tags, batches);
    let msg_tags_raw: Vec<(String, Option<String>)> = msg
        .tags
        .as_ref()
        .map(|ts| ts.iter().map(|Tag(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    match msg.command {
        Command::PRIVMSG(target, body) => {
            if let Some(action) = unwrap_ctcp_action(&body) {
                vec![Event::Action { target, nick, body: strip_irc_formatting(&action), meta }]
            } else if is_ctcp_wrapped(&body) {
                vec![]
            } else {
                vec![Event::Privmsg { target, nick, body: strip_irc_formatting(&body), meta }]
            }
        }
        Command::JOIN(channel, account, realname) => {
            let account = account.filter(|s| !s.is_empty() && s != "*");
            let realname = realname.filter(|s| !s.is_empty());
            vec![Event::UserJoined { channel, nick, userhost: sender_userhost, account, realname, meta }]
        }
        Command::PART(channel, _) => vec![Event::UserLeft { channel, nick, meta }],
        Command::QUIT(reason) => {
            let reason = reason.filter(|s| !s.is_empty());
            vec![Event::UserQuit { nick, reason, meta }]
        }
        Command::NICK(new) => vec![Event::NickChanged { old: nick, new, meta }],
        Command::ACCOUNT(account) => {
            let account = if account == "*" || account.is_empty() { None } else { Some(account) };
            vec![Event::AccountChanged { nick, account, meta }]
        }
        Command::AWAY(message) => {
            let message = message.filter(|s| !s.is_empty());
            vec![Event::AwayChanged { nick, message, meta }]
        }
        Command::CHGHOST(ident, host) => vec![Event::HostChanged { nick, ident, host, meta }],
        Command::TOPIC(channel, Some(topic)) => {
            vec![Event::Topic { channel, topic: strip_irc_formatting(&topic) }]
        }
        Command::INVITE(invited, channel) => vec![Event::Invited {
            by: nick,
            target: invited,
            channel,
            meta,
        }],
        Command::KICK(channel, target, reason) => {
            let text = match reason {
                Some(r) if !r.is_empty() => format!("{nick} kicked {target} from {channel} ({r})"),
                _ => format!("{nick} kicked {target} from {channel}"),
            };
            vec![
                Event::UserLeft { channel, nick: target, meta: meta.clone() },
                Event::Notice { from: "*".into(), text, meta },
            ]
        }
        Command::ChannelMODE(channel, modes) => {
            let rendered = render_modes(&modes);
            vec![Event::Notice { from: "*".into(), text: format!("{nick} sets mode {channel} {rendered}"), meta }]
        }
        Command::UserMODE(target, modes) => {
            let rendered = render_modes(&modes);
            vec![Event::Notice { from: "*".into(), text: format!("{nick} sets mode {target} {rendered}"), meta }]
        }
        Command::NOTICE(_, text) => match unwrap_ctcp_reply(&text) {
            Some((query, args)) => vec![Event::CtcpReply { from: nick, query, args }],
            None => vec![Event::Notice { from: nick, text: strip_irc_formatting(&text), meta }],
        },
        Command::Response(code, args) => match code {
            // RPL_KEYVALUE <me> <target> <key> <vis> [<value>]: the answer to
            // our own METADATA SET, same shape as a live update.
            Response::RPL_KEYVALUE if args.len() >= 4 => vec![Event::Metadata {
                target: args[1].clone(),
                key: args[2].clone(),
                value: args.get(4).cloned(),
            }],
            Response::RPL_METADATAEND => vec![],
            Response::RPL_ISUPPORT => {
                let token_count = args.len().saturating_sub(1);
                let mut changed = false;
                for tok in args.iter().skip(1).take(token_count.saturating_sub(1)) {
                    if apply_isupport_token(isupport, tok) {
                        changed = true;
                    }
                }
                if changed { vec![Event::ISupport(isupport.clone())] } else { vec![] }
            }
            Response::RPL_NAMREPLY if args.len() >= 4 => {
                // Emit each 353 line's slice immediately; the UI appends them into
                // the channel roster. A big channel splits NAMES across many 353
                // lines, so deferring to the closing 366 would lose the whole
                // roster if that 366 ever failed to arrive or line up.
                let channel = args[2].clone();
                let members = args[3].split_whitespace().filter_map(parse_name_entry).collect();
                vec![Event::Names { channel, members }]
            }
            // The roster is built incrementally from the 353 lines above, so the
            // closing 366 is just a terminator we swallow to keep it off the status
            // buffer.
            Response::RPL_ENDOFNAMES => vec![],
            Response::RPL_TOPIC if args.len() >= 3 => {
                vec![Event::Topic { channel: args[1].clone(), topic: strip_irc_formatting(&args[2]) }]
            }
            Response::RPL_MONONLINE if args.len() >= 2 => {
                let nicks = parse_monitor_targets(&args[1]);
                if nicks.is_empty() { vec![] } else { vec![Event::Presence { nicks, online: true }] }
            }
            Response::RPL_MONOFFLINE if args.len() >= 2 => {
                let nicks = parse_monitor_targets(&args[1]);
                if nicks.is_empty() { vec![] } else { vec![Event::Presence { nicks, online: false }] }
            }
            Response::RPL_MONLIST | Response::RPL_ENDOFMONLIST => vec![],
            // WHO reply (352): `<me> <chan> <user> <host> <server> <nick> <flags> :<hop realname>`.
            // The flags field carries `B` for IRCv3 bot-mode users and `G`/`H` for
            // gone/here (away) status. We annotate the roster silently — no
            // status-buffer line — and swallow the 315 terminator.
            Response::RPL_WHOREPLY if args.len() >= 7 => {
                vec![Event::WhoReply {
                    channel: args[1].clone(),
                    nick: args[5].clone(),
                    is_bot: args[6].contains('B'),
                    is_away: args[6].contains('G'),
                }]
            }
            _ => format_numeric(code, &args)
                .or_else(|| render_raw_numeric(code as u16, &args))
                .map(|text| vec![Event::Notice { from: "*".into(), text, meta }])
                .unwrap_or_default(),
        },
        Command::METADATA(ref target, None, Some(ref params)) if !params.is_empty() => {
            // `METADATA <target> <key> <vis>` (3 args: the key was cleared).
            vec![Event::Metadata { target: target.clone(), key: params[0].clone(), value: None }]
        }
        Command::Raw(ref cmd, ref args) if cmd.eq_ignore_ascii_case("METADATA") && args.len() >= 3 => {
            vec![Event::Metadata { target: args[0].clone(), key: args[1].clone(), value: args.get(3).cloned() }]
        }
        // RPL_METADATASUBOK; 771 UNSUBOK, 772 SUBS and 774 SYNCLATER (retried
        // by the worker loop) are bookkeeping, not worth a status line.
        Command::Raw(ref cmd, ref args) if cmd == "770" => {
            vec![Event::MetadataSubOk(args.iter().skip(1).cloned().collect())]
        }
        Command::Raw(ref cmd, _) if matches!(cmd.as_str(), "771" | "772" | "774") => vec![],
        Command::Raw(ref cmd, ref args) => {
            if let Ok(n) = cmd.parse::<u16>() {
                if let Some(text) = format_extended_numeric(n, args) {
                    return vec![Event::Notice { from: "*".into(), text, meta }];
                }
                if let Some(text) = render_raw_numeric(n, args) {
                    return vec![Event::Notice { from: "*".into(), text, meta }];
                }
            }
            if let Some(text) = format_standard_reply(cmd, args) {
                let mut evs = vec![Event::Notice { from: "*".into(), text, meta }];
                // A refused CHATHISTORY request never opens a batch, so nothing
                // else would end the buffer's "loading" state.
                if cmd.eq_ignore_ascii_case("FAIL")
                    && args.first().is_some_and(|c| c.eq_ignore_ascii_case("CHATHISTORY"))
                {
                    evs.push(Event::ChatHistoryFailed);
                }
                return evs;
            }
            if cmd.eq_ignore_ascii_case("CHATHISTORY")
                && args.first().map(|s| s.eq_ignore_ascii_case("TARGETS")).unwrap_or(false)
                && args.len() >= 3
            {
                return vec![Event::Notice {
                    from: "*".into(),
                    text: format!("history target: {} @ {}", args[1], args[2]),
                    meta,
                }];
            }
            if cmd.eq_ignore_ascii_case("TAGMSG") && !args.is_empty() {
                if let Some(events) = parse_tagmsg_event(&nick, &args[0], &msg_tags_raw, meta.msgid.clone()) {
                    return events;
                }
                return vec![];
            }
            if cmd.eq_ignore_ascii_case("MARKREAD") && !args.is_empty() {
                let target = args[0].clone();
                let timestamp = args
                    .iter()
                    .skip(1)
                    .find_map(|s| s.strip_prefix("timestamp=").map(str::to_string));
                return vec![Event::ReadMarker { target, timestamp }];
            }
            if cmd.eq_ignore_ascii_case("SETNAME") && !args.is_empty() {
                let new = args.last().cloned().unwrap_or_default();
                return vec![Event::Notice { from: "*".into(), text: format!("{nick} updated realname → {new}"), meta }];
            }
            if cmd.eq_ignore_ascii_case("REDACT") && args.len() >= 2 {
                let target = args[0].clone();
                let msgid = args[1].clone();
                let reason = args.get(2).cloned();
                return vec![Event::Redacted { target, msgid, by_nick: nick.clone(), reason }];
            }
            vec![]
        }
        _ => vec![],
    }
}

fn render_modes<T>(modes: &[irc::proto::Mode<T>]) -> String
where
    T: irc::proto::mode::ModeType,
{
    modes.iter().map(|m| m.to_string()).collect::<Vec<_>>().join(" ")
}

fn format_numeric(code: Response, args: &[String]) -> Option<String> {
    let p = |i: usize| args.get(i).map(String::as_str).unwrap_or("");
    match code {
        Response::RPL_AWAY if args.len() >= 3 => Some(format!("{} is away: {}", p(1), p(2))),
        Response::RPL_UNAWAY => Some("you are no longer marked as away".into()),
        Response::RPL_NOWAWAY => Some("you have been marked as away".into()),
        Response::RPL_WHOISUSER if args.len() >= 6 => {
            Some(format!("whois {}: {}!{}@{} — {}", p(1), p(1), p(2), p(3), p(5)))
        }
        Response::RPL_WHOISSERVER if args.len() >= 4 => Some(format!("whois {}: server {} ({})", p(1), p(2), p(3))),
        Response::RPL_WHOISOPERATOR if args.len() >= 3 => Some(format!("whois {}: {}", p(1), p(2))),
        Response::RPL_WHOISIDLE if args.len() >= 4 => Some(format!("whois {}: idle {}s, signon {}", p(1), p(2), p(3))),
        Response::RPL_WHOISCHANNELS if args.len() >= 3 => Some(format!("whois {}: channels {}", p(1), p(2))),
        Response::RPL_ENDOFWHOIS if args.len() >= 2 => Some(format!("whois {}: end", p(1))),
        Response::RPL_WHOISCERTFP if args.len() >= 3 => Some(format!("whois {}: {}", p(1), p(2))),
        Response::RPL_WHOWASUSER if args.len() >= 6 => {
            Some(format!("whowas {}: {}!{}@{} — {}", p(1), p(1), p(2), p(3), p(5)))
        }
        Response::RPL_ENDOFWHOWAS if args.len() >= 2 => Some(format!("whowas {}: end", p(1))),
        Response::ERR_WASNOSUCHNICK if args.len() >= 3 => Some(format!("there was no such nick: {}", p(1))),
        Response::ERR_NOSUCHNICK if args.len() >= 3 => Some(format!("no such nick: {}", p(1))),
        Response::ERR_NOSUCHCHANNEL if args.len() >= 3 => Some(format!("no such channel: {}", p(1))),
        Response::ERR_CHANOPRIVSNEEDED if args.len() >= 3 => Some(format!("not channel operator: {}", p(1))),
        Response::RPL_INVITING if args.len() >= 3 => Some(format!("inviting {} to {}", p(1), p(2))),
        Response::RPL_CHANNELMODEIS if args.len() >= 3 => Some(format!("mode {}: {}", p(1), args[2..].join(" "))),
        Response::RPL_BANLIST if args.len() >= 3 => Some(format!("banlist {}: {}", p(1), args[2..].join(" "))),
        Response::RPL_ENDOFBANLIST if args.len() >= 2 => Some(format!("banlist {}: end", p(1))),
        Response::ERR_USERNOTINCHANNEL if args.len() >= 4 => Some(format!("{} is not on {}", p(1), p(2))),
        Response::ERR_NOTONCHANNEL if args.len() >= 3 => Some(format!("you are not on {}", p(1))),
        Response::ERR_USERONCHANNEL if args.len() >= 4 => Some(format!("{} is already on {}", p(1), p(2))),
        Response::ERR_NEEDMOREPARAMS if args.len() >= 3 => Some(format!("{} needs more parameters", p(1))),
        Response::ERR_UNKNOWNMODE if args.len() >= 3 => Some(format!("unknown mode char: {}", p(1))),
        Response::ERR_INVITEONLYCHAN if args.len() >= 3 => Some(format!("cannot join {}: invite-only", p(1))),
        Response::ERR_NOPRIVILEGES => Some("permission denied: server operator required".into()),
        Response::RPL_LOGGEDIN if args.len() >= 4 => {
            Some(format!("logged in as {} ({})", p(2), args.last().map(String::as_str).unwrap_or("")))
        }
        Response::RPL_LOGGEDOUT => Some("logged out of services".into()),
        Response::RPL_SASLSUCCESS => Some("SASL authentication successful".into()),
        Response::ERR_SASLFAIL => {
            Some(format!("SASL authentication failed: {}", args.last().map(String::as_str).unwrap_or("")))
        }
        _ => None,
    }
}

fn parse_tagmsg_event(
    nick: &str,
    target: &str,
    tags: &[(String, Option<String>)],
    own_msgid: Option<String>,
) -> Option<Vec<Event>> {
    let mut typing: Option<TypingState> = None;
    let mut reply_msgid: Option<String> = None;
    let mut react_emoji: Option<String> = None;
    for (k, v) in tags {
        match (k.as_str(), v.as_deref()) {
            ("+typing", Some("active")) => typing = Some(TypingState::Active),
            ("+typing", Some("paused")) => typing = Some(TypingState::Paused),
            ("+typing", Some("done")) => typing = Some(TypingState::Done),
            ("+draft/reply", Some(id)) if !id.is_empty() => reply_msgid = Some(id.to_string()),
            ("+draft/react", Some(e)) if !e.is_empty() => react_emoji = Some(e.to_string()),
            _ => {}
        }
    }
    if let (Some(msgid), Some(emoji)) = (reply_msgid, react_emoji) {
        return Some(vec![Event::Reaction {
            target: target.to_string(),
            target_msgid: msgid,
            nick: nick.to_string(),
            emoji,
            msgid: own_msgid,
        }]);
    }
    if let Some(state) = typing {
        return Some(vec![Event::TypingChanged { target: target.to_string(), nick: nick.to_string(), state }]);
    }
    None
}

fn parse_monitor_targets(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|t| t.split('!').next().unwrap_or(t).trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn apply_isupport_token(isupport: &mut ISupport, tok: &str) -> bool {
    let (key, value) = match tok.split_once('=') {
        Some((k, v)) => (k, Some(v)),
        None => (tok, None),
    };
    match key {
        "MODES" => {
            let new = value.and_then(|v| v.parse::<u8>().ok());
            if isupport.modes != new {
                isupport.modes = new;
                return true;
            }
        }
        "CHANTYPES" => {
            let new = value.unwrap_or("").to_string();
            if isupport.chantypes != new {
                isupport.chantypes = new;
                return true;
            }
        }
        "PREFIX" => {
            let new = value.unwrap_or("").to_string();
            if isupport.prefix != new {
                isupport.prefix = new;
                return true;
            }
        }
        "CASEMAPPING" => {
            let new = value.unwrap_or("").to_string();
            if isupport.casemapping != new {
                isupport.casemapping = new;
                return true;
            }
        }
        "NETWORK" => {
            let new = value.map(str::to_string);
            if isupport.network != new {
                isupport.network = new;
                return true;
            }
        }
        "soju.im/FILEHOST" | "draft/FILEHOST" => {
            let new = value.map(str::to_string);
            if isupport.filehost != new {
                isupport.filehost = new;
                return true;
            }
        }
        "BOT" => {
            let new = value.and_then(|v| v.chars().next());
            if isupport.bot_mode != new {
                isupport.bot_mode = new;
                return true;
            }
        }
        "MONITOR" => {
            let new = value.and_then(|v| v.parse::<u32>().ok()).or(Some(u32::MAX));
            if isupport.monitor_limit != new {
                isupport.monitor_limit = new;
                return true;
            }
        }
        "MSGREFTYPES" => {
            let new: Vec<String> = value.unwrap_or("").split(',').filter(|s| !s.is_empty()).map(str::to_string).collect();
            if isupport.msgreftypes != new {
                isupport.msgreftypes = new;
                return true;
            }
        }
        "CLIENTTAGDENY" => {
            let mut deny_all = false;
            let mut deny: HashSet<String> = HashSet::new();
            let mut allow: HashSet<String> = HashSet::new();
            for entry in value.unwrap_or("").split(',') {
                let entry = entry.trim();
                if entry.is_empty() {
                    continue;
                }
                if entry == "*" {
                    deny_all = true;
                } else if let Some(rest) = entry.strip_prefix('-') {
                    allow.insert(rest.trim_start_matches('+').to_string());
                } else {
                    deny.insert(entry.trim_start_matches('+').to_string());
                }
            }
            if isupport.client_tag_deny_all != deny_all
                || isupport.client_tag_deny != deny
                || isupport.client_tag_allow != allow
            {
                isupport.client_tag_deny_all = deny_all;
                isupport.client_tag_deny = deny;
                isupport.client_tag_allow = allow;
                return true;
            }
        }
        _ => {}
    }
    false
}

fn format_standard_reply(cmd: &str, args: &[String]) -> Option<String> {
    let kind = match cmd.to_ascii_uppercase().as_str() {
        "FAIL" => "fail",
        "WARN" => "warn",
        "NOTE" => "note",
        _ => return None,
    };
    if args.len() < 3 {
        return None;
    }
    let command = &args[0];
    let code = &args[1];
    let description = args.last().map(String::as_str).unwrap_or("");
    let ctx = &args[2..args.len().saturating_sub(1)];
    let mut head = format!("{kind} {command} {code}");
    if !ctx.is_empty() {
        head.push_str(" [");
        head.push_str(&ctx.join(" "));
        head.push(']');
    }
    Some(format!("{head}: {description}"))
}

fn format_extended_numeric(code: u16, args: &[String]) -> Option<String> {
    let p = |i: usize| args.get(i).map(String::as_str).unwrap_or("");
    match code {
        330 if args.len() >= 4 => Some(format!("whois {}: account {}", p(1), p(2))),
        338 if args.len() >= 3 => Some(format!("whois {}: {}", p(1), args[2..].join(" "))),
        671 if args.len() >= 3 => Some(format!("whois {}: {}", p(1), p(2))),
        378 if args.len() >= 3 => Some(format!("whois {}: {}", p(1), p(2))),
        379 if args.len() >= 3 => Some(format!("whois {}: {}", p(1), p(2))),
        _ => None,
    }
}

fn is_suppressed_numeric(code: u16) -> bool {
    matches!(
        code,
        1 | 2 | 3 | 4 | 5 | 251 | 252 | 253 | 254 | 255 | 265 | 266 | 315 | 372 | 375 | 376 | 422
    )
}

fn render_raw_numeric(code: u16, args: &[String]) -> Option<String> {
    if is_suppressed_numeric(code) {
        return None;
    }
    // RPL_WHOISBOT (335): `<me> <nick> :is a bot…`. Not a variant in irc-proto, so
    // it lands here — format it like the other whois lines instead of `[335] …`.
    if code == 335 && args.len() >= 3 {
        return Some(format!("whois {}: {}", args[1], args[2..].join(" ")));
    }
    let body = if args.len() > 1 { args[1..].join(" ") } else { args.join(" ") };
    Some(format!("[{code}] {body}"))
}

fn is_ctcp_wrapped(body: &str) -> bool {
    body.starts_with('\x01') && body.len() >= 2 && body.ends_with('\x01')
}

/// Strip mIRC formatting + control codes we don't render.
pub fn strip_irc_formatting(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x02' | '\x1d' | '\x1f' | '\x1e' | '\x11' | '\x16' | '\x0f' => {}
            '\x03' => {
                let fg = take_digits(&mut chars, 2);
                if fg > 0 {
                    let mut peek = chars.clone();
                    if peek.next() == Some(',') && peek.peek().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                        chars.next();
                        let _ = take_digits(&mut chars, 2);
                    }
                }
            }
            '\x04' => {
                let _ = take_hex(&mut chars, 6);
                let mut peek = chars.clone();
                if peek.next() == Some(',') && peek.peek().map(|c| c.is_ascii_hexdigit()).unwrap_or(false) {
                    chars.next();
                    let _ = take_hex(&mut chars, 6);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

fn take_digits(chars: &mut std::iter::Peekable<std::str::Chars>, max: usize) -> usize {
    let mut n = 0;
    while n < max {
        match chars.peek() {
            Some(c) if c.is_ascii_digit() => {
                chars.next();
                n += 1;
            }
            _ => break,
        }
    }
    n
}

fn take_hex(chars: &mut std::iter::Peekable<std::str::Chars>, max: usize) -> usize {
    let mut n = 0;
    while n < max {
        match chars.peek() {
            Some(c) if c.is_ascii_hexdigit() => {
                chars.next();
                n += 1;
            }
            _ => break,
        }
    }
    n
}

const NAME_PREFIX_CHARS: &[char] = &['~', '&', '@', '%', '+'];

fn parse_name_entry(token: &str) -> Option<MemberEntry> {
    if token.is_empty() {
        return None;
    }
    let prefix_len = token
        .chars()
        .take_while(|c| NAME_PREFIX_CHARS.contains(c))
        .map(char::len_utf8)
        .sum();
    let (prefixes, rest) = token.split_at(prefix_len);
    if rest.is_empty() {
        return None;
    }
    let (nick, userhost) = match rest.split_once('!') {
        Some((n, uh)) if !uh.is_empty() => (n.to_string(), Some(uh.to_string())),
        _ => (rest.to_string(), None),
    };
    Some(MemberEntry { nick, prefixes: prefixes.to_string(), userhost, is_bot: false, is_away: false })
}

fn unwrap_ctcp_action(body: &str) -> Option<String> {
    let inner = body.strip_prefix('\x01')?.strip_suffix('\x01')?;
    let rest = inner.strip_prefix("ACTION ")?;
    Some(rest.to_string())
}

fn unwrap_ctcp_reply(text: &str) -> Option<(String, String)> {
    let inner = text.strip_prefix('\x01')?.strip_suffix('\x01')?;
    let mut parts = inner.splitn(2, ' ');
    let q = parts.next()?.trim().to_string();
    if q.is_empty() {
        return None;
    }
    let a = parts.next().unwrap_or("").to_string();
    Some((q, a))
}

#[cfg(test)]
mod tests {
    #[test]
    fn soju_filehost_isupport_is_parsed() {
        let msg: Message = ":irc.priet.us 005 teraflops BOT=B CASEMAPPING=ascii soju.im/FILEHOST=https://irc.priet.us/uploads soju.im/NO_IMPLICIT_NAMES :are supported"
            .parse()
            .unwrap();
        let mut is = ISupport::default();
        let evs = translate(msg, &HashMap::new(), &mut is);
        assert_eq!(is.filehost.as_deref(), Some("https://irc.priet.us/uploads"), "events: {}", evs.len());
    }

    use super::*;

    #[test]
    fn rawlog_redacts_secrets() {
        assert_eq!(redact_wire("PASS hunter2\r\n"), "PASS <redacted>");
        assert_eq!(redact_wire("AUTHENTICATE dXNlcgB1c2VyAHB3"), "AUTHENTICATE <redacted>");
        assert_eq!(redact_wire("OPER admin s3cret"), "OPER <redacted>");
        assert_eq!(redact_wire("PRIVMSG NickServ :IDENTIFY me pw"), "PRIVMSG NickServ <redacted>");
        assert_eq!(redact_wire("@label=1 PRIVMSG NickServ :identify pw"), "@label=1 PRIVMSG NickServ <redacted>");
        // Ordinary traffic is untouched (minus the line ending).
        assert_eq!(redact_wire(":a!b@c PRIVMSG #x :hello there\r\n"), ":a!b@c PRIVMSG #x :hello there");
        assert_eq!(redact_wire("PRIVMSG #x :how do I identify with nickserv?"), "PRIVMSG #x :how do I identify with nickserv?");
    }

    fn translated(line: &str) -> Vec<Event> {
        let msg: Message = line.parse().unwrap();
        translate(msg, &HashMap::new(), &mut ISupport::default())
    }

    #[test]
    fn metadata_wire_forms() {
        // Value set (4 args: irc-proto leaves it Raw).
        match translated(":soju METADATA bob soju.im/blocked * 1").as_slice() {
            [Event::Metadata { target, key, value }] => {
                assert_eq!((target.as_str(), key.as_str(), value.as_deref()), ("bob", "soju.im/blocked", Some("1")));
            }
            other => panic!("unexpected: {}", other.len()),
        }
        // Cleared (3 args: irc-proto's METADATA variant).
        match translated(":srv METADATA alice pronouns *").as_slice() {
            [Event::Metadata { value: None, key, .. }] => assert_eq!(key, "pronouns"),
            other => panic!("unexpected: {}", other.len()),
        }
        // RPL_KEYVALUE answering our SET.
        match translated(":soju 761 me #c soju.im/muted * 1").as_slice() {
            [Event::Metadata { target, value, .. }] => {
                assert_eq!(target, "#c");
                assert_eq!(value.as_deref(), Some("1"));
            }
            other => panic!("unexpected: {}", other.len()),
        }
        assert!(matches!(
            translated(":soju 770 me soju.im/blocked").as_slice(),
            [Event::MetadataSubOk(k)] if k == &vec!["soju.im/blocked".to_string()]
        ));
        assert!(translated(":soju 772 me soju.im/blocked").is_empty());
    }

    #[test]
    fn monitor_targets_are_split_under_the_line_limit() {
        let nicks: Vec<String> = (0..100).map(|i| format!("nick{i:04}")).collect();
        let chunks = monitor_chunks(&nicks);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.len() <= 400));
        assert_eq!(chunks.join(",").split(',').count(), 100);
    }

    /// Drive `scram_step` through the RFC 7677 exchange, base64 on the wire.
    fn scram_round(caps: &mut CapState, phase: &mut AuthPhase, server: &str) -> (AuthOutcome, Vec<String>) {
        use base64::Engine;
        let mut sent = Vec::new();
        let data = if server == "+" {
            "+".to_string()
        } else {
            base64::engine::general_purpose::STANDARD.encode(server)
        };
        let out = scram_step(&data, &mut |c| {
            if let Command::AUTHENTICATE(a) = c {
                sent.push(a);
            }
            true
        }, phase, caps);
        (out, sent)
    }

    fn decoded(lines: &[String]) -> String {
        use base64::Engine;
        let b = base64::engine::general_purpose::STANDARD.decode(lines.concat()).unwrap();
        String::from_utf8(b).unwrap()
    }

    fn scram_caps() -> CapState {
        CapState {
            scram: Some(super::super::scram::ScramClient::with_nonce("user", "pencil", "rOprNGfwEbeRWgbNEkqO".into())),
            ..CapState::default()
        }
    }

    const SCRAM_SERVER_FIRST: &str =
        "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";

    #[test]
    fn scram_exchange_completes_on_a_valid_server_signature() {
        let mut caps = scram_caps();
        let mut phase = AuthPhase::AwaitingChallenge;
        let (_, sent) = scram_round(&mut caps, &mut phase, "+");
        assert_eq!(decoded(&sent), "n,,n=user,r=rOprNGfwEbeRWgbNEkqO");
        let (_, sent) = scram_round(&mut caps, &mut phase, SCRAM_SERVER_FIRST);
        assert!(decoded(&sent).ends_with(",p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="));
        let (out, sent) = scram_round(&mut caps, &mut phase, "v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=");
        assert!(matches!(out, AuthOutcome::Pending));
        assert_eq!(sent, vec!["+"]);
        assert_eq!(phase, AuthPhase::AwaitingResult);
    }

    #[test]
    fn scram_aborts_on_a_forged_server_signature() {
        let mut caps = scram_caps();
        let mut phase = AuthPhase::AwaitingChallenge;
        scram_round(&mut caps, &mut phase, "+");
        scram_round(&mut caps, &mut phase, SCRAM_SERVER_FIRST);
        let (out, sent) = scram_round(&mut caps, &mut phase, "v=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=");
        assert!(matches!(out, AuthOutcome::Failed(_)));
        assert_eq!(sent, vec!["*"]);
        assert!(caps.scram_fell_back, "a forged signature must never fall back to PLAIN");
    }

    #[test]
    fn scram_reassembles_a_challenge_split_at_400_bytes() {
        let mut caps = scram_caps();
        let mut phase = AuthPhase::AwaitingChallenge;
        scram_round(&mut caps, &mut phase, "+");
        let mut sent = 0;
        let out = scram_step(&"A".repeat(400), &mut |_| { sent += 1; true }, &mut phase, &mut caps);
        assert!(matches!(out, AuthOutcome::Pending));
        assert_eq!(sent, 0);
        assert_eq!(caps.sasl_buf.len(), 400, "waits for the rest of the challenge");
    }

    #[test]
    fn scram_is_chosen_only_when_offered_with_a_password() {
        let mut cfg = NetworkConfig {
            name: "t".into(), nickname: "me".into(), username: None, realname: None,
            server: "s".into(), port: 6697, use_tls: true, nick_password: None,
            sasl_username: None, sasl_password: Some("pw".into()), client_cert_path: None,
            client_cert_pass: None, channels: vec![], buddies: vec![], autoconnect: true, profile: Default::default(),
        };
        let mut caps = CapState::default();
        caps.values.insert("sasl".into(), "PLAIN,SCRAM-SHA-256".into());
        assert!(wants_scram(AuthMode::SaslPlain, &cfg, &caps));
        assert!(!wants_scram(AuthMode::SaslExternal, &cfg, &caps));
        caps.scram_fell_back = true;
        assert!(!wants_scram(AuthMode::SaslPlain, &cfg, &caps));
        caps.scram_fell_back = false;
        caps.values.insert("sasl".into(), "PLAIN,EXTERNAL".into());
        assert!(!wants_scram(AuthMode::SaslPlain, &cfg, &caps));
        cfg.sasl_password = None;
        caps.values.insert("sasl".into(), "SCRAM-SHA-256".into());
        assert!(!wants_scram(AuthMode::SaslPlain, &cfg, &caps));
    }

    fn namreply(chan: &str, names: &str) -> Message {
        Message {
            tags: None,
            prefix: None,
            command: Command::Response(
                Response::RPL_NAMREPLY,
                vec!["me".into(), "=".into(), chan.into(), names.into()],
            ),
        }
    }

    fn endofnames(chan: &str) -> Message {
        Message {
            tags: None,
            prefix: None,
            command: Command::Response(
                Response::RPL_ENDOFNAMES,
                vec!["me".into(), chan.into(), "End of /NAMES list".into()],
            ),
        }
    }

    #[test]
    fn each_353_line_emits_its_own_names_slice() {
        let batches = HashMap::new();
        let mut isupport = ISupport::default();

        // Every 353 line yields a Names event with that line's members; the UI
        // appends them so a big channel split across many lines is never lost.
        let evs = translate(namreply("#c", "@alice bob carol"), &batches, &mut isupport);
        match evs.as_slice() {
            [Event::Names { channel, members }] => {
                assert_eq!(channel, "#c");
                assert_eq!(members.len(), 3);
                assert!(members.iter().any(|m| m.nick == "alice" && m.prefixes == "@"));
                assert!(members.iter().any(|m| m.nick == "carol"));
            }
            _ => panic!("expected a Names event for the 353 line"),
        }

        let evs = translate(namreply("#c", "dave +erin"), &batches, &mut isupport);
        match evs.as_slice() {
            [Event::Names { members, .. }] => {
                assert_eq!(members.len(), 2);
                assert!(members.iter().any(|m| m.nick == "erin" && m.prefixes == "+"));
            }
            _ => panic!("expected a Names event for the second 353 line"),
        }

        // The closing 366 is swallowed (no status-buffer noise).
        assert!(translate(endofnames("#c"), &batches, &mut isupport).is_empty());
    }
}
