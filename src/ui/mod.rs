//! Terminal rendering. A single `draw` entry point lays out the sidebar, chat
//! view, member list, input line, and status bar from [`App`] state. Every
//! color comes from `app.theme` (see [`crate::theme`]).

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as RLine, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui_image::{Resize, StatefulImage};
use unicode_width::UnicodeWidthStr;

use crate::app::state::*;
use crate::images::{extract_urls, ImageState};
use crate::theme::Theme;

/// Max columns an inline image is drawn across.
const IMAGE_MAX_COLS: u16 = 60;
/// Left indent for inline images / card thumbnails, so they sit under the
/// message body (matching the link-card text indent) rather than flush-left.
const IMAGE_INDENT: u16 = 8;
/// A link-card's og:image is only a thumbnail, so it's drawn much smaller than
/// a directly-pasted image: narrow and capped to a few rows.
const CARD_THUMB_COLS: u16 = 22;
const CARD_THUMB_MAX_ROWS: u16 = 6;

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
    } else {
        // No sidebar this frame: drop any stale click targets so a click can't
        // hit a row that's no longer on screen.
        app.sidebar_rows.clear();
        app.sidebar_w = 0;
    }
    let chat_area = cols[idx];
    idx += 1;
    draw_chat(f, chat_area, app);
    if show_members {
        draw_members(f, cols[idx], app);
    } else {
        // Member panel hidden: drop stale click targets.
        app.member_rows.clear();
        app.member_x = (0, 0);
    }

    draw_input(f, input_row, app);
    draw_status(f, status_row, app);
}

fn draw_sidebar(f: &mut Frame, area: Rect, app: &mut App) {
    let t = &app.theme;
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(t.dim));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Screen rows of clickable buffer entries, for mouse hit-testing. The
    // Paragraph below doesn't wrap, so line index `i` lands on screen row
    // `inner.y + i`.
    let mut hits: Vec<(u16, usize, usize)> = Vec::new();
    let mut lines: Vec<RLine> = Vec::new();
    for (ni, net) in app.networks.iter().enumerate() {
        let sel_net = ni == app.active.net;
        let dot = net.conn.glyph();
        let dot_color = match net.conn {
            ConnState::Connected => t.good,
            ConnState::Connecting | ConnState::Reconnecting => t.warn,
            ConnState::Disconnected => t.dim,
            ConnState::Error => t.bad,
        };
        lines.push(RLine::from(vec![
            Span::styled(format!("{dot} "), Style::default().fg(dot_color)),
            Span::styled(
                net.cfg.name.clone(),
                Style::default().fg(if sel_net { t.accent } else { t.bright }).add_modifier(Modifier::BOLD),
            ),
        ]));
        for (bi, buf) in net.buffers.iter().enumerate() {
            let active = sel_net && bi == app.active.buf;
            let mut spans = Vec::new();
            let marker = if active { "▌" } else { " " };
            spans.push(Span::styled(marker, Style::default().fg(t.accent)));
            let name_style = if active {
                Style::default().fg(t.bright).add_modifier(Modifier::BOLD)
            } else if buf.mentions > 0 {
                Style::default().fg(t.bad)
            } else if buf.unread > 0 {
                Style::default().fg(t.bright)
            } else {
                Style::default().fg(t.dim)
            };
            let label = if bi == 0 { "status".to_string() } else { buf.name.clone() };
            spans.push(Span::styled(format!(" {label}"), name_style));
            if buf.mentions > 0 {
                spans.push(Span::styled(
                    format!(" ({})", buf.mentions),
                    Style::default().fg(t.bad).add_modifier(Modifier::BOLD),
                ));
            } else if buf.unread > 0 {
                spans.push(Span::styled(format!(" ({})", buf.unread), Style::default().fg(t.dim)));
            }
            hits.push((inner.y + lines.len() as u16, ni, bi));
            lines.push(RLine::from(spans));
        }
        // Buddy presence (MONITOR), if any are configured.
        if !net.cfg.buddies.is_empty() {
            lines.push(RLine::from(Span::styled("  buddies", Style::default().fg(t.dim).add_modifier(Modifier::BOLD))));
            let mut buddies = net.cfg.buddies.clone();
            buddies.sort_by_key(|b| {
                let online = net.online_buddies.contains(&b.to_lowercase());
                (!online, b.to_lowercase())
            });
            for b in &buddies {
                let online = net.online_buddies.contains(&b.to_lowercase());
                let (dot, color) = if online { ('●', t.good) } else { ('○', t.dim) };
                lines.push(RLine::from(vec![
                    Span::styled(format!("  {dot} "), Style::default().fg(color)),
                    Span::styled(b.clone(), Style::default().fg(if online { t.bright } else { t.dim })),
                ]));
            }
        }
        lines.push(RLine::from(""));
    }
    f.render_widget(Paragraph::new(lines), inner);
    // Only rows actually on screen are clickable (the Paragraph clips the rest).
    hits.retain(|&(y, _, _)| y < inner.y + inner.height);
    app.sidebar_rows = hits;
    app.sidebar_w = inner.width;
}

