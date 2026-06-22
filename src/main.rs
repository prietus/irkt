mod app;
mod config;
mod images;
mod irc;
mod keys;
mod theme;
mod ui;

use std::io::{self, Stdout};
use std::sync::OnceLock;

use crossterm::event::{Event as CEvent, EventStream, KeyEventKind};
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
    Irc(usize, IrcEvent),
    Image(ImageMsg),
}

async fn run(cfg: config::AppConfig) -> io::Result<()> {
    let (tick_tx, mut tick_rx) = mpsc::channel::<Tick>(1024);

    // Detect the terminal's graphics protocol (Kitty / iTerm2 / Sixel) for
    // inline images, falling back to unicode halfblocks.
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::from_fontsize((8, 16)));
    let (img_tx, mut img_rx) = mpsc::channel::<ImageMsg>(64);
    let images = Images::new(picker, img_tx);

    // Buddy lists modified live via /monitor are stored in a sidecar so the
    // user's config.toml is never rewritten. Merge them back in here.
    let saved = config::state::load();

    let mut app = App::new(cfg.clone(), images);
    // A theme picked live with /theme (sidecar) overrides config.toml.
    if let Some(name) = &saved.theme {
        app.theme = theme::Theme::by_name(name);
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

    let mut term = setup_terminal()?;
    term.draw(|f| ui::draw(f, &mut app))?;

    while let Some(tick) = tick_rx.recv().await {
        match tick {
            Tick::Key(key) => {
                if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    keys::handle_key(&mut app, key);
                }
            }
            Tick::Resize => {}
            Tick::Irc(id, ev) => app.apply_event(id, ev),
            Tick::Image(msg) => app.images.apply(msg),
        }
        if app.should_quit {
            break;
        }
        term.draw(|f| ui::draw(f, &mut app))?;
    }

    restore_terminal(&mut term)?;
    Ok(())
}

fn setup_terminal() -> io::Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;
    term.clear()?;
    Ok(term)
}

fn restore_terminal(term: &mut Term) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    Ok(())
}
