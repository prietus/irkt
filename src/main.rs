mod app;
mod config;
mod dict;
mod images;
mod irc;
mod keys;
mod notify;
mod spell;
mod theme;
mod ui;
mod upload;

use std::io::{self, Stdout};
use std::sync::OnceLock;

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CEvent, EventStream, KeyEventKind,
    MouseButton, MouseEventKind,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::execute;
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui_image::picker::Picker;
use tokio::sync::mpsc;

use app::App;
use app::state::Network;
use images::{ImageMsg, Images};
use irc::Event as IrcEvent;

/// Local UTC offset in seconds, captured once on the main thread before tokio
/// spawns workers (`time`'s local-offset lookup is unsound once multithreaded).
static LOCAL_OFFSET: OnceLock<i64> = OnceLock::new();

pub fn local_offset_secs() -> i64 {
    *LOCAL_OFFSET.get().unwrap_or(&0)
}

fn main() {
    let _ = LOCAL_OFFSET.set(
        time::UtcOffset::current_local_offset()
            .map(|o| o.whole_seconds() as i64)
            .unwrap_or(0),
    );

    let cfg = match config::load() {
        config::LoadResult::Loaded(c) => c,
        config::LoadResult::WroteTemplate(p) => {
            eprintln!("Wrote a starter config at {}.", p.display());
            eprintln!("Edit it with your nick/server, then run irkt again.");
            return;
        }
        config::LoadResult::Error(e) => {
            eprintln!("config error: {e}");
            return;
        }
    };

    if cfg.networks.is_empty() {
        eprintln!("No networks configured. Edit your config.toml.");
        return;
    }

    // Pin the notification app identity before any worker thread spawns
    // (matters on macOS; see notify::init).
    notify::init();

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(e) = rt.block_on(run(cfg)) {
        eprintln!("fatal: {e}");
    }
}

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Everything the UI loop reacts to, funneled through a single channel. Using
/// one channel (instead of `select!` over several) avoids cancelling a
/// half-read terminal-event future every time an IRC event arrives — that
/// race silently drops keystrokes.
enum Tick {
    Key(crossterm::event::KeyEvent),
    Resize,
    /// Mouse wheel notch: +1 scrolls up (into history), -1 down. Only emitted
    /// when `mouse = true` in config (otherwise the mouse isn't captured).
    Scroll(i32),
    /// Left-click at a screen cell `(column, row)`. Only emitted when
    /// `mouse = true`.
    Click(u16, u16),
    Irc(usize, IrcEvent),
    Image(ImageMsg),
    Upload(upload::UploadMsg),
    /// Periodic wake-up so time-based UI state (e.g. expiring a stale
    /// `… is typing` indicator) gets reaped even when nothing else happens.
    Tock,
}