fn draw_chat(f: &mut Frame, area: Rect, app: &mut App) {
    // Drop last frame's click targets; they're repopulated below unless an
    // early return leaves the chat empty.
    app.chat_rows.clear();
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

    let (title, topic, loading_history) = {
        let b = &app.networks[ni].buffers[bi];
        (b.name.clone(), b.topic.clone().unwrap_or_default(), b.history_loading)
    };
    let mut header_spans = vec![
        Span::styled(
            format!(" {title} "),
            Style::default().fg(app.theme.header_fg).bg(app.theme.accent).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ];
    if loading_history {
        header_spans.push(Span::styled("⟳ history… ", Style::default().fg(app.theme.warn)));
    }
    header_spans.push(Span::styled(topic, Style::default().fg(app.theme.dim)));
    f.render_widget(Paragraph::new(RLine::from(header_spans)), header);

    let width = body.width as usize;
    let height = body.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let inline = app.inline_images;
    let unfurl = app.link_previews;
    let hide_join_part = app.hide_join_part;
    let dimmed = app.dimmed_nicks.clone();
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
    // (row_start, height_rows, width_cols, url)
    let mut placements: Vec<(usize, u16, u16, String)> = Vec::new();
    let mut sel_range: Option<(usize, usize)> = None;
    // (buffer row index, msgid) for selectable messages, converted to screen
    // coordinates after the viewport window is known.
    let mut row_msgids: Vec<(usize, String)> = Vec::new();
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
            theme: &app.theme,
            width,
            img_cols,
            inline,
            unfurl,
            images: &app.images,
            dimmed: &dimmed,
        };
        for i in 0..lines.len() {
            if is_child[i] {
                continue;
            }
            // Hide join/part/quit churn when /joins is off.
            if hide_join_part
                && matches!(lines[i].kind, LineKind::Join | LineKind::Part | LineKind::Quit)
            {
                continue;
            }
            push_message(&ctx, i, 0, &mut rows, &mut placements, &mut sel_range, &mut row_msgids);
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

    // Record where each selectable message landed on screen, for mouse clicks.
    app.chat_x = (body.x, body.x + body.width);
    app.chat_rows = row_msgids
        .into_iter()
        .filter(|&(ri, _)| ri >= start && ri < end)
        .map(|(ri, mid)| (body.y + (ri - start) as u16, mid))
        .collect();

    // Reaching the top of the scrollback while there's likely more on the server
    // asks the main loop to fetch an older CHATHISTORY page. Gated on the user
    // actually having scrolled up (scroll > 0) so short buffers don't auto-page.
    if start == 0 && scroll > 0 {
        let buf = &mut app.networks[ni].buffers[bi];
        if buf.history_loaded && !buf.history_loading && !buf.history_exhausted {
            buf.request_older = true;
        }
    }

    // Draw any images whose reserved rows are within the visible window.
    for (rstart, h, cols, url) in placements {
        let rend = rstart + h as usize;
        if rend <= start || rstart >= end {
            continue;
        }
        let vis_top = rstart.max(start);
        let vis_bot = rend.min(end);
        let y = body.y + (vis_top - start) as u16;
        let height = (vis_bot - vis_top) as u16;
        // Indent the image to sit under the message body (matching the card
        // text indent), rather than flush-left or centered.
        let indent = IMAGE_INDENT.min(body.width.saturating_sub(1));
        let w = cols.min(body.width.saturating_sub(indent));
        let rect = Rect { x: body.x + indent, y, width: w, height };
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
    theme: &'a Theme,
    width: usize,
    img_cols: u16,
    inline: bool,
    unfurl: bool,
    images: &'a crate::images::Images,
    /// Nicks whose lines render dimmed (soft ignore via `/dim`).
    dimmed: &'a [String],
}

/// Render one message and, recursively, its threaded replies nested beneath it.
fn push_message(
    ctx: &RenderCtx,
    idx: usize,
    depth: usize,
    rows: &mut Vec<RLine<'static>>,
    placements: &mut Vec<(usize, u16, u16, String)>,
    sel_range: &mut Option<(usize, usize)>,
    row_msgids: &mut Vec<(usize, String)>,
) {
    let t = ctx.theme;
    let line = &ctx.lines[idx];
    let is_selected = ctx.selection.is_some() && line.msgid.as_deref() == ctx.selection;
    let row_start = rows.len();
    let mut these = if depth == 0 {
        render_line(line, ctx.width, t)
    } else {
        render_reply_child(line, depth, ctx.width, t)
    };
    // Soft-ignore: recolor a dimmed nick's whole line to the muted tone and
    // drop the bold, so it stays legible but recedes (set with `/dim`).
    if ctx.dimmed.iter().any(|n| n.eq_ignore_ascii_case(&line.from)) {
        for rl in &mut these {
            for sp in &mut rl.spans {
                sp.style = sp.style.fg(t.dim).remove_modifier(Modifier::BOLD);
            }
        }
    }
    if is_selected {
        // Highlight the whole message — reverse-video on adaptive themes, a
        // background bar otherwise.
        for rl in &mut these {
            for sp in &mut rl.spans {
                sp.style = if t.sel_reverse {
                    sp.style.add_modifier(Modifier::REVERSED)
                } else {
                    sp.style.bg(t.sel_bg)
                };
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
                    placements.push((s, r, ctx.img_cols, url));
                }
                Some(ImageState::Card { title, desc, host, image }) if ctx.unfurl => {
                    rows.extend(render_card(title, desc, host, ctx.width, t));
                    if ctx.inline {
                        if let Some(img) = image {
                            if let Some(ImageState::Ready { w, h, .. }) = ctx.images.map.get(img) {
                                // A card thumbnail: narrow and height-capped, so a
                                // link preview never balloons to a full image.
                                let cols = CARD_THUMB_COLS.min(ctx.img_cols);
                                let r = ctx
                                    .images
                                    .rows_for(*w, *h, cols)
                                    .min(CARD_THUMB_MAX_ROWS);
                                let s = rows.len();
                                for _ in 0..r {
                                    rows.push(RLine::from(""));
                                }
                                placements.push((s, r, cols, img.clone()));
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
                        Style::default().fg(t.badge_fg).bg(t.badge_bg),
                    ));
                    spans.push(Span::raw(" "));
                }
                rows.push(RLine::from(spans));
            }
        }
    }

    // Map this message's own rows (text + image + reaction badge, before any
    // nested replies) to its msgid, so a mouse click on them selects it.
    if let Some(mid) = &line.msgid {
        for r in row_start..rows.len() {
            row_msgids.push((r, mid.clone()));
        }
    }

    // Nested replies (depth-capped as a guard against pathological chains).
    if depth < 8 {
        if let Some(kids) = ctx.children.get(&idx) {
            for &k in kids {
                push_message(ctx, k, depth + 1, rows, placements, sel_range, row_msgids);
            }
        }
    }
}

/// Render a threaded reply as a nested `↳ author: text` line under its parent.
fn render_reply_child(line: &Line, depth: usize, width: usize, t: &Theme) -> Vec<RLine<'static>> {
    let indent = 6 + depth.saturating_sub(1) * 2;
    let author = line.from.clone();
    let arrow = "↳ ";
    let head = format!("{author}: ");
    let prefix_len = indent + arrow.width() + head.width();
    let avail = width.saturating_sub(prefix_len).max(8);
    let mut style = Style::default().fg(t.text);
    if line.highlight {
        style = mention_style(style, t);
    }
    let wrapped = wrap_text(&line.text, avail);
    let cont = " ".repeat(prefix_len);
    let mut out = Vec::new();
    for (i, seg) in wrapped.iter().enumerate() {
        if i == 0 {
            out.push(RLine::from(vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(arrow.to_string(), Style::default().fg(t.dim)),
                Span::styled(head.clone(), Style::default().fg(t.nick(&author)).add_modifier(Modifier::BOLD)),
                Span::styled(seg.clone(), style),
            ]));
        } else {
            out.push(RLine::from(vec![Span::raw(cont.clone()), Span::styled(seg.clone(), style)]));
        }
    }
    if out.is_empty() {
        out.push(RLine::from(vec![
            Span::raw(" ".repeat(indent)),
            Span::styled(arrow.to_string(), Style::default().fg(t.dim)),
            Span::styled(head, Style::default().fg(t.nick(&author)).add_modifier(Modifier::BOLD)),
        ]));
    }
    out
}

