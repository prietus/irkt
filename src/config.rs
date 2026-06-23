use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Top-level config: a list of networks plus a few global UI prefs.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default, rename = "network")]
    pub networks: Vec<NetworkConfig>,
    #[serde(default)]
    pub theme: Option<String>,
    /// Extra keywords (besides your nick) that count as a highlight/mention.
    #[serde(default)]
    pub highlight_keywords: Vec<String>,
    /// Nicks hidden completely from every buffer.
    #[serde(default)]
    pub ignored_nicks: Vec<String>,
    /// Render inline images by default. Toggle live with `/images`.
    #[serde(default = "default_true")]
    pub inline_images: bool,
    /// Render link preview cards (OpenGraph/title) by default. Toggle with `/unfurl`.
    #[serde(default = "default_true")]
    pub link_previews: bool,
    /// Hide join/part/quit lines by default (cuts noise in busy channels).
    /// Toggle live with `/joins`.
    #[serde(default)]
    pub hide_join_part: bool,
    /// File-upload backend (`/upload`).
    #[serde(default)]
    pub upload: UploadConfig,
}

/// File-upload backend selection. When `use_custom` is false, uploads use the
/// IRC server's advertised FILEHOST endpoint (soju.im/FILEHOST). When true,
/// they go to the user-configured `custom` HTTP uploader (e.g. a pastebin).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UploadConfig {
    #[serde(default)]
    pub use_custom: bool,
    #[serde(default)]
    pub custom: Option<CustomUploader>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomUploader {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub token: Option<String>,
    /// Multipart form field name for the file. Empty = send the raw bytes as
    /// the request body instead of a multipart form.
    #[serde(default = "default_upload_field")]
    pub field: String,
    /// How to read the resulting URL from the response: "json", "location"
    /// (header), or "text" (whole body).
    #[serde(default = "default_response_kind")]
    pub response_kind: String,
    /// JSON key holding the URL when `response_kind == "json"`.
    #[serde(default = "default_response_key")]
    pub response_key: String,
}

impl Default for CustomUploader {
    fn default() -> Self {
        Self {
            name: String::new(),
            url: String::new(),
            token: None,
            field: default_upload_field(),
            response_kind: default_response_kind(),
            response_key: default_response_key(),
        }
    }
}

fn default_upload_field() -> String {
    "file".into()
}
fn default_response_kind() -> String {
    "json".into()
}
fn default_response_key() -> String {
    "url".into()
}

/// Per-network identity + connection settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    pub name: String,
    pub nickname: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub realname: Option<String>,
    pub server: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_tls")]
    pub use_tls: bool,
    #[serde(default)]
    pub nick_password: Option<String>,
    #[serde(default)]
    pub sasl_username: Option<String>,
    #[serde(default)]
    pub sasl_password: Option<String>,
    #[serde(default)]
    pub client_cert_path: Option<String>,
    #[serde(default)]
    pub client_cert_pass: Option<String>,
    #[serde(default)]
    pub channels: Vec<String>,
    /// Buddy nicks tracked via MONITOR for online/offline presence.
    #[serde(default)]
    pub buddies: Vec<String>,
    #[serde(default = "default_true")]
    pub autoconnect: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    None,
    NickServ,
    SaslPlain,
    SaslExternal,
}

impl NetworkConfig {
    /// Auth method, derived from which credentials are set. Priority:
    /// SASL EXTERNAL (CertFP) > SASL PLAIN > NickServ IDENTIFY.
    pub fn auth_mode(&self) -> AuthMode {
        if self.client_cert_path.as_deref().is_some_and(|s| !s.is_empty()) {
            AuthMode::SaslExternal
        } else if self.sasl_password.as_deref().is_some_and(|s| !s.is_empty()) {
            AuthMode::SaslPlain
        } else if self.nick_password.as_deref().is_some_and(|s| !s.is_empty()) {
            AuthMode::NickServ
        } else {
            AuthMode::None
        }
    }

    pub fn sasl_user(&self) -> &str {
        self.sasl_username
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.nickname)
    }
}

fn default_port() -> u16 {
    6697
}
fn default_tls() -> bool {
    true
}
fn default_true() -> bool {
    true
}

pub enum LoadResult {
    Loaded(AppConfig),
    WroteTemplate(PathBuf),
    Error(String),
}

pub fn config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "irkt").map(|p| p.config_dir().join("config.toml"))
}