async fn run(cfg: config::AppConfig) -> io::Result<()> {
    let (tick_tx, mut tick_rx) = mpsc::channel::<Tick>(1024);

    // Detect the terminal's graphics protocol (Kitty / iTerm2 / Sixel) for
    // inline images, falling back to unicode halfblocks.
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::from_fontsize((8, 16)));
    let (img_tx, mut img_rx) = mpsc::channel::<ImageMsg>(64);
    let images = Images::new(picker, img_tx);
    // Channel uploads report their result back on, bridged into the Tick loop.
    let (up_tx, mut up_rx) = mpsc::channel::<upload::UploadMsg>(16);

    // Buddy lists modified live via /monitor are stored in a sidecar so the
    // user's config.toml is never rewritten. Merge them back in here.
    let saved = config::state::load();

    let mut app = App::new(cfg.clone(), images);
    // A theme picked live with /theme (sidecar) overrides config.toml.
    if let Some(name) = &saved.theme {
        app.theme = theme::Theme::by_name(name);
    }
    // Likewise the /joins visibility toggle (sidecar) overrides config.toml.
    if let Some(hide) = saved.hide_join_part {
        app.hide_join_part = hide;
    }
    // And the /notify desktop-notification toggle (sidecar) overrides config.toml.
    if let Some(on) = saved.notifications {
        app.notifications = on;
    }
    app.up_tx = Some(up_tx);
    // Per-channel languages chosen with /lang (sidecar) drive spell-check and
    // dictionary autocomplete. The dictionaries themselves are loaded lazily
    // off disk (hunspell .dic/.aff); both maps are empty when none are present.
    app.channel_langs = saved.channel_langs.clone().into_iter().collect();
    app.dict_words = dict::load_all();
    app.spellers = spell::load_all();
    // Merge live-added highlight keywords (sidecar) with any from config.toml.
    for w in &saved.highlight_keywords {
        if !app.highlight_keywords.iter().any(|k| k.eq_ignore_ascii_case(w)) {
            app.highlight_keywords.push(w.clone());
        }
    }
    // Likewise the live /ignore and /dim nick lists merge with config.toml.
    for n in &saved.ignored_nicks {
        if !app.ignored_nicks.iter().any(|x| x.eq_ignore_ascii_case(n)) {
            app.ignored_nicks.push(n.clone());
        }
    }
    for n in &saved.dimmed_nicks {
        if !app.dimmed_nicks.iter().any(|x| x.eq_ignore_ascii_case(n)) {
            app.dimmed_nicks.push(n.clone());
        }
    }
    for (id, net_cfg) in cfg.networks.iter().enumerate() {
        if !net_cfg.autoconnect {
            continue;
        }
        let mut net_cfg = net_cfg.clone();
        if let Some(saved_buddies) = saved.buddies.get(&net_cfg.name) {
            for b in saved_buddies {
                if !net_cfg.buddies.iter().any(|x| x.eq_ignore_ascii_case(b)) {
                    net_cfg.buddies.push(b.clone());
                }
            }
        }
        // Per-worker channel, bridged into the shared channel tagged with id.
        let (wtx, mut wrx) = mpsc::channel::<IrcEvent>(256);
        let out = irc::spawn_network(&net_cfg, wtx);
        let tx = tick_tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = wrx.recv().await {
                if tx.send(Tick::Irc(id, ev)).await.is_err() {
                    break;
                }
            }
        });
        app.networks.push(Network::new(id, net_cfg, out));
    }

    if app.networks.is_empty() {
        eprintln!("No autoconnect networks. Set autoconnect = true on a [[network]].");
        return Ok(());
    }

    // Forward decoded images into the unified channel.
    {
        let tx = tick_tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = img_rx.recv().await {
                if tx.send(Tick::Image(msg)).await.is_err() {
                    break;
                }
            }
        });
    }

    // Forward upload results into the unified channel.
    {
        let tx = tick_tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = up_rx.recv().await {
                if tx.send(Tick::Upload(msg)).await.is_err() {
                    break;
                }
            }
        });
    }

    // Heartbeat so time-based UI state expires on its own. Cheap: the loop
    // only redraws on a tock if something actually changed (see expire_typing).
    {
        let tx = tick_tx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(2));
            loop {
                ticker.tick().await;
                if tx.send(Tick::Tock).await.is_err() {
                    break;
                }
            }
        });
    }

    // Dedicated terminal-input reader. Owning the EventStream in its own task
    // means its reads are never cancelled by other branches.
    {
        let tx = tick_tx.clone();
        tokio::spawn(async move {
            let mut events = EventStream::new();
            while let Some(item) = events.next().await {
                let tick = match item {
                    Ok(CEvent::Key(k)) => Some(Tick::Key(k)),
                    Ok(CEvent::Resize(_, _)) => Some(Tick::Resize),
                    Ok(CEvent::Mouse(m)) => match m.kind {
                        MouseEventKind::ScrollUp => Some(Tick::Scroll(1)),
                        MouseEventKind::ScrollDown => Some(Tick::Scroll(-1)),
                        MouseEventKind::Down(MouseButton::Left) => {
                            Some(Tick::Click(m.column, m.row))
                        }
                        _ => None,
                    },
                    Ok(_) => None,
                    Err(_) => break,
                };
                if let Some(t) = tick {
                    if tx.send(t).await.is_err() {
                        break;
                    }
                }
            }
        });
    }
    drop(tick_tx);

    let mut term = setup_terminal(cfg.mouse)?;
    term.draw(|f| ui::draw(f, &mut app))?;
    let mut view = view_key(&app);

    while let Some(first) = tick_rx.recv().await {
        // Drain the just-received tick *plus* everything already queued and
        // redraw only once for the whole batch. A redraw of the full TUI is
        // far more expensive than applying an event, so redrawing per-event
        // (the old behaviour) made the UI drain slowly during bursts —
        // history loads, big-channel joins, netsplits. While it lagged, a
        // worker would sit blocked on its full event channel, stop reading
        // its socket, and miss the server's PONG; the `irc` crate then
        // reports a ping timeout and drops the connection ("connection reset:
        // no ping response"). Coalescing keeps the workers' sockets readable.
        let mut resized = false;
        // A bare heartbeat (Tock) only forces a redraw when it actually reaps
        // something; every other tick changes visible state, so redraw.
        let mut need_redraw = false;
        let mut batch = Some(first);
        // Cap so a sustained flood can't starve the screen of redraws forever.
        let mut budget = 1024u32;
        while let Some(tick) = batch.take() {
            match tick {
                Tick::Key(key) => {
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                        keys::handle_key(&mut app, key);
                    }
                    need_redraw = true;
                }
                Tick::Resize => {
                    resized = true;
                    need_redraw = true;
                }
                Tick::Scroll(notches) => {
                    keys::scroll(&mut app, notches * 3);
                    need_redraw = true;
                }
                Tick::Click(x, y) => {
                    keys::click(&mut app, x, y);
                    need_redraw = true;
                }
                Tick::Irc(id, ev) => {
                    app.apply_event(id, ev);
                    need_redraw = true;
                }
                Tick::Image(msg) => {
                    app.images.apply(msg);
                    need_redraw = true;
                }
                Tick::Upload(msg) => {
                    app.on_upload_finished(msg);
                    need_redraw = true;
                }
                Tick::Tock => need_redraw |= app.expire_typing(),
            }
            if app.should_quit {
                break;
            }
            budget -= 1;
            if budget == 0 {
                break;
            }
            batch = tick_rx.try_recv().ok();
        }
        if app.should_quit {
            break;
        }
        if !need_redraw {
            continue;
        }
        // Inline images drawn with a terminal graphics protocol aren't erased by
        // ratatui's cell diffing, so a scroll / buffer switch / panel toggle /
        // resize would leave stale image pixels behind. Force a full clear on
        // those view changes (only when graphics are actually on screen).
        let new_view = view_key(&app);
        if (new_view != view || resized) && app.images.graphics_active() {
            term.clear()?;
        }
        view = new_view;
        term.draw(|f| ui::draw(f, &mut app))?;
        // Backlog from a bouncer/server: fetch the active buffer's initial
        // history, and (after the draw decided we're at the top) older pages.
        app.maybe_load_active_history();
        app.request_older_history();
    }

    restore_terminal(&mut term, cfg.mouse)?;
    Ok(())
}

/// A snapshot of everything that, when changed, moves where inline images sit
/// on screen: the active buffer, its scroll offset, and which side panels show.
fn view_key(app: &App) -> (usize, usize, usize, bool, bool) {
    let scroll = app.active_buffer().map(|b| b.scroll).unwrap_or(0);
    (app.active.net, app.active.buf, scroll, app.show_sidebar, app.show_members)
}

fn setup_terminal(mouse: bool) -> io::Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    // Capturing the mouse lets the wheel scroll the buffer, at the cost of the
    // terminal's native text selection (the user opts in via `mouse = true`).
    if mouse {
        execute!(stdout, EnableMouseCapture)?;
    }
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;
    term.clear()?;
    Ok(term)
}

fn restore_terminal(term: &mut Term, mouse: bool) -> io::Result<()> {
    disable_raw_mode()?;
    if mouse {
        execute!(term.backend_mut(), DisableMouseCapture)?;
    }
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    Ok(())
}
