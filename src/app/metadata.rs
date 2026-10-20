//! IRCv3 `draft/metadata-2`, ported from arkhe:
//!
//! * On a soju bouncer, the exact-nick `/ignore` list, `/mute` and `/pin` are
//!   mirrored to soju's per-target keys (`soju.im/blocked`, `soju.im/muted`,
//!   `soju.im/pinned`), so they follow you to other clients — and soju itself
//!   drops a blocked nick's messages. Each key is merged three ways on
//!   connect (server state, local state, and the state at the last sync), so
//!   changes made on either side while apart are neither lost nor revived.
//! * Elsewhere, other users' profile keys (`display-name`, `pronouns`,
//!   `avatar`) are subscribed to and kept per nick. soju rejects those keys.

use std::collections::{HashMap, HashSet};

use super::state::*;
use crate::irc::Outgoing;

pub const BLOCKED: &str = "soju.im/blocked";
pub const MUTED: &str = "soju.im/muted";
pub const PINNED: &str = "soju.im/pinned";
pub const SYNCED_KEYS: [&str; 3] = [BLOCKED, MUTED, PINNED];
pub const PROFILE_KEYS: [&str; 3] = ["avatar", "display-name", "pronouns"];

/// A target that can go on the wire as one METADATA parameter. A blank one
/// (e.g. the config template's `ignored_nicks = [""]`) would shift `SET` into
/// the target slot.
fn valid_target(t: &str) -> bool {
    !t.is_empty() && t != "*" && !t.contains([' ', ',']) && !t.starts_with(':')
}

/// Per-connection sync state for [`SYNCED_KEYS`].
#[derive(Debug, Default)]
pub struct MetaSync {
    /// Keys whose `METADATA * SUB` is out: collecting soju's current state.
    pub syncing: HashSet<String>,
    /// Keys whose initial state is merged: changes now flow both ways live.
    pub active: HashSet<String>,
    /// Per key, the targets (lowercased) soju reported as set while syncing.
    pub server: HashMap<String, HashMap<String, String>>,
}

impl Network {
    fn has_cap(&self, cap: &str) -> bool {
        self.caps.iter().any(|c| c == cap)
    }

    /// A soju bouncer (it acks its vendor caps).
    pub fn is_soju(&self) -> bool {
        self.caps.iter().any(|c| c.starts_with("soju.im/"))
    }

    /// soju keeps metadata per network, so only a connection bound to one
    /// (`user/network` login) can sync it.
    fn can_sync_metadata(&self) -> bool {
        let bound = self.cfg.sasl_user().contains('/')
            || self.cfg.username.as_deref().is_some_and(|u| u.contains('/'));
        self.has_cap("draft/metadata-2") && self.is_soju() && bound
    }

    pub fn is_muted(&self, name: &str) -> bool {
        self.muted.contains(&name.to_lowercase())
    }

    pub fn is_pinned(&self, name: &str) -> bool {
        self.pinned.contains(&name.to_lowercase())
    }

    /// Other users' profile value for `key`, if they published one.
    pub fn profile(&self, nick: &str, key: &str) -> Option<&str> {
        self.user_meta.get(&nick.to_lowercase())?.get(key).map(String::as_str)
    }

    fn send_flag(&self, key: &str, target: &str, on: bool) {
        if !valid_target(target) {
            return;
        }
        let _ = self.out.try_send(Outgoing::Raw {
            cmd: "METADATA".into(),
            args: vec![target.into(), "SET".into(), key.into(), if on { "1" } else { "0" }.into()],
        });
    }
}

impl App {
    /// After registration: subscribe to soju's flags on a bound soju
    /// connection, or to profile keys elsewhere (one `METADATA * SUB`).
    pub(super) fn start_metadata(&mut self, ni: usize) {
        let net = &mut self.networks[ni];
        net.meta = MetaSync::default();
        if !net.has_cap("draft/metadata-2") {
            return;
        }
        let mut keys: Vec<String> = Vec::new();
        if net.can_sync_metadata() {
            net.meta.syncing = SYNCED_KEYS.iter().map(|k| k.to_string()).collect();
            keys.extend(SYNCED_KEYS.iter().map(|k| k.to_string()));
        }
        if !net.is_soju() {
            keys.extend(PROFILE_KEYS.iter().map(|k| k.to_string()));
        }
        if keys.is_empty() {
            return;
        }
        let mut args = vec!["*".to_string(), "SUB".to_string()];
        args.extend(keys);
        let _ = net.out.try_send(Outgoing::Raw { cmd: "METADATA".into(), args });
    }

    /// `METADATA <target> <key> <vis> [<value>]` (or `RPL_KEYVALUE`): during
    /// the initial sync it's soju's state, afterwards a live change, possibly
    /// made from another client.
    pub(super) fn on_metadata(&mut self, ni: usize, target: &str, key: &str, value: Option<&str>) {
        let key = key.to_lowercase();
        let net = &mut self.networks[ni];
        if !valid_target(target) {
            return;
        }
        if PROFILE_KEYS.contains(&key.as_str()) {
            if net.is_channel(target) {
                return;
            }
            let entry = net.user_meta.entry(target.to_lowercase()).or_default();
            match value.filter(|v| !v.is_empty()) {
                Some(v) => {
                    entry.insert(key, v.to_string());
                }
                None => {
                    entry.remove(&key);
                }
            }
            return;
        }
        if !SYNCED_KEYS.contains(&key.as_str()) || (key == BLOCKED && net.is_channel(target)) {
            return;
        }
        let on = value == Some("1");
        if net.meta.syncing.contains(&key) {
            if on {
                net.meta.server.entry(key).or_default().insert(target.to_lowercase(), target.to_string());
            }
        } else if net.meta.active.contains(&key) {
            self.set_local_flag(ni, &key, target, on);
            self.record_synced(ni, &key, target, on);
        }
    }