pub fn load() -> LoadResult {
    let Some(path) = config_path() else {
        return LoadResult::Error("could not resolve config directory".into());
    };

    if !path.exists() {
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return LoadResult::Error(format!("create {}: {e}", parent.display()));
            }
        }
        if let Err(e) = std::fs::write(&path, TEMPLATE) {
            return LoadResult::Error(format!("write {}: {e}", path.display()));
        }
        return LoadResult::WroteTemplate(path);
    }

    let text = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => return LoadResult::Error(format!("read {}: {e}", path.display())),
    };

    match toml::from_str::<AppConfig>(&text) {
        Ok(cfg) => LoadResult::Loaded(cfg),
        Err(e) => LoadResult::Error(format!("parse {}: {e}", path.display())),
    }
}

fn atomic_write(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// Runtime state persisted to a sidecar `state.toml`, so the user's hand-written
/// `config.toml` (with its comments) is never rewritten by the app. Currently
/// just per-network buddy lists modified live via `/monitor`.
pub mod state {
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    #[derive(Debug, Default, Serialize, Deserialize)]
    pub struct State {
        /// network name -> buddy nicks.
        #[serde(default)]
        pub buddies: BTreeMap<String, Vec<String>>,
        /// Active theme chosen live with `/theme` (overrides config.toml).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub theme: Option<String>,
        /// Join/part/quit visibility toggled live with `/joins` (overrides config.toml).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub hide_join_part: Option<bool>,
        /// Per-buffer language for spell-check/autocomplete, set with `/lang`.
        /// Keyed by `"<network>/<channel-lowercased>"`.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        pub channel_langs: BTreeMap<String, String>,
        /// Mention-highlight keywords added live with `/highlight` (merged with
        /// any in config.toml).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub highlight_keywords: Vec<String>,
    }

    fn path() -> Option<PathBuf> {
        super::config_path().and_then(|p| p.parent().map(|d| d.join("state.toml")))
    }

    pub fn load() -> State {
        let Some(p) = path() else { return State::default() };
        let Ok(text) = std::fs::read_to_string(&p) else {
            return State::default();
        };
        toml::from_str(&text).unwrap_or_default()
    }

    /// Persist the buddy list for one network without touching config.toml.
    pub fn save_buddies(network: &str, buddies: &[String]) -> Result<(), String> {
        let mut st = load();
        st.buddies.insert(network.to_string(), buddies.to_vec());
        store(&st)
    }

    /// Persist the chosen theme without touching config.toml.
    pub fn save_theme(theme: &str) -> Result<(), String> {
        let mut st = load();
        st.theme = Some(theme.to_string());
        store(&st)
    }

    /// Persist the join/part/quit visibility toggle without touching config.toml.
    pub fn save_hide_join_part(hide: bool) -> Result<(), String> {
        let mut st = load();
        st.hide_join_part = Some(hide);
        store(&st)
    }

    /// Persist the live `/highlight` keyword list without touching config.toml.
    pub fn save_highlight_keywords(words: &[String]) -> Result<(), String> {
        let mut st = load();
        st.highlight_keywords = words.to_vec();
        store(&st)
    }

    /// Set or clear a buffer's `/lang` choice without touching config.toml.
    /// `lang = None` removes the entry (spell-check/autocomplete off there).
    pub fn save_channel_lang(key: &str, lang: Option<&str>) -> Result<(), String> {
        let mut st = load();
        match lang {
            Some(l) => {
                st.channel_langs.insert(key.to_string(), l.to_string());
            }
            None => {
                st.channel_langs.remove(key);
            }
        }
        store(&st)
    }

    fn store(st: &State) -> Result<(), String> {
        let Some(p) = path() else {
            return Err("could not resolve config dir".into());
        };
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let body = toml::to_string_pretty(st).map_err(|e| e.to_string())?;
        super::atomic_write(&p, &body).map_err(|e| e.to_string())
    }
}

const TEMPLATE: &str = r##"# irkt config
# ====================================================================
# A modern terminal IRC client. Each [[network]] block is a separate
# IRC connection with its own identity and auto-join channels.

[[network]]
name = "libera"
nickname = "YOUR_NICK"
# username = "your_handle"     # default: same as nickname
# realname = "Your Name"       # default: same as nickname
server = "irc.libera.chat"
# port = 6697                  # default: 6697 (TLS)
# use_tls = true               # default: true
channels = ["#rust"]
# autoconnect = true           # default: true

# === Authentication (per network) =====================================
# Pick AT MOST ONE. Priority if multiple are set:
#   SASL EXTERNAL (CertFP)  >  SASL PLAIN  >  NickServ IDENTIFY
#
# (1) NickServ IDENTIFY
# nick_password = "your-nickserv-password"
#
# (2) SASL PLAIN — sasl_username defaults to your nickname.
# sasl_username = "your-account"
# sasl_password = "your-password"
#
# (3) SASL EXTERNAL / CertFP — TLS client certificate auth.
#     openssl pkcs12 -export -inkey key.pem -in cert.pem \
#         -out client.p12 -passout pass:something-non-empty
#     Then: /msg NickServ CERT ADD <sha512-of-cert-DER>
#     macOS: client_cert_pass MUST NOT be empty.
# client_cert_path = "/absolute/path/to/client.p12"
# client_cert_pass = "something-non-empty"

# Buddies tracked via MONITOR for online/offline presence.
# buddies = ["alice", "bob"]

# Add more networks by copying the [[network]] block above.

# === Global =========================================================
# Color theme: "dark" (default), "light", "terminal" (adapts to your
# terminal's palette — best legibility on unusual themes), or "nord".
# Switch live with /theme <name>.
# theme = "dark"
# Words (besides your nick) that trigger a mention highlight. You can also
# manage these live with /highlight add|del <word> (saved to state.toml).
# highlight_keywords = ["irkt", "murmur"]
# ignored_nicks = ["spammer42"]
# inline_images = true
# link_previews = true
# Hide join/part/quit lines (toggle live with /joins). Nick changes are
# always shown only for people who have spoken recently.
# hide_join_part = false

# === Spell-check & autocomplete =====================================
# Per-channel, enabled live with /lang <code> (e.g. /lang en, /lang es;
# /lang off to disable). Put hunspell .dic/.aff pairs in a "dicts" folder
# next to this file (e.g. en_US.dic + en_US.aff -> the "en" language); the
# .dic alone gives autocomplete, the .aff adds spell checking. On Linux,
# /usr/share/hunspell and /usr/share/myspell/dicts are also used.

# === File upload (/upload <path>) ===================================
# By default `/upload` posts to the IRC server's advertised FILEHOST
# endpoint (soju.im/FILEHOST), authenticating with your SASL credentials.
# To use your own HTTP uploader (pastebin / 0x0-style service) instead:
# [upload]
# use_custom = true
#   [upload.custom]
#   name = "0x0"
#   url = "https://0x0.st"
#   # token = "optional-bearer-token"
#   field = "file"          # multipart field name; "" = raw request body
#   response_kind = "text"  # "json" | "location" (header) | "text" (body)
#   response_key = "url"    # JSON key holding the URL when response_kind="json"
"##;

/// IRCv3 Strict Transport Security policy storage.
///
/// Persists `(host -> StsPolicy)` in `sts.toml` next to `config.toml`. The
/// worker parses the `sts=duration=N,port=P` CAP-LS value, calls `upsert`,
/// and on the *next* connection attempt `get_active(host)` returns the
/// override that should be applied to `NetworkConfig`.
pub mod sts {
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct StsPolicy {
        pub port: u16,
        pub expires_at: u64,
    }

    #[derive(Debug, Default, Serialize, Deserialize)]
    struct StsFile {
        #[serde(default)]
        policies: HashMap<String, StsPolicy>,
    }

    fn path() -> Option<PathBuf> {
        super::config_path().and_then(|p| p.parent().map(|d| d.join("sts.toml")))
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn load() -> StsFile {
        let Some(p) = path() else {
            return StsFile::default();
        };
        let Ok(text) = std::fs::read_to_string(&p) else {
            return StsFile::default();
        };
        toml::from_str::<StsFile>(&text).unwrap_or_default()
    }

    fn store(file: &StsFile) -> std::io::Result<()> {
        let Some(p) = path() else {
            return Err(std::io::Error::other("could not resolve config dir"));
        };
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(file).map_err(std::io::Error::other)?;
        std::fs::write(&p, text)
    }

    /// Returns the unexpired policy for `host`, if any.
    pub fn get_active(host: &str) -> Option<StsPolicy> {
        let file = load();
        let key = host.to_ascii_lowercase();
        file.policies
            .get(&key)
            .cloned()
            .filter(|p| p.expires_at > now_secs())
    }

    /// Upsert a policy. `duration_secs` is added to the current time.
    pub fn upsert(host: &str, port: u16, duration_secs: u64) -> std::io::Result<()> {
        let mut file = load();
        let expires_at = now_secs().saturating_add(duration_secs);
        file.policies
            .insert(host.to_ascii_lowercase(), StsPolicy { port, expires_at });
        store(&file)
    }
}
