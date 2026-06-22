//! Terminal rendering. A single `draw` entry point lays out the sidebar, chat
//! view, member list, input line, and status bar from [`App`] state.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as RLine, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui_image::{Resize, StatefulImage};
use unicode_width::UnicodeWidthStr;

use crate::app::state::*;
use crate::images::{extract_urls, ImageState};

/// Max columns an inline image is drawn across.
const IMAGE_MAX_COLS: u16 = 60;

/// A small, readable palette for hashing nicks to stable colors.
const NICK_COLORS: &[Color] = &[
    Color::LightRed,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightBlue,
    Color::LightMagenta,
    Color::LightCyan,
    Color::Rgb(0xff, 0xa5, 0x00),
    Color::Rgb(0x9b, 0x7e, 0xde),
    Color::Rgb(0x4e, 0xc9, 0xb0),
    Color::Rgb(0xd1, 0x9a, 0x66),
];

fn nick_color(nick: &str) -> Color {
    let mut h: u32 = 2166136261;
    for b in nick.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    NICK_COLORS[(h as usize) % NICK_COLORS.len()]
}

const ACCENT: Color = Color::Rgb(0x7a, 0xa2, 0xf7);
const DIM: Color = Color::Rgb(0x6c, 0x70, 0x86);
/// Background for the selected message (Alt+↑/↓ message cursor).
const SEL_BG: Color = Color::Rgb(0x2d, 0x33, 0x44);

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    // Rows: main | input | statusbar.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    let main = rows[0];
    let input_row = rows[1];
    let status_row = rows[2];

    // Columns: sidebar | chat | members.
    let active_is_channel = matches!(app.active_buffer().map(|b| b.kind), Some(BufferKind::Channel));
    let mut constraints = Vec::new();
    if app.show_sidebar {
        constraints.push(Constraint::Length(22));
    }
    constraints.push(Constraint::Min(10));
    let show_members = app.show_members && active_is_channel;
    if show_members {
        constraints.push(Constraint::Length(18));
    }
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(main);

    let mut idx = 0;
    if app.show_sidebar {
        draw_sidebar(f, cols[idx], app);
        idx += 1;
    }
    let chat_area = cols[idx];
    idx += 1;
    draw_chat(f, chat_area, app);
    if show_members {
        draw_members(f, cols[idx], app);
    }

    draw_input(f, input_row, app);
    draw_status(f, status_row, app);
}