/// Apply the mention highlight: reverse-video on adaptive themes, a tinted
/// background otherwise.
fn mention_style(style: Style, t: &Theme) -> Style {
    if t.mention_reverse {
        style.add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        style.bg(t.mention_bg).add_modifier(Modifier::BOLD)
    }
}

/// Format one buffer line into one or more wrapped terminal rows.
fn render_line(line: &Line, width: usize, t: &Theme) -> Vec<RLine<'static>> {
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
                Span::styled(format!("{time} "), Style::default().fg(t.dim)),
                Span::styled(format!("{nick} "), Style::default().fg(t.nick(nick)).add_modifier(Modifier::BOLD)),
            ];
            (g, time.width() + 1 + nick.width() + 1, Style::default().fg(t.text))
        }
        LineKind::Self_ => {
            let nick = &line.from;
            let g = vec![
                Span::styled(format!("{time} "), Style::default().fg(t.dim)),
                Span::styled(format!("{nick} "), Style::default().fg(t.accent).add_modifier(Modifier::BOLD)),
            ];
            (g, time.width() + 1 + nick.width() + 1, Style::default().fg(t.bright))
        }
        LineKind::Action => {
            let nick = &line.from;
            let g = vec![
                Span::styled(format!("{time} "), Style::default().fg(t.dim)),
                Span::styled(format!("* {nick} "), Style::default().fg(t.special)),
            ];
            (g, time.width() + 1 + 2 + nick.width() + 1, Style::default().fg(t.special))
        }
        LineKind::Notice => {
            let from = &line.from;
            let g = vec![
                Span::styled(format!("{time} "), Style::default().fg(t.dim)),
                Span::styled(format!("-{from}- "), Style::default().fg(t.warn)),
            ];
            (g, time.width() + 1 + from.width() + 3, Style::default().fg(t.warn))
        }
        LineKind::Join => simple_system(&time, "→", t.good, t.dim),
        LineKind::Part => simple_system(&time, "←", t.part, t.dim),
        LineKind::Quit => simple_system(&time, "⤫", t.dim, t.dim),
        LineKind::System => simple_system(&time, "·", t.dim, t.dim),
    };

    let avail = width.saturating_sub(prefix_len).max(8);
    let wrapped = wrap_text(&line.text, avail);
    let mut out = Vec::new();
    let pad = " ".repeat(prefix_len);
    let mut style = text_style;
    if line.highlight {
        style = mention_style(style, t);
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
fn render_card(title: &str, desc: &str, host: &str, width: usize, t: &Theme) -> Vec<RLine<'static>> {
    let bar = Span::styled("▏", Style::default().fg(t.accent));
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
                Style::default().fg(t.bright).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t.bright)
            };
            push(vec![Span::styled(seg, style)]);
        }
    }
    if !desc.is_empty() {
        for seg in wrap_text(desc, avail).into_iter().take(3) {
            push(vec![Span::styled(seg, Style::default().fg(t.card_desc))]);
        }
    }
    if !host.is_empty() {
        push(vec![Span::styled(host.to_string(), Style::default().fg(t.dim).add_modifier(Modifier::ITALIC))]);
    }
    out
}

