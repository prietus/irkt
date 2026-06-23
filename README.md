# irkt

A modern terminal IRC client in Rust. Full IRCv3, SASL (PLAIN + EXTERNAL/CertFP),
multi-network, and inline images (Kitty / iTerm2 / Sixel, with a halfblocks fallback).

Built on [`ratatui`](https://ratatui.rs), [`ratatui-image`](https://github.com/ratatui/ratatui-image),
the [`irc`](https://crates.io/crates/irc) protocol crate, and `tokio`.

## Features

- **Multi-network** — connect to several networks at once, one worker per network,
  each with its own identity, auto-join channels, and reconnect/backoff.
- **SASL** — PLAIN and EXTERNAL (CertFP via a TLS client certificate),
  `draft/sasl-ir` fast path.
- **Full IRCv3** — message-tags, server-time, account-tag, extended-join,
  account/away/setname/chghost-notify, multi-prefix, userhost-in-names,
  echo-message, labeled-response, cap-notify (live CAP NEW/DEL), STS,
  batch (netsplit/netjoin summaries), `draft/multiline`, `draft/typing`,
  `draft/read-marker`, `draft/message-redaction`, `draft/chathistory`,
  MONITOR presence, reactions (`+draft/react`), and reply threading
  (`+draft/reply`). ISUPPORT (PREFIX/CHANTYPES/CASEMAPPING/MONITOR/CLIENTTAGDENY).
- **Inline images** — URLs in messages are fetched and classified by their
  `Content-Type` (not their file extension, so extension-less imgur/CDN links
  work). Images are drawn inline; the graphics protocol (Kitty, iTerm2, Sixel)
  is auto-detected, falling back to unicode halfblocks. Toggle with `/images`.
- **Link previews** — HTML pages become compact cards (OpenGraph / Twitter /
  `<title>` + host). Image-wrapper pages (paste sites) and `og:image`
  thumbnails are promoted to inline images. Toggle with `/unfurl`.
- **Replies & reactions** — threaded replies (`+draft/reply`) render nested
  under their parent message; reactions (`+draft/react`) show as aggregated
  emoji badges beneath the message. Pick the target message with `Alt+↑/↓` —
  `Enter` replies to it, `Alt+R` reacts (emoji via your OS picker).
- **Themes** — `dark` (default), `light`, `nord`, and `terminal` (adapts to
  your terminal's own palette and uses reverse-video highlights, so it stays
  legible on any background). Switch live with `/theme <name>`; the choice
  persists without rewriting your `config.toml`.
- **File uploads** — `/upload <path>` posts a file and drops the resulting
  URL into the composer. Uses the IRC server's advertised **FILEHOST**
  endpoint (`soju.im/FILEHOST`, authenticated with your SASL credentials) by
  default, or a **custom HTTP uploader** (pastebin / `0x0`-style) configured
  under `[upload.custom]`.
- **Quiet busy channels** — hide join/part/quit churn with `/joins` (or
  `hide_join_part` in config), and "*nick* is now known as …" lines only show
  for people who have actually spoken recently, not silent lurkers.
- **Autocomplete & spell-check** — as you type, a dim **ghost suggestion**
  completes the current `/command`, channel nick, or — for channels with a
  language set — a dictionary word; `Tab` accepts it. With a language set,
  misspelled words are **underlined** live and `Alt+S` replaces the nearest one
  with its top correction (alternatives shown in the status bar). Enable per
  channel with `/lang <code>`; dictionaries are standard hunspell `.dic`/`.aff`
  files (see [Spell-check](#spell-check--autocomplete-dictionaries)).
- **TUI** — network/channel sidebar with unread + mention badges and buddy
  presence, member list with prefixes, topic bar, typing indicator,
  tab-completion (nicks + commands), and scrollback.

## Configuration

On first run, irkt writes a starter config and prints its path
(`~/.config/irkt/config.toml` on Linux, `~/Library/Application Support/irkt/config.toml`
on macOS). Edit it with your nick and server:

```toml
[[network]]
name = "libera"
nickname = "yournick"
server = "irc.libera.chat"
channels = ["#rust"]

# SASL PLAIN
# sasl_username = "youraccount"
# sasl_password = "yourpassword"

# SASL EXTERNAL / CertFP
# client_cert_path = "/absolute/path/to/client.p12"
# client_cert_pass = "non-empty-passphrase"
```

Add more `[[network]]` blocks to connect to multiple networks.

## Keys

| Key | Action |
|-----|--------|
| `Enter` | send message / run command |
| `Tab` | complete nick, `/command`, or file path (after `/upload`) |
| `Ctrl+N` / `Ctrl+P` | next / previous buffer |
| `Alt+1`..`9` | jump to the Nth network |
| `Alt+↑` / `Alt+↓` | select a message (for reply/react); `Esc` deselects |
| `Enter` (with a message selected) | send your text as a threaded reply to it |
| `Alt+R` | react to the selected (or last) message — type/insert an emoji with your OS picker, then `Enter` |
| `Alt+S` | correct the nearest misspelled word (needs `/lang`) |
| `PageUp` / `PageDown` | scroll the buffer |
| `Alt+B` / `Alt+M` | toggle sidebar / member list |
| `Ctrl+A` / `Ctrl+E` | start / end of line |
| `Ctrl+U` / `Ctrl+W` | clear line / delete word |
| `Ctrl+C` | quit |

## Commands

`/join /part /msg /query /me /nick /topic /whois /away /mode /kick /invite`
`/raw /names /monitor (/buddy) /setname /close /server /images /unfurl /joins /theme /lang /upload /react /reply /redact /quit`

`/upload <path>` uploads a file (FILEHOST or a custom uploader) and inserts
its URL into the composer so you can add a caption before sending. The path
argument tab-completes against the filesystem (zsh/vim style): directories get
a trailing `/` and a second Tab descends into them.

`/joins [on|off]` toggles whether join/part/quit lines are shown (persisted to
the sidecar `state.toml`, so your `config.toml` is untouched).

`/lang <code>` sets the spell-check / autocomplete language for the current
channel or query (e.g. `/lang en`, `/lang es`); `/lang off` disables it and
`/lang` alone reports the current setting and the dictionaries available. The
choice is per-buffer and persisted to `state.toml`.

### Spell-check & autocomplete dictionaries

irkt reads standard **hunspell** dictionaries — a `.dic` word list (used for
ghost-text autocomplete) and its matching `.aff` affix file (used for spell
checking and corrections). Drop the pairs into a `dicts/` folder next to your
config:

- Linux: `~/.config/irkt/dicts/`
- macOS: `~/Library/Application Support/irkt/dicts/`

On Linux, the system dictionaries in `/usr/share/hunspell` and
`/usr/share/myspell/dicts` are also picked up automatically. The language code
is the first two letters of the filename, so `en_US.dic`/`en_US.aff` register as
`en`, `es_ES.*` as `es`, and so on. Without a dictionary for a language, `/lang`
will tell you what's installed; with only a `.dic` (no `.aff`) you still get
autocomplete but not spell checking.

`/react <emoji>` and `/reply <text>` act on the **selected** message (chosen
with `Alt+↑/↓`), or the most recent message with a server `msgid` if none is
selected.

Your `config.toml` is never rewritten by the app — its comments are safe. Runtime
changes (e.g. buddies added with `/monitor`) are persisted to a sidecar
`state.toml` next to it.

## Build

```sh
cargo build --release
```

The protocol backend is adapted from the author's graphical IRC client, *murmur*.

## License

MIT — see [LICENSE](LICENSE).