fn draw_sidebar(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(DIM));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<RLine> = Vec::new();
    for (ni, net) in app.networks.iter().enumerate() {
        let sel_net = ni == app.active.net;
        let dot = net.conn.glyph();
        let dot_color = match net.conn {
            ConnState::Connected => Color::Green,
            ConnState::Connecting | ConnState::Reconnecting => Color::Yellow,
            ConnState::Disconnected => DIM,
            ConnState::Error => Color::Red,
        };
        lines.push(RLine::from(vec![
            Span::styled(format!("{dot} "), Style::default().fg(dot_color)),
            Span::styled(
                net.cfg.name.clone(),
                Style::default().fg(if sel_net { ACCENT } else { Color::White }).add_modifier(Modifier::BOLD),
            ),
        ]));
        for (bi, buf) in net.buffers.iter().enumerate() {
            let active = sel_net && bi == app.active.buf;
            let mut spans = Vec::new();
            let marker = if active { "▌" } else { " " };
            spans.push(Span::styled(marker, Style::default().fg(ACCENT)));
            let name_style = if active {
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
            } else if buf.mentions > 0 {
                Style::default().fg(Color::LightRed)
            } else if buf.unread > 0 {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(DIM)
            };
            let label = if bi == 0 { "status".to_string() } else { buf.name.clone() };
            spans.push(Span::styled(format!(" {label}"), name_style));
            if buf.mentions > 0 {
                spans.push(Span::styled(
                    format!(" ({})", buf.mentions),
                    Style::default().fg(Color::LightRed).add_modifier(Modifier::BOLD),
                ));
            } else if buf.unread > 0 {
                spans.push(Span::styled(format!(" ({})", buf.unread), Style::default().fg(DIM)));
            }
            lines.push(RLine::from(spans));
        }
        // Buddy presence (MONITOR), if any are configured.
        if !net.cfg.buddies.is_empty() {
            lines.push(RLine::from(Span::styled("  buddies", Style::default().fg(DIM).add_modifier(Modifier::BOLD))));
            let mut buddies = net.cfg.buddies.clone();
            buddies.sort_by_key(|b| {
                let online = net.online_buddies.contains(&b.to_lowercase());
                (!online, b.to_lowercase())
            });
            for b in &buddies {
                let online = net.online_buddies.contains(&b.to_lowercase());
                let (dot, color) = if online { ('●', Color::Green) } else { ('○', DIM) };
                lines.push(RLine::from(vec![
                    Span::styled(format!("  {dot} "), Style::default().fg(color)),
                    Span::styled(b.clone(), Style::default().fg(if online { Color::White } else { DIM })),
                ]));
            }
        }
        lines.push(RLine::from(""));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_chat(f: &mut Frame, area: Rect, app: &mut App) {
    // Topic / header row + messages.
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);
    let header = parts[0];
    let body = parts[1];

    let ni = app.active.net;
    let bi = app.active.buf;
    if app.networks.get(ni).and_then(|n| n.buffers.get(bi)).is_none() {
        return;
    }

    let (title, topic) = {
        let b = &app.networks[ni].buffers[bi];
        (b.name.clone(), b.topic.clone().unwrap_or_default())
    };
    let header_line = RLine::from(vec![
        Span::styled(format!(" {title} "), Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(topic, Style::default().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(header_line), header);

    let width = body.width as usize;
    let height = body.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let inline = app.inline_images;
    let unfurl = app.link_previews;
    let img_cols = (body.width).min(IMAGE_MAX_COLS).max(1);

    // First, ensure a fetch is in flight for any URL in this buffer. Whether
    // each turns out to be an image, a link card, or nothing is decided by its
    // Content-Type when the fetch resolves.
    if inline || unfurl {
        let urls: Vec<String> = app.networks[ni].buffers[bi]
            .lines
            .iter()
            .filter(|l| matches!(l.kind, LineKind::Message | LineKind::Self_))
            .flat_map(|l| extract_urls(&l.text))
            .collect();
        for u in &urls {
            app.images.ensure(u);
        }
        // Cards may carry an og:image thumbnail (a URL not present in the
        // message text); fetch those too.
        if inline {
            let thumbs: Vec<String> = urls
                .iter()
                .filter_map(|u| match app.images.map.get(u) {
                    Some(ImageState::Card { image: Some(img), .. }) => Some(img.clone()),
                    _ => None,
                })
                .collect();
            for t in &thumbs {
                app.images.ensure(t);
            }
        }
    }

    // Build the wrapped rows. Threaded replies are nested under their parent
    // message rather than shown in their own chronological position; lines
    // with a ready image reserve blank rows the image is later drawn over.
    let mut rows: Vec<RLine<'static>> = Vec::new();
    let mut placements: Vec<(usize, u16, String)> = Vec::new();
    let mut sel_range: Option<(usize, usize)> = None;
    {
        let buf = &app.networks[ni].buffers[bi];
        let lines = &buf.lines;

        // Index msgid -> position, then attach each reply to its parent. A
        // reply only nests if its parent appears *earlier* in the buffer,
        // which keeps the tree acyclic.
        let mut msgid_idx: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for (i, l) in lines.iter().enumerate() {
            if let Some(m) = &l.msgid {
                msgid_idx.insert(m.as_str(), i);
            }
        }
        let mut children: std::collections::HashMap<usize, Vec<usize>> = std::collections::HashMap::new();
        let mut is_child = vec![false; lines.len()];
        for (i, l) in lines.iter().enumerate() {
            if let Some(pid) = &l.reply_to {
                if let Some(&pidx) = msgid_idx.get(pid.as_str()) {
                    if pidx < i {
                        children.entry(pidx).or_default().push(i);
                        is_child[i] = true;
                    }
                }
            }
        }

        let ctx = RenderCtx {
            lines,
            children: &children,
            reactions: &buf.reactions,
            selection: buf.selection.as_deref(),
            width,
            img_cols,
            inline,
            unfurl,
            images: &app.images,
        };
        for i in 0..lines.len() {
            if is_child[i] {
                continue;
            }
            push_message(&ctx, i, 0, &mut rows, &mut placements, &mut sel_range);
        }
    }

    let total = rows.len();
    // Keep the selected message within the viewport.
    if let Some((s, e)) = sel_range {
        let buf = &mut app.networks[ni].buffers[bi];
        let mut scroll = buf.scroll.min(total.saturating_sub(1));
        let end = total.saturating_sub(scroll);
        let start = end.saturating_sub(height);
        if s < start {
            scroll = total.saturating_sub(s + height);
        } else if e > end {
            scroll = total.saturating_sub(e);
        }
        buf.scroll = scroll.min(total.saturating_sub(1));
    }
    let scroll = app.networks[ni].buffers[bi].scroll.min(total.saturating_sub(1));
    let end = total.saturating_sub(scroll);
    let start = end.saturating_sub(height);
    let visible: Vec<RLine> = rows[start..end].to_vec();
    f.render_widget(Paragraph::new(visible), body);

    // Draw any images whose reserved rows are within the visible window.
    for (rstart, h, url) in placements {
        let rend = rstart + h as usize;
        if rend <= start || rstart >= end {
            continue;
        }
        let vis_top = rstart.max(start);
        let vis_bot = rend.min(end);
        let y = body.y + (vis_top - start) as u16;
        let height = (vis_bot - vis_top) as u16;
        let rect = Rect { x: body.x, y, width: img_cols.min(body.width), height };
        if let Some(ImageState::Ready { proto, .. }) = app.images.map.get_mut(&url) {
            f.render_stateful_widget(
                StatefulImage::default().resize(Resize::Fit(None)),
                rect,
                proto,
            );
        }
    }
}

/// Shared context for the recursive message renderer.
struct RenderCtx<'a> {
    lines: &'a [Line],
    /// parent line index -> child (reply) line indices, in chronological order.
    children: &'a std::collections::HashMap<usize, Vec<usize>>,
    /// msgid -> reactions (emoji, nicks), rendered as a badge under the message.
    reactions: &'a std::collections::HashMap<String, Vec<(String, Vec<String>)>>,
    /// msgid of the currently selected message, if any.
    selection: Option<&'a str>,
    width: usize,
    img_cols: u16,
    inline: bool,
    unfurl: bool,
    images: &'a crate::images::Images,
}

/// Render one message and, recursively, its threaded replies nested beneath it.
fn push_message(
    ctx: &RenderCtx,
    idx: usize,
    depth: usize,
    rows: &mut Vec<RLine<'static>>,
    placements: &mut Vec<(usize, u16, String)>,
    sel_range: &mut Option<(usize, usize)>,
) {
    let line = &ctx.lines[idx];
    let is_selected = ctx.selection.is_some() && line.msgid.as_deref() == ctx.selection;
    let row_start = rows.len();
    let mut these = if depth == 0 {
        render_line(line, ctx.width)
    } else {
        render_reply_child(line, depth, ctx.width)
    };
    if is_selected {
        // Highlight the whole message with a selection bar.
        for rl in &mut these {
            for sp in &mut rl.spans {
                sp.style = sp.style.bg(SEL_BG);
            }
        }
        *sel_range = Some((row_start, row_start + these.len()));
    }
    rows.extend(these);

    // Inline images / link cards for this message's URLs.
    if (ctx.inline || ctx.unfurl) && matches!(line.kind, LineKind::Message | LineKind::Self_) {
        for url in extract_urls(&line.text) {
            match ctx.images.map.get(&url) {
                Some(ImageState::Ready { w, h, .. }) if ctx.inline => {
                    let r = ctx.images.rows_for(*w, *h, ctx.img_cols);
                    let s = rows.len();
                    for _ in 0..r {
                        rows.push(RLine::from(""));
                    }
                    placements.push((s, r, url));
                }
                Some(ImageState::Card { title, desc, host, image }) if ctx.unfurl => {
                    rows.extend(render_card(title, desc, host, ctx.width));
                    if ctx.inline {
                        if let Some(img) = image {
                            if let Some(ImageState::Ready { w, h, .. }) = ctx.images.map.get(img) {
                                let r = ctx.images.rows_for(*w, *h, ctx.img_cols);
                                let s = rows.len();
                                for _ in 0..r {
                                    rows.push(RLine::from(""));
                                }
                                placements.push((s, r, img.clone()));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // Reaction badges on a small line just below the message.
    if let Some(msgid) = &line.msgid {
        if let Some(rx) = ctx.reactions.get(msgid) {
            if !rx.is_empty() {
                let indent = 8 + depth * 2;
                let mut spans = vec![Span::raw(" ".repeat(indent))];
                for (emoji, nicks) in rx {
                    spans.push(Span::styled(
                        format!(" {emoji} {} ", nicks.len()),
                        Style::default().fg(Color::White).bg(Color::Rgb(0x2a, 0x2e, 0x3a)),
                    ));
                    spans.push(Span::raw(" "));
                }
                rows.push(RLine::from(spans));
            }
        }
    }

    // Nested replies (depth-capped as a guard against pathological chains).
    if depth < 8 {
        if let Some(kids) = ctx.children.get(&idx) {
            for &k in kids {
                push_message(ctx, k, depth + 1, rows, placements, sel_range);
            }
        }
    }
}

/// Render a threaded reply as a nested `↳ author: text` line under its parent.
fn render_reply_child(line: &Line, depth: usize, width: usize) -> Vec<RLine<'static>> {
    let indent = 6 + depth.saturating_sub(1) * 2;
    let author = line.from.clone();
    let arrow = "↳ ";
    let head = format!("{author}: ");
    let prefix_len = indent + arrow.width() + head.width();
    let avail = width.saturating_sub(prefix_len).max(8);
    let mut style = Style::default().fg(Color::Rgb(0xc0, 0xc5, 0xce));
    if line.highlight {
        style = style.bg(Color::Rgb(0x3a, 0x2f, 0x1a)).add_modifier(Modifier::BOLD);
    }
    let wrapped = wrap_text(&line.text, avail);
    let cont = " ".repeat(prefix_len);
    let mut out = Vec::new();
    for (i, seg) in wrapped.iter().enumerate() {
        if i == 0 {
            out.push(RLine::from(vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(arrow.to_string(), Style::default().fg(DIM)),
                Span::styled(head.clone(), Style::default().fg(nick_color(&author)).add_modifier(Modifier::BOLD)),
                Span::styled(seg.clone(), style),
            ]));
        } else {
            out.push(RLine::from(vec![Span::raw(cont.clone()), Span::styled(seg.clone(), style)]));
        }
    }
    if out.is_empty() {
        out.push(RLine::from(vec![
            Span::raw(" ".repeat(indent)),
            Span::styled(arrow.to_string(), Style::default().fg(DIM)),
            Span::styled(head, Style::default().fg(nick_color(&author)).add_modifier(Modifier::BOLD)),
        ]));
    }
    out
}

/// Format one buffer line into one or more wrapped terminal rows.
fn render_line(line: &Line, width: usize) -> Vec<RLine<'static>> {
    let time = if line.time.is_empty() {
        "     ".to_string()
    } else {
        line.time.clone()
    };
    // Build the gutter spans and the plain prefix used for wrap math.
    let (gutter, prefix_len, text_style): (Vec<Span<'static>>, usize, Style) = match line.kind {
        LineKind::Message => {
            let nick = &line.from;
            let g = vec![
                Span::styled(format!("{time} "), Style::default().fg(DIM)),
                Span::styled(format!("{nick} "), Style::default().fg(nick_color(nick)).add_modifier(Modifier::BOLD)),
            ];
            (g, time.width() + 1 + nick.width() + 1, Style::default().fg(Color::Rgb(0xc0, 0xc5, 0xce)))
        }
        LineKind::Self_ => {
            let nick = &line.from;
            let g = vec![
                Span::styled(format!("{time} "), Style::default().fg(DIM)),
                Span::styled(format!("{nick} "), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            ];
            (g, time.width() + 1 + nick.width() + 1, Style::default().fg(Color::White))
        }
        LineKind::Action => {
            let nick = &line.from;
            let g = vec![
                Span::styled(format!("{time} "), Style::default().fg(DIM)),
                Span::styled(format!("* {nick} "), Style::default().fg(Color::LightMagenta)),
            ];
            (g, time.width() + 1 + 2 + nick.width() + 1, Style::default().fg(Color::LightMagenta))
        }
        LineKind::Notice => {
            let from = &line.from;
            let g = vec![
                Span::styled(format!("{time} "), Style::default().fg(DIM)),
                Span::styled(format!("-{from}- "), Style::default().fg(Color::Yellow)),
            ];
            (g, time.width() + 1 + from.width() + 3, Style::default().fg(Color::Yellow))
        }
        LineKind::Join => simple_system(&time, "→", Color::Green),
        LineKind::Part => simple_system(&time, "←", Color::Rgb(0xb0, 0x60, 0x60)),
        LineKind::Quit => simple_system(&time, "⤫", DIM),
        LineKind::System => simple_system(&time, "·", DIM),
    };

    let avail = width.saturating_sub(prefix_len).max(8);
    let wrapped = wrap_text(&line.text, avail);
    let mut out = Vec::new();
    let pad = " ".repeat(prefix_len);
    let mut style = text_style;
    if line.highlight {
        style = style.bg(Color::Rgb(0x3a, 0x2f, 0x1a)).add_modifier(Modifier::BOLD);
    }
    for (i, seg) in wrapped.iter().enumerate() {
        let mut spans: Vec<Span<'static>> = if i == 0 {
            gutter.clone()
        } else {
            vec![Span::raw(pad.clone())]
        };
        spans.push(Span::styled(seg.clone(), style));
        out.push(RLine::from(spans));
    }
    if out.is_empty() {
        out.push(RLine::from(gutter));
    }
    out
}

/// Render an unfurled link card as a small accent-barred block: title (bold),
/// wrapped description (dim), and host (faint).
fn render_card(title: &str, desc: &str, host: &str, width: usize) -> Vec<RLine<'static>> {
    let bar = Span::styled("▏", Style::default().fg(ACCENT));
    let indent = 8usize; // align roughly under the message body
    let pad = " ".repeat(indent);
    let avail = width.saturating_sub(indent + 2).max(8);
    let mut out: Vec<RLine> = Vec::new();

    let mut push = |spans: Vec<Span<'static>>| {
        let mut row = vec![Span::raw(pad.clone()), bar.clone(), Span::raw(" ")];
        row.extend(spans);
        out.push(RLine::from(row));
    };

    if !title.is_empty() {
        for (i, seg) in wrap_text(title, avail).into_iter().enumerate().take(2) {
            let style = if i == 0 {
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            push(vec![Span::styled(seg, style)]);
        }
    }
    if !desc.is_empty() {
        for seg in wrap_text(desc, avail).into_iter().take(3) {
            push(vec![Span::styled(seg, Style::default().fg(Color::Rgb(0x9a, 0x9f, 0xb0)))]);
        }
    }
    if !host.is_empty() {
        push(vec![Span::styled(host.to_string(), Style::default().fg(DIM).add_modifier(Modifier::ITALIC))]);
    }
    out
}

fn simple_system(time: &str, glyph: &str, color: Color) -> (Vec<Span<'static>>, usize, Style) {
    let g = vec![
        Span::styled(format!("{time} "), Style::default().fg(DIM)),
        Span::styled(format!("{glyph} "), Style::default().fg(color)),
    ];
    (g, time.width() + 1 + glyph.width() + 1, Style::default().fg(color))
}

/// Greedy word-wrap by display width, splitting over-long words.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0;
    for word in text.split(' ') {
        let ww = word.width();
        if ww > width {
            // Flush current, then hard-split the long word.
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            let mut chunk = String::new();
            let mut cw = 0;
            for ch in word.chars() {
                let chw = ch.to_string().width();
                if cw + chw > width {
                    lines.push(std::mem::take(&mut chunk));
                    cw = 0;
                }
                chunk.push(ch);
                cw += chw;
            }
            cur = chunk;
            cur_w = cw;
            continue;
        }
        let add = if cur.is_empty() { ww } else { ww + 1 };
        if cur_w + add > width {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(word);
            cur_w = ww;
        } else {
            if !cur.is_empty() {
                cur.push(' ');
                cur_w += 1;
            }
            cur.push_str(word);
            cur_w += ww;
        }
    }
    lines.push(cur);
    lines
}

fn draw_members(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(DIM));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(net) = app.active_net() else { return };
    let Some(buf) = app.active_buffer() else { return };
    let key = buf.name.to_lowercase();
    let Some(members) = net.members.get(&key) else { return };

    let mut sorted = members.clone();
    sorted.sort_by(|a, b| {
        let pa = prefix_rank(&a.prefixes);
        let pb = prefix_rank(&b.prefixes);
        pa.cmp(&pb).then_with(|| a.nick.to_lowercase().cmp(&b.nick.to_lowercase()))
    });

    let mut lines: Vec<RLine> = Vec::new();
    lines.push(RLine::from(Span::styled(
        format!("{} members", sorted.len()),
        Style::default().fg(DIM).add_modifier(Modifier::BOLD),
    )));
    for m in &sorted {
        let prefix = m.prefixes.chars().next().unwrap_or(' ');
        let pcolor = match prefix {
            '~' | '&' | '@' => Color::LightRed,
            '%' => Color::Yellow,
            '+' => Color::Green,
            _ => DIM,
        };
        lines.push(RLine::from(vec![
            Span::styled(prefix.to_string(), Style::default().fg(pcolor)),
            Span::styled(m.nick.clone(), Style::default().fg(nick_color(&m.nick))),
        ]));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn prefix_rank(prefixes: &str) -> u8 {
    match prefixes.chars().next() {
        Some('~') => 0,
        Some('&') => 1,
        Some('@') => 2,
        Some('%') => 3,
        Some('+') => 4,
        _ => 5,
    }
}

fn draw_input(f: &mut Frame, area: Rect, app: &App) {
    let target = app.active_target().unwrap_or_default();
    // The author the current action targets: the selected message, or the most
    // recent one with a msgid.
    let who = app.active_buffer().and_then(|b| {
        b.selected_from().map(str::to_string).or_else(|| {
            b.lines.iter().rev().find(|l| l.msgid.is_some()).map(|l| l.from.clone())
        })
    });
    let (prompt, color) = if app.react_mode {
        let label = match &who {
            Some(w) => format!("[react → {w}] "),
            None => format!("[react {target}] "),
        };
        (label, Color::Yellow)
    } else if let Some(from) = app.active_buffer().and_then(|b| b.selected_from()) {
        // A message is selected: the prompt shows the reply target.
        (format!("[{target} → {from}] "), Color::LightMagenta)
    } else {
        (format!("[{target}] "), ACCENT)
    };
    let prompt_w = prompt.width();
    let line = RLine::from(vec![
        Span::styled(prompt, Style::default().fg(color)),
        Span::raw(app.input.clone()),
    ]);
    f.render_widget(Paragraph::new(line), area);
    // Cursor position: prompt width + display width of input up to cursor.
    let before = &app.input[..app.cursor.min(app.input.len())];
    let cx = area.x + (prompt_w + before.width()) as u16;
    f.set_cursor_position((cx.min(area.x + area.width.saturating_sub(1)), area.y));
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let mut spans = Vec::new();
    if let Some(buf) = app.active_buffer() {
        if !buf.typing.is_empty() {
            let who = buf.typing.join(", ");
            let txt = if buf.typing.len() == 1 {
                format!(" {who} is typing… ")
            } else {
                format!(" {} are typing… ", buf.typing.len())
            };
            spans.push(Span::styled(txt, Style::default().fg(Color::Yellow)));
        }
    }
    if let Some(msg) = &app.status_msg {
        spans.push(Span::styled(format!(" {msg} "), Style::default().fg(Color::Black).bg(Color::Yellow)));
    }
    if spans.is_empty() {
        let net = app.active_net().map(|n| n.cfg.name.as_str()).unwrap_or("-");
        let nick = app.active_net().map(|n| n.nick.as_str()).unwrap_or("-");
        spans.push(Span::styled(
            format!(" irkt · {net} · {nick}  —  ^N/^P switch · ⌥↑↓ select · ⌥R react · Tab complete · ^C quit"),
            Style::default().fg(DIM),
        ));
    }
    f.render_widget(
        Paragraph::new(RLine::from(spans)).style(Style::default().bg(Color::Rgb(0x16, 0x18, 0x22))),
        area,
    );
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::app::App;
    use crate::config::{AppConfig, NetworkConfig};
    use crate::images::Images;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui_image::picker::Picker;
    use tokio::sync::mpsc;

    fn net_cfg() -> NetworkConfig {
        NetworkConfig {
            name: "libera".into(), nickname: "me".into(), username: None, realname: None,
            server: "s".into(), port: 6697, use_tls: true, nick_password: None,
            sasl_username: None, sasl_password: None, client_cert_path: None, client_cert_pass: None,
            channels: vec![], buddies: vec![], autoconnect: true,
        }
    }

    fn buffer_to_string(term: &Terminal<TestBackend>) -> String {
        let buf = term.backend().buffer();
        let area = *buf.area();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn renders_reply_quote() {
        let (img_tx, _r) = mpsc::channel(1);
        std::mem::forget(_r);
        let images = Images::new(Picker::from_fontsize((8, 16)), img_tx);
        let mut app = App::new(AppConfig::default(), images);
        let (out, _r2) = mpsc::channel(8);
        std::mem::forget(_r2);
        app.networks.push(Network::new(0, net_cfg(), out));

        // A channel buffer with a parent message and a threaded reply to it.
        let net = &mut app.networks[0];
        let bi = net.ensure_buffer("#chan", BufferKind::Channel);
        net.buffers[bi].push(Line {
            time: "12:00".into(), kind: LineKind::Message, from: "alice".into(),
            text: "are we still meeting at 3pm today?".into(),
            msgid: Some("p1".into()), highlight: false, reply_to: None,
        });
        net.buffers[bi].push(Line {
            time: "12:01".into(), kind: LineKind::Message, from: "bob".into(),
            text: "yes, see you then".into(),
            msgid: Some("m2".into()), highlight: false, reply_to: Some("p1".into()),
        });
        // Two people react to bob's reply with a heart, one with thumbs up.
        net.buffers[bi].add_reaction("m2".into(), "♥".into(), "carol".into());
        net.buffers[bi].add_reaction("m2".into(), "♥".into(), "dave".into());
        net.buffers[bi].add_reaction("m2".into(), "+1".into(), "carol".into());
        app.active = ActiveBuffer { net: 0, buf: bi };
        app.show_members = false;

        let backend = TestBackend::new(70, 10);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();

        let screen = buffer_to_string(&term);
        println!("\n===== RENDERED CHAT =====\n{screen}=========================");
        assert!(screen.contains('↳'), "nested reply arrow should render");
        // The reply is nested under the parent, showing the reply author (bob).
        assert!(screen.contains("↳ bob:"), "reply shows nested under parent with its author");
        // It must NOT also appear as a standalone timestamped line.
        assert!(!screen.contains("12:01"), "reply should not render in its own chronological row");
        // Reactions appear as an aggregated badge under the message.
        assert!(screen.contains("♥ 2"), "heart reaction aggregated to count 2");
        assert!(screen.contains("+1 1"), "thumbs-up reaction shown with count 1");
    }

    #[test]
    fn selected_message_shows_reply_chip() {
        let (img_tx, _r) = mpsc::channel(1);
        std::mem::forget(_r);
        let images = Images::new(Picker::from_fontsize((8, 16)), img_tx);
        let mut app = App::new(AppConfig::default(), images);
        let (out, _r2) = mpsc::channel(8);
        std::mem::forget(_r2);
        app.networks.push(Network::new(0, net_cfg(), out));

        let net = &mut app.networks[0];
        let bi = net.ensure_buffer("#chan", BufferKind::Channel);
        net.buffers[bi].push(Line {
            time: "12:00".into(), kind: LineKind::Message, from: "alice".into(),
            text: "anyone around?".into(), msgid: Some("p1".into()), highlight: false, reply_to: None,
        });
        net.buffers[bi].push(Line {
            time: "12:01".into(), kind: LineKind::Message, from: "bob".into(),
            text: "what's up".into(), msgid: Some("p2".into()), highlight: false, reply_to: None,
        });
        // Select alice's message (Alt+Up Alt+Up).
        net.buffers[bi].selection = Some("p1".into());
        app.active = ActiveBuffer { net: 0, buf: bi };
        app.show_members = false;
        app.input = "yes! here".into();
        app.cursor = app.input.len();

        let backend = TestBackend::new(70, 10);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();

        let screen = buffer_to_string(&term);
        println!("\n===== SELECTION + REPLY CHIP =====\n{screen}==================================");
        // The input prompt shows the reply target.
        assert!(screen.contains("→ alice"), "prompt shows the reply target chip");
        assert!(screen.contains("yes! here"), "composed text shown");
    }
}