fn simple_system(time: &str, glyph: &str, color: Color, dim: Color) -> (Vec<Span<'static>>, usize, Style) {
    let g = vec![
        Span::styled(format!("{time} "), Style::default().fg(dim)),
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

fn draw_members(f: &mut Frame, area: Rect, app: &mut App) {
    // Repopulated below; cleared up-front so an early return (no members) can't
    // leave stale click targets on screen.
    app.member_rows.clear();
    let t = &app.theme;
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(t.dim));
    let inner = block.inner(area);
    app.member_x = (inner.x, inner.x + inner.width);
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

    // Clickable rows, recorded for mouse hit-testing (the Paragraph doesn't
    // wrap, so line index `i` lands on screen row `inner.y + i`).
    let mut hits: Vec<(u16, String)> = Vec::new();
    let mut lines: Vec<RLine> = Vec::new();
    lines.push(RLine::from(Span::styled(
        format!("{} members", sorted.len()),
        Style::default().fg(t.dim).add_modifier(Modifier::BOLD),
    )));
    for m in &sorted {
        let prefix = m.prefixes.chars().next().unwrap_or(' ');
        let pcolor = match prefix {
            '~' | '&' | '@' => t.bad,
            '%' => t.warn,
            '+' => t.good,
            _ => t.dim,
        };
        let y = inner.y + lines.len() as u16;
        if y < inner.y + inner.height {
            hits.push((y, m.nick.clone()));
        }
        lines.push(RLine::from(vec![
            Span::styled(prefix.to_string(), Style::default().fg(pcolor)),
            Span::styled(m.nick.clone(), Style::default().fg(t.nick(&m.nick))),
        ]));
    }
    f.render_widget(Paragraph::new(lines), inner);
    app.member_rows = hits;
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
    let t = &app.theme;
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
        (label, t.warn)
    } else if let Some(from) = app.active_buffer().and_then(|b| b.selected_from()) {
        // A message is selected: the prompt shows the reply target.
        (format!("[{target} → {from}] "), t.special)
    } else {
        (format!("[{target}] "), t.accent)
    };
    // A persistent upload indicator (survives keystrokes, unlike status_msg).
    let badge = if app.uploading {
        Some(Span::styled("⋯up ", Style::default().fg(t.warn).add_modifier(Modifier::BOLD)))
    } else {
        None
    };
    let badge_w = badge.as_ref().map(|s| s.width()).unwrap_or(0);
    let prompt_w = prompt.width();
    let mut spans = Vec::new();
    if let Some(b) = badge {
        spans.push(b);
    }
    spans.push(Span::styled(prompt, Style::default().fg(color)));
    // The input itself, with misspelled words underlined in the "bad" color.
    let misspelled = app.misspelled_ranges();
    let mut at = 0usize;
    for &(s, e) in &misspelled {
        if s > at {
            spans.push(Span::raw(app.input[at..s].to_string()));
        }
        spans.push(Span::styled(
            app.input[s..e].to_string(),
            Style::default().fg(t.bad).add_modifier(Modifier::UNDERLINED),
        ));
        at = e;
    }
    if at < app.input.len() {
        spans.push(Span::raw(app.input[at..].to_string()));
    }
    // Dim ghost-text completion shown after the cursor (cursor is at the end
    // whenever a ghost exists, so it trails the input).
    if let Some(ghost) = app.ghost_suggestion() {
        spans.push(Span::styled(ghost, Style::default().fg(t.dim)));
    }
    f.render_widget(Paragraph::new(RLine::from(spans)), area);
    // Cursor position: badge + prompt width + display width of input up to cursor.
    let before = &app.input[..app.cursor.min(app.input.len())];
    let cx = area.x + (badge_w + prompt_w + before.width()) as u16;
    f.set_cursor_position((cx.min(area.x + area.width.saturating_sub(1)), area.y));
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let mut spans = Vec::new();
    if let Some(buf) = app.active_buffer() {
        if !buf.typing.is_empty() {
            let who = buf.typing.join(", ");
            let txt = if buf.typing.len() == 1 {
                format!(" {who} is typing… ")
            } else {
                format!(" {} are typing… ", buf.typing.len())
            };
            spans.push(Span::styled(txt, Style::default().fg(t.warn)));
        }
    }
    if let Some(msg) = &app.status_msg {
        spans.push(Span::styled(format!(" {msg} "), Style::default().fg(t.header_fg).bg(t.warn)));
    }
    if spans.is_empty() {
        // A selected (or react-target) message gets an action hint in place of
        // the generic help line — the workflow isn't obvious when you've just
        // clicked a message with the mouse rather than used ⌥↑↓.
        if app.react_mode {
            spans.push(Span::styled(
                " react: insert an emoji, then Enter · Esc cancel ",
                Style::default().fg(t.special).add_modifier(Modifier::BOLD),
            ));
        } else if app.active_buffer().is_some_and(|b| b.selection.is_some()) {
            spans.push(Span::styled(
                " message selected — type to reply · ⌥R react · Esc cancel ",
                Style::default().fg(t.special).add_modifier(Modifier::BOLD),
            ));
        } else {
            let net = app.active_net().map(|n| n.cfg.name.as_str()).unwrap_or("-");
            let nick = app.active_net().map(|n| n.nick.as_str()).unwrap_or("-");
            let attach = if app.has_upload_target() { " · 📎 /upload" } else { "" };
            let spell = if app.active_lang().is_some() { " · ⌥S fix" } else { "" };
            spans.push(Span::styled(
                format!(" irkt · {net} · {nick}  —  ^N/^P switch · ⌥↑↓ select · ⌥R react{attach}{spell} · Tab complete · ^C quit"),
                Style::default().fg(t.dim),
            ));
        }
    }
    f.render_widget(
        Paragraph::new(RLine::from(spans)).style(Style::default().bg(t.statusbar_bg)),
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

    fn render_with_theme(theme: crate::theme::Theme) -> Terminal<TestBackend> {
        let (img_tx, _r) = mpsc::channel(1);
        std::mem::forget(_r);
        let images = Images::new(Picker::from_fontsize((8, 16)), img_tx);
        let mut app = App::new(AppConfig::default(), images);
        let (out, _r2) = mpsc::channel(8);
        std::mem::forget(_r2);
        app.networks.push(Network::new(0, net_cfg(), out));
        let bi = app.networks[0].ensure_buffer("#chan", BufferKind::Channel);
        app.networks[0].buffers[bi].push(Line {
            time: "12:00".into(), kind: LineKind::Message, from: "alice".into(),
            text: "selected line".into(), msgid: Some("p1".into()), highlight: false, reply_to: None,
        });
        app.networks[0].buffers[bi].selection = Some("p1".into());
        app.active = ActiveBuffer { net: 0, buf: bi };
        app.show_members = false;
        app.theme = theme;
        let mut term = Terminal::new(TestBackend::new(60, 8)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        term
    }

    #[test]
    fn selection_highlight_respects_theme() {
        // The `terminal` theme uses reverse-video (legible on any background).
        let term = render_with_theme(crate::theme::terminal());
        let buf = term.backend().buffer();
        let area = *buf.area();
        let any_reversed = (0..area.height).any(|y| {
            (0..area.width).any(|x| buf[(x, y)].style().add_modifier.contains(Modifier::REVERSED))
        });
        assert!(any_reversed, "terminal theme selects with reverse-video");

        // The `dark` theme paints an explicit selection background.
        let term = render_with_theme(crate::theme::dark());
        let buf = term.backend().buffer();
        let area = *buf.area();
        let sel_bg = crate::theme::dark().sel_bg;
        let any_sel_bg = (0..area.height).any(|y| {
            (0..area.width).any(|x| buf[(x, y)].style().bg == Some(sel_bg))
        });
        assert!(any_sel_bg, "dark theme selects with a background bar");
    }

    #[test]
    fn selected_message_shows_action_hint() {
        let (img_tx, _r) = mpsc::channel(1);
        std::mem::forget(_r);
        let images = Images::new(Picker::from_fontsize((8, 16)), img_tx);
        let mut app = App::new(AppConfig::default(), images);
        let (out, _r2) = mpsc::channel(8);
        std::mem::forget(_r2);
        app.networks.push(Network::new(0, net_cfg(), out));
        let bi = app.networks[0].ensure_buffer("#chan", BufferKind::Channel);
        app.networks[0].buffers[bi].push(Line {
            time: "12:00".into(), kind: LineKind::Message, from: "alice".into(),
            text: "hi".into(), msgid: Some("m1".into()), highlight: false, reply_to: None,
        });
        app.active = ActiveBuffer { net: 0, buf: bi };
        app.show_members = false;

        // No selection: the status bar shows the generic help line.
        let mut term = Terminal::new(TestBackend::new(90, 12)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        assert!(buffer_to_string(&term).contains("^N/^P switch"), "generic help when nothing selected");

        // With a message selected, the hint replaces the help line.
        app.networks[0].buffers[bi].selection = Some("m1".into());
        term.draw(|f| draw(f, &mut app)).unwrap();
        assert!(buffer_to_string(&term).contains("type to reply"), "selection shows the reply/react hint");

        // In react mode, the hint switches to the emoji instruction.
        app.react_mode = true;
        term.draw(|f| draw(f, &mut app)).unwrap();
        assert!(buffer_to_string(&term).contains("insert an emoji"), "react mode shows the emoji hint");
    }

    #[test]
    fn chat_click_selects_message() {
        let (img_tx, _r) = mpsc::channel(1);
        std::mem::forget(_r);
        let images = Images::new(Picker::from_fontsize((8, 16)), img_tx);
        let mut app = App::new(AppConfig::default(), images);
        let (out, _r2) = mpsc::channel(8);
        std::mem::forget(_r2);
        app.networks.push(Network::new(0, net_cfg(), out));
        let bi = app.networks[0].ensure_buffer("#chan", BufferKind::Channel);
        // A message with a msgid (selectable) and one without (not selectable).
        app.networks[0].buffers[bi].push(Line {
            time: "12:00".into(), kind: LineKind::Message, from: "alice".into(),
            text: "click me".into(), msgid: Some("m1".into()), highlight: false, reply_to: None,
        });
        app.networks[0].buffers[bi].push(Line {
            time: "12:01".into(), kind: LineKind::System, from: "".into(),
            text: "no msgid here".into(), msgid: None, highlight: false, reply_to: None,
        });
        app.active = ActiveBuffer { net: 0, buf: bi };
        app.show_members = false;

        let mut term = Terminal::new(TestBackend::new(70, 12)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();

        // The selectable message got a clickable row; the system line did not.
        let &(y, ref mid) = app.chat_rows.first().expect("a selectable message row");
        assert_eq!(mid, "m1");
        assert!(app.chat_rows.iter().all(|(_, m)| m == "m1"), "only msgid lines are clickable");

        // Click selects it; clicking again toggles the selection off.
        let chat_x0 = app.chat_x.0;
        crate::keys::click(&mut app, chat_x0 + 2, y);
        assert_eq!(app.active_buffer().unwrap().selection.as_deref(), Some("m1"));
        let y2 = app.chat_rows.iter().find(|(_, m)| m == "m1").unwrap().0;
        crate::keys::click(&mut app, chat_x0 + 2, y2);
        assert_eq!(app.active_buffer().unwrap().selection, None, "second click deselects");

        // A click left of the chat area (in the sidebar) is not a message hit.
        crate::keys::click(&mut app, 1, y);
        assert_eq!(app.active_buffer().unwrap().selection, None);
    }

    #[test]
    fn member_click_opens_query() {
        use crate::irc::MemberEntry;
        let (img_tx, _r) = mpsc::channel(1);
        std::mem::forget(_r);
        let images = Images::new(Picker::from_fontsize((8, 16)), img_tx);
        let mut app = App::new(AppConfig::default(), images);
        let (out, _r2) = mpsc::channel(8);
        std::mem::forget(_r2);
        app.networks.push(Network::new(0, net_cfg(), out));
        let bi = app.networks[0].ensure_buffer("#chan", BufferKind::Channel);
        app.networks[0].members.insert(
            "#chan".into(),
            vec![
                MemberEntry { nick: "alice".into(), prefixes: "@".into(), userhost: None },
                MemberEntry { nick: "bob".into(), prefixes: String::new(), userhost: None },
            ],
        );
        app.active = ActiveBuffer { net: 0, buf: bi };
        app.show_members = true;

        let mut term = Terminal::new(TestBackend::new(80, 12)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();

        // Each member got a clickable row inside the member panel's x-range.
        let &(y, ref nick) = app
            .member_rows
            .iter()
            .find(|(_, n)| n == "bob")
            .expect("bob should have a clickable member row");
        assert_eq!(nick, "bob");
        let mx = app.member_x.0 + 1;
        crate::keys::click(&mut app, mx, y);

        // A query buffer with bob is now open and active.
        let active = app.active_buffer().unwrap();
        assert_eq!(active.name, "bob");
        assert!(matches!(active.kind, BufferKind::Query));
    }

    #[test]
    fn sidebar_click_switches_buffer() {
        let (img_tx, _r) = mpsc::channel(1);
        std::mem::forget(_r);
        let images = Images::new(Picker::from_fontsize((8, 16)), img_tx);
        let mut app = App::new(AppConfig::default(), images);
        let (out, _r2) = mpsc::channel(8);
        std::mem::forget(_r2);
        app.networks.push(Network::new(0, net_cfg(), out));
        // status (buf 0) + two channels.
        app.networks[0].ensure_buffer("#one", BufferKind::Channel);
        let two = app.networks[0].ensure_buffer("#two", BufferKind::Channel);
        app.active = ActiveBuffer { net: 0, buf: 0 };
        app.show_members = false;

        let mut term = Terminal::new(TestBackend::new(70, 12)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();

        // The draw recorded a clickable row for #two; clicking it activates it.
        let &(y, net, buf) = app
            .sidebar_rows
            .iter()
            .find(|&&(_, n, b)| n == 0 && b == two)
            .expect("#two should have a clickable sidebar row");
        assert!(app.sidebar_w > 0, "sidebar width recorded");
        crate::keys::click(&mut app, 2, y);
        assert_eq!((app.active.net, app.active.buf), (net, buf));

        // A click in the chat area (past the sidebar) must not switch buffers,
        // even when it shares a row with a sidebar entry. Reset to status first
        // so a no-op is distinguishable from a switch.
        app.active = ActiveBuffer { net: 0, buf: 0 };
        let chat_x = app.sidebar_w + 5;
        crate::keys::click(&mut app, chat_x, y);
        assert_eq!((app.active.net, app.active.buf), (0, 0), "chat-area click is ignored");
    }

    #[test]
    fn dimmed_nick_renders_muted() {
        let (img_tx, _r) = mpsc::channel(1);
        std::mem::forget(_r);
        let images = Images::new(Picker::from_fontsize((8, 16)), img_tx);
        let mut app = App::new(AppConfig::default(), images);
        let (out, _r2) = mpsc::channel(8);
        std::mem::forget(_r2);
        app.networks.push(Network::new(0, net_cfg(), out));
        let bi = app.networks[0].ensure_buffer("#chan", BufferKind::Channel);
        for (time, who, txt) in [("12:00", "alice", "hi all"), ("12:01", "bob", "spam spam")] {
            app.networks[0].buffers[bi].push(Line {
                time: time.into(), kind: LineKind::Message, from: who.into(),
                text: txt.into(), msgid: None, highlight: false, reply_to: None,
            });
        }
        app.active = ActiveBuffer { net: 0, buf: bi };
        app.show_members = false;
        app.show_sidebar = false;
        app.dimmed_nicks = vec!["bob".into()];
        let theme = crate::theme::dark();
        app.theme = theme.clone();

        let mut term = Terminal::new(TestBackend::new(60, 8)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer();
        let area = *buf.area();

        // alice isn't dimmed, so her per-nick color still shows somewhere.
        let alice_color = theme.nick("alice");
        let alice_shown = (0..area.height).any(|y| {
            (0..area.width).any(|x| buf[(x, y)].style().fg == Some(alice_color))
        });
        assert!(alice_shown, "a non-dimmed nick keeps its bright color");

        // bob is dimmed: his whole line is recolored to the muted dim tone,
        // with no bold left on the nick.
        let mut found = false;
        for y in 0..area.height {
            let row: String = (0..area.width).map(|x| buf[(x, y)].symbol()).collect();
            if !row.contains("bob") {
                continue;
            }
            found = true;
            for x in 0..area.width {
                let cell = &buf[(x, y)];
                if cell.symbol().trim().is_empty() {
                    continue;
                }
                assert_eq!(cell.style().fg, Some(theme.dim), "dimmed row uses the muted tone");
                assert!(!cell.style().add_modifier.contains(Modifier::BOLD), "dimmed row drops bold");
            }
        }
        assert!(found, "bob's line should be on screen");
    }
}