    /// `RPL_METADATASUBOK <me> <key>…`: soju sends it per key, after that
    /// key's state — time to merge it.
    pub(super) fn on_metadata_sub_ok(&mut self, ni: usize, keys: &[String]) {
        for key in keys.iter().map(|k| k.to_lowercase()) {
            if self.networks[ni].meta.syncing.remove(&key) {
                self.finish_metadata_sync(ni, &key);
            }
        }
    }

    /// Three-way merge of soju's state for `key`, the local state and the
    /// state at the last sync.
    fn finish_metadata_sync(&mut self, ni: usize, key: &str) {
        let server = self.networks[ni].meta.server.remove(key).unwrap_or_default();
        let local = self.local_flags(ni, key);
        let mut synced = self.networks[ni].meta_synced.get(key).cloned().unwrap_or_default();
        synced.retain(|n| valid_target(n));
        let mut to_server: Vec<(String, bool)> = Vec::new();
        for (name, target) in &server {
            if !local.contains_key(name) {
                if synced.contains(name) {
                    to_server.push((target.clone(), false)); // cleared here while away
                } else {
                    self.set_local_flag(ni, key, target, true); // set from another client
                }
            }
        }
        for (name, target) in &local {
            if !server.contains_key(name) {
                if synced.contains(name) {
                    self.set_local_flag(ni, key, target, false); // cleared elsewhere
                } else {
                    to_server.push((target.clone(), true)); // set here while away
                }
            }
        }
        let net = &mut self.networks[ni];
        net.meta.active.insert(key.to_string());
        for (target, on) in to_server {
            net.send_flag(key, &target, on);
        }
        let now: HashSet<String> = self.local_flags(ni, key).into_keys().collect();
        self.networks[ni].meta_synced.insert(key.to_string(), now);
        self.save_synced(ni, key);
    }

    /// The local state for a key: lowercased target → name to send.
    fn local_flags(&self, ni: usize, key: &str) -> HashMap<String, String> {
        let net = &self.networks[ni];
        let set: Vec<String> = match key {
            BLOCKED => self.ignored_nicks.clone(),
            MUTED => net.muted.iter().cloned().collect(),
            _ => net.pinned.iter().cloned().collect(),
        };
        set.into_iter().filter(|n| valid_target(n)).map(|n| (n.to_lowercase(), n)).collect()
    }

    /// Change local state only (nothing is sent), persisting it.
    fn set_local_flag(&mut self, ni: usize, key: &str, target: &str, on: bool) {
        if !valid_target(target) {
            return;
        }
        let lc = target.to_lowercase();
        match key {
            BLOCKED => {
                let has = self.ignored_nicks.iter().any(|n| n.eq_ignore_ascii_case(target));
                if on && !has {
                    self.ignored_nicks.push(target.to_string());
                } else if !on && has {
                    self.ignored_nicks.retain(|n| !n.eq_ignore_ascii_case(target));
                } else {
                    return;
                }
                let _ = crate::config::state::save_ignored_nicks(&self.ignored_nicks);
            }
            _ => {
                let net = &mut self.networks[ni];
                let set = if key == MUTED { &mut net.muted } else { &mut net.pinned };
                let changed = if on { set.insert(lc) } else { set.remove(&lc) };
                if changed {
                    let list: Vec<String> = set.iter().cloned().collect();
                    let _ = crate::config::state::save_target_flags(&net.cfg.name, key, &list);
                }
            }
        }
    }

    fn record_synced(&mut self, ni: usize, key: &str, target: &str, on: bool) {
        let set = self.networks[ni].meta_synced.entry(key.to_string()).or_default();
        if on {
            set.insert(target.to_lowercase());
        } else {
            set.remove(&target.to_lowercase());
        }
        self.save_synced(ni, key);
    }

    fn save_synced(&self, ni: usize, key: &str) {
        let net = &self.networks[ni];
        let list: Vec<String> = net.meta_synced.get(key).map(|s| s.iter().cloned().collect()).unwrap_or_default();
        let _ = crate::config::state::save_metadata_synced(&net.cfg.name, key, &list);
    }

    /// A local change (a command): apply it and mirror it to soju where the
    /// key is live. Offline, or before the initial merge, nothing is sent:
    /// the next connect's merge carries it. Blocks are global in irkt, so they
    /// go to every network that syncs them.
    pub fn set_flag(&mut self, ni: usize, key: &str, target: &str, on: bool) {
        self.set_local_flag(ni, key, target, on);
        let nets: Vec<usize> = if key == BLOCKED { (0..self.networks.len()).collect() } else { vec![ni] };
        for n in nets {
            if self.networks[n].meta.active.contains(key) {
                self.networks[n].send_flag(key, target, on);
                self.record_synced(n, key, target, on);
            }
        }
    }

    /// Mirror an edit of the `/ignore` list (old → new) to soju.
    pub fn sync_ignore_edit(&mut self, before: &[String]) {
        let after = self.ignored_nicks.clone();
        let has = |list: &[String], n: &str| list.iter().any(|x| x.eq_ignore_ascii_case(n));
        let added: Vec<String> = after.iter().filter(|n| !has(before, n)).cloned().collect();
        let removed: Vec<String> = before.iter().filter(|n| !has(&after, n)).cloned().collect();
        for n in 0..self.networks.len() {
            if !self.networks[n].meta.active.contains(BLOCKED) {
                continue;
            }
            for (list, on) in [(&added, true), (&removed, false)] {
                for nick in list {
                    self.networks[n].send_flag(BLOCKED, nick, on);
                    self.record_synced(n, BLOCKED, nick, on);
                }
            }
        }
    }
}
