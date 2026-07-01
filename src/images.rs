//! Inline media: detect URLs in messages, fetch them off the UI path, and hold
//! either a decoded terminal-graphics image (Kitty / iTerm2 / Sixel via
//! `ratatui-image`, halfblocks fallback) or an unfurled link-preview card
//! (OpenGraph / Twitter / `<title>`), keyed by URL.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use image::GenericImageView;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use tokio::sync::mpsc;

/// Cap on downloaded image size to avoid decoding hostile payloads.
const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
/// Cap on HTML we read when unfurling a link.
const MAX_HTML_BYTES: usize = 256 * 1024;

pub enum ImageState {
    Pending,
    /// Fetched but neither an image nor an unfurlable page (or it failed).
    /// Kept so we don't retry and don't reserve space for it.
    None,
    Ready {
        proto: StatefulProtocol,
        /// Decoded pixel dimensions, used to reserve the right row count.
        w: u32,
        h: u32,
    },
    /// An animated GIF/WebP. Terminals don't self-animate inline images in the
    /// alternate screen (where a TUI lives), so we drive it ourselves: one
    /// prebuilt protocol per frame, and the render/main loop advances `idx` on a
    /// timer (see `App::advance_anims`). Produced for any graphics protocol.
    Anim {
        frames: Vec<StatefulProtocol>,
        /// Per-frame display duration, parallel to `frames`.
        delays: Vec<Duration>,
        /// Index of the frame currently shown.
        idx: usize,
        /// When the current frame should give way to the next.
        next_due: Instant,
        /// Pixel dimensions, used to reserve the right row count (as for `Ready`).
        w: u32,
        h: u32,
    },
    /// An unfurled link preview, optionally with an og:image thumbnail (the
    /// `image` field is the resolved image URL, fetched separately).
    Card {
        title: String,
        desc: String,
        host: String,
        image: Option<String>,
    },
}

/// What a fetch resolved a URL to.
pub enum Fetched {
    Image(StatefulProtocol, u32, u32),
    /// An animated GIF/WebP: (per-frame protocols, per-frame delays, w, h).
    Anim(Vec<StatefulProtocol>, Vec<Duration>, u32, u32),
    Card { title: String, desc: String, host: String, image: Option<String> },
    Nothing,
}

/// Result of a fetch task, delivered back to the UI loop.
pub struct ImageMsg {
    pub url: String,
    pub fetched: Fetched,
}

pub struct Images {
    pub picker: Picker,
    pub map: HashMap<String, ImageState>,
    tx: mpsc::Sender<ImageMsg>,
    http: reqwest::Client,
}

impl Images {
    pub fn new(picker: Picker, tx: mpsc::Sender<ImageMsg>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("irkt/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        Images { picker, map: HashMap::new(), tx, http }
    }

    /// Kick off a fetch for `url` if we haven't seen it yet.
    pub fn ensure(&mut self, url: &str) {
        if self.map.contains_key(url) {
            return;
        }
        self.map.insert(url.to_string(), ImageState::Pending);
        let picker = self.picker;
        let tx = self.tx.clone();
        let http = self.http.clone();
        let url_s = url.to_string();
        tokio::spawn(async move {
            let fetched = fetch(&http, &picker, &url_s).await;
            let _ = tx.send(ImageMsg { url: url_s, fetched }).await;
        });
    }

    pub fn apply(&mut self, msg: ImageMsg) {
        let st = match msg.fetched {
            Fetched::Image(proto, w, h) => ImageState::Ready { proto, w, h },
            Fetched::Anim(frames, delays, w, h) => {
                let next_due =
                    Instant::now() + delays.first().copied().unwrap_or(Duration::from_millis(100));
                ImageState::Anim { frames, delays, idx: 0, next_due, w, h }
            }
            Fetched::Card { title, desc, host, image } => {
                ImageState::Card { title, desc, host, image }
            }
            Fetched::Nothing => ImageState::None,
        };
        self.map.insert(msg.url, st);
    }

    /// True when out-of-band terminal graphics (Kitty / iTerm2 / Sixel) are in
    /// use and at least one decoded image exists. Such graphics are painted by
    /// the terminal, not by ratatui's cell buffer, so they aren't erased by the
    /// usual cell diffing — when the view scrolls or the layout shifts, the
    /// caller must force a full clear to avoid stale image artifacts. With the
    /// halfblocks fallback images are plain cells, so this stays false.
    pub fn graphics_active(&self) -> bool {
        use ratatui_image::picker::ProtocolType;
        !matches!(self.picker.protocol_type(), ProtocolType::Halfblocks)
            && self
                .map
                .values()
                .any(|s| matches!(s, ImageState::Ready { .. } | ImageState::Anim { .. }))
    }

    /// Number of terminal rows an image of pixel size `w`×`h` needs when drawn
    /// `cols` columns wide, given the detected font cell size.
    pub fn rows_for(&self, w: u32, h: u32, cols: u16) -> u16 {
        let (cw, ch) = self.picker.font_size();
        if w == 0 || ch == 0 || cw == 0 {
            return 1;
        }
        let disp_w_px = cols as f32 * cw as f32;
        let scale = disp_w_px / w as f32;
        let disp_h_px = h as f32 * scale;
        ((disp_h_px / ch as f32).ceil() as u16).clamp(1, 20)
    }
}

async fn fetch(http: &reqwest::Client, picker: &Picker, url: &str) -> Fetched {
    fetch_inner(http, picker, url).await.unwrap_or(Fetched::Nothing)
}

async fn fetch_inner(http: &reqwest::Client, picker: &Picker, url: &str) -> Result<Fetched, String> {
    // One GET: inspect the response's Content-Type header before downloading
    // the body, so images are detected by MIME (like murmur) rather than by
    // file extension — imgur/CDN links without a `.png` suffix still work, and
    // HTML pages become link-preview cards.
    let resp = http.get(url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();

    let size = resp.content_length();

    if ct.starts_with("image/") {
        if let Some(len) = size {
            if len > MAX_IMAGE_BYTES {
                return Err("image too large".into());
            }
        }
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        // On a graphics terminal, decode an animated GIF/WebP into frames we
        // animate ourselves; everything else becomes a single static frame.
        if is_graphics(picker)
            && let Some((frames, delays, w, h)) = decode_animation(picker, bytes.to_vec()).await
        {
            return Ok(Fetched::Anim(frames, delays, w, h));
        }
        let (proto, w, h) = decode_image(picker, bytes.to_vec()).await?;
        return Ok(Fetched::Image(proto, w, h));
    }

    if ct.starts_with("text/html") || ct.starts_with("application/xhtml") {
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        let slice = &bytes[..bytes.len().min(MAX_HTML_BYTES)];
        let html = String::from_utf8_lossy(slice);
        let meta = parse_html_meta(&html);

        let has_og_or_twitter = meta.keys().any(|k| k.starts_with("og:") || k.starts_with("twitter:"));
        let has_title = meta.get("title").map(|s| !collapse_ws(s).is_empty()).unwrap_or(false);
        // A paste/file viewer whose <title> is an image filename (e.g.
        // "Screenshot ….png — pastebin") is really an image page.
        let title_is_image_name = meta.get("title").map(|t| title_looks_like_image(t)).unwrap_or(false);

        // Image-wrapper pages (paste sites etc.): no real metadata, or a
        // title that's just an image filename — promote the embedded <img>
        // to a full inline image instead of a sparse card.
        if (!has_og_or_twitter && !has_title) || title_is_image_name {
            if let Some(src) = extract_first_img_src(&html) {
                let img_url = resolve_url(url, &src);
                if let Ok(fetched) = decode_url_media(http, picker, &img_url).await {
                    return Ok(fetched);
                }
            }
        }

        let title = meta
            .get("og:title")
            .or_else(|| meta.get("twitter:title"))
            .or_else(|| meta.get("title"))
            .map(|s| collapse_ws(s))
            .unwrap_or_default();
        let mut desc = meta
            .get("og:description")
            .or_else(|| meta.get("twitter:description"))
            .or_else(|| meta.get("description"))
            .map(|s| collapse_ws(s))
            .unwrap_or_default();
        let image = meta
            .get("og:image")
            .or_else(|| meta.get("twitter:image"))
            .or_else(|| meta.get("twitter:image:src"))
            .map(|s| resolve_url(url, s));

        if title.is_empty() && desc.is_empty() && image.is_none() {
            return Ok(Fetched::Nothing);
        }
        if desc.chars().count() > 200 {
            desc = desc.chars().take(197).collect::<String>() + "…";
        }
        return Ok(Fetched::Card { title, desc, host: url_host(url), image });
    }

    // Fallback: some paste/raw endpoints serve images as text/plain or
    // application/octet-stream. If it's small enough, try decoding as an image.
    let ct_unknown = ct.is_empty() || ct == "application/octet-stream" || ct.starts_with("text/plain");
    let too_big = size.map_or(false, |s| s > MAX_IMAGE_BYTES);
    if ct_unknown && !too_big {
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        if is_graphics(picker)
            && let Some((frames, delays, w, h)) = decode_animation(picker, bytes.to_vec()).await
        {
            return Ok(Fetched::Anim(frames, delays, w, h));
        }
        if let Ok((proto, w, h)) = decode_image(picker, bytes.to_vec()).await {
            return Ok(Fetched::Image(proto, w, h));
        }
    }

    Ok(Fetched::Nothing)
}

/// Decode raw bytes into a terminal-graphics protocol on a blocking thread.
async fn decode_image(picker: &Picker, bytes: Vec<u8>) -> Result<(StatefulProtocol, u32, u32), String> {
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err("image too large".into());
    }
    let picker = *picker;
    tokio::task::spawn_blocking(move || {
        let img = image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
        let (w, h) = img.dimensions();
        let proto = picker.new_resize_protocol(img);
        Ok::<_, String>((proto, w, h))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Cap on how many frames we keep for an animation, to bound memory (each frame
/// is a decoded image held in its own protocol).
const MAX_ANIM_FRAMES: usize = 300;
/// Downscale frames wider than this before building protocols, to bound memory.
const MAX_ANIM_WIDTH: u32 = 480;

fn is_graphics(picker: &Picker) -> bool {
    !matches!(picker.protocol_type(), ratatui_image::picker::ProtocolType::Halfblocks)
}

/// Decode an animated GIF/WebP into one resize-protocol per frame plus per-frame
/// delays and the pixel dimensions. Returns `None` for a still image (one frame
/// or fewer) or anything that isn't an animated GIF/WebP, so the caller falls
/// back to a single still frame. Runs on a blocking thread — decode is CPU-bound.
async fn decode_animation(
    picker: &Picker,
    bytes: Vec<u8>,
) -> Option<(Vec<StatefulProtocol>, Vec<Duration>, u32, u32)> {
    let picker = *picker;
    tokio::task::spawn_blocking(move || {
        let frames = decode_frames(&bytes)?;
        let (w, h) = frames[0].0.dimensions();
        let mut protos = Vec::with_capacity(frames.len());
        let mut delays = Vec::with_capacity(frames.len());
        for (img, delay) in frames {
            protos.push(picker.new_resize_protocol(image::DynamicImage::ImageRgba8(img)));
            delays.push(delay);
        }
        Some((protos, delays, w, h))
    })
    .await
    .ok()
    .flatten()
}

/// Decode the frames of an animated GIF/WebP to RGBA buffers with their delays.
/// `None` unless the input is an animated (multi-frame) GIF or WebP.
fn decode_frames(bytes: &[u8]) -> Option<Vec<(image::RgbaImage, Duration)>> {
    use image::AnimationDecoder;
    use image::codecs::gif::GifDecoder;
    use image::codecs::webp::WebPDecoder;

    let is_gif = bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a");
    let is_webp = bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP";
    let raw: Vec<image::Frame> = if is_gif {
        GifDecoder::new(std::io::Cursor::new(bytes))
            .ok()?
            .into_frames()
            .take(MAX_ANIM_FRAMES)
            .collect::<Result<_, _>>()
            .ok()?
    } else if is_webp {
        WebPDecoder::new(std::io::Cursor::new(bytes))
            .ok()?
            .into_frames()
            .take(MAX_ANIM_FRAMES)
            .collect::<Result<_, _>>()
            .ok()?
    } else {
        return None;
    };
    if raw.len() <= 1 {
        return None; // a still GIF/WebP — let the normal path make one frame
    }

    let out = raw
        .into_iter()
        .map(|f| {
            // Clamp: 0ms delays (common in GIFs) play far too fast; browsers use
            // ~100ms. Floor the rest at 20ms so nothing pins the CPU.
            let d = Duration::from(f.delay());
            let delay = if d.is_zero() {
                Duration::from_millis(100)
            } else {
                d.max(Duration::from_millis(20))
            };
            let mut img = f.into_buffer();
            if img.width() > MAX_ANIM_WIDTH {
                let nh = (img.height() * MAX_ANIM_WIDTH / img.width().max(1)).max(1);
                img = image::imageops::resize(
                    &img,
                    MAX_ANIM_WIDTH,
                    nh,
                    image::imageops::FilterType::Triangle,
                );
            }
            (img, delay)
        })
        .collect();
    Some(out)
}

/// GET a URL and decode it as inline media — an animated GIF/WebP (on a graphics
/// terminal) or a still image. Used to resolve the `<img src>` embedded in a
/// paste/wrapper page, which is a separate fetch from the original page. This is
/// where paste-hosted animations get their frames: the wrapper page is HTML, so
/// only the embedded image URL reaches an image decoder.
async fn decode_url_media(
    http: &reqwest::Client,
    picker: &Picker,
    url: &str,
) -> Result<Fetched, String> {
    let resp = http.get(url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    if let Some(len) = resp.content_length() {
        if len > MAX_IMAGE_BYTES {
            return Err("image too large".into());
        }
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if is_graphics(picker)
        && let Some((frames, delays, w, h)) = decode_animation(picker, bytes.to_vec()).await
    {
        return Ok(Fetched::Anim(frames, delays, w, h));
    }
    let (proto, w, h) = decode_image(picker, bytes.to_vec()).await?;
    Ok(Fetched::Image(proto, w, h))
}

/// True if `title` contains an image-file extension followed by end-of-string
/// or a non-alphanumeric (so "shot.png — pastebin" matches but "the apng" or
/// "report.pngx" don't).
fn title_looks_like_image(title: &str) -> bool {
    let t = title.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".heic", ".heif", ".avif", ".svg"]
        .iter()
        .any(|ext| match t.find(ext) {
            Some(i) => t[i + ext.len()..].chars().next().map_or(true, |c| !c.is_alphanumeric()),
            None => false,
        })
}

/// Find the first `<img src="...">` in the document.
fn extract_first_img_src(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0usize;
    while let Some(rel) = lower[i..].find("<img") {
        let start = i + rel;
        let after_tag = start + 4;
        if after_tag >= bytes.len() {
            return None;
        }
        let next = bytes[after_tag];
        if !next.is_ascii_whitespace() && next != b'/' && next != b'>' {
            i = after_tag;
            continue;
        }
        let close = match lower[after_tag..].find('>') {
            Some(p) => after_tag + p,
            None => return None,
        };
        let attrs = &html[after_tag..close];
        if let Some(src) = extract_attr(attrs, "src") {
            if !src.is_empty() {
                return Some(src);
            }
        }
        i = close + 1;
    }
    None
}

/// Resolve `target` (absolute, protocol-relative, root-relative, or same-dir)
/// against `base`. Good enough for og:image / `<img src>` use.
fn resolve_url(base: &str, target: &str) -> String {
    let t = target.trim();
    if t.starts_with("http://") || t.starts_with("https://") {
        return t.to_string();
    }
    let scheme = if base.starts_with("http://") { "http" } else { "https" };
    if let Some(rest) = t.strip_prefix("//") {
        return format!("{scheme}://{rest}");
    }
    let host = url_host(base);
    if let Some(rest) = t.strip_prefix('/') {
        return format!("{scheme}://{host}/{rest}");
    }
    let path_start = base.find("://").map(|p| p + 3).unwrap_or(0);
    let after_host = base[path_start..].find('/').map(|p| path_start + p).unwrap_or(base.len());
    let dir_end = base[..after_host].len() + base[after_host..].rfind('/').map(|p| p + 1).unwrap_or(1);
    format!("{}{}", &base[..dir_end.min(base.len())], t)
}

/// The host portion of a URL, for the card footer (e.g. `github.com`).
fn url_host(url: &str) -> String {
    let after = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let host = after.split(['/', '?', '#']).next().unwrap_or(after);
    host.strip_prefix("www.").unwrap_or(host).to_string()
}

/// Extract og:*, twitter:*, and `<title>` from raw HTML with a tiny hand-rolled
/// parser. Ported from murmur — robust for well-formed meta tags, not a real
/// HTML parser.
fn parse_html_meta(html: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let lower = html.to_ascii_lowercase();

    if let Some(start) = lower.find("<title") {
        if let Some(gt) = lower[start..].find('>') {
            let after = start + gt + 1;
            if let Some(end_rel) = lower[after..].find("</title>") {
                let raw = &html[after..after + end_rel];
                out.insert("title".into(), html_decode(raw));
            }
        }
    }

    let bytes = lower.as_bytes();
    let mut i = 0usize;
    while let Some(rel) = lower[i..].find("<meta") {
        let start = i + rel;
        let after_tag = start + 5;
        if after_tag >= bytes.len() {
            break;
        }
        let next = bytes[after_tag];
        if !next.is_ascii_whitespace() && next != b'/' && next != b'>' {
            i = after_tag;
            continue;
        }
        let close = match lower[after_tag..].find('>') {
            Some(p) => after_tag + p,
            None => break,
        };
        let attrs = &html[after_tag..close];
        i = close + 1;

        let key = extract_attr(attrs, "property")
            .or_else(|| extract_attr(attrs, "name"))
            .map(|s| s.to_ascii_lowercase());
        let content = extract_attr(attrs, "content");
        if let (Some(k), Some(v)) = (key, content) {
            if k.starts_with("og:") || k.starts_with("twitter:") || k == "description" {
                out.entry(k).or_insert(html_decode(&v));
            }
        }
    }
    out
}

fn extract_attr(attrs: &str, name: &str) -> Option<String> {
    let lower = attrs.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find(name) {
        let pos = from + rel;
        let prev_ok = pos == 0
            || attrs.as_bytes()[pos - 1].is_ascii_whitespace()
            || attrs.as_bytes()[pos - 1] == b'/';
        let after = pos + name.len();
        if !prev_ok || after >= attrs.len() {
            from = after;
            continue;
        }
        let bytes = attrs.as_bytes();
        let mut j = after;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b'=' {
            from = after;
            continue;
        }
        j += 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= bytes.len() {
            return None;
        }
        let q = bytes[j];
        if q == b'"' || q == b'\'' {
            j += 1;
            let end = attrs[j..].find(q as char).map(|p| j + p)?;
            return Some(attrs[j..end].to_string());
        } else {
            let end = attrs[j..]
                .find(|c: char| c.is_ascii_whitespace() || c == '>' || c == '/')
                .map(|p| j + p)
                .unwrap_or(attrs.len());
            return Some(attrs[j..end].to_string());
        }
    }
    None
}

fn html_decode(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for c in s.trim().chars() {
        let ws = c.is_whitespace();
        if ws {
            if !prev_ws {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
        prev_ws = ws;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_plain_url() {
        assert_eq!(
            extract_urls("look https://e.com/a.png nice"),
            vec!["https://e.com/a.png".to_string()]
        );
    }

    #[test]
    fn strips_surrounding_punctuation() {
        assert_eq!(
            extract_urls("see (https://e.com/a.jpg)."),
            vec!["https://e.com/a.jpg".to_string()]
        );
    }

    #[test]
    fn extracts_extensionless_url() {
        // No file extension — still extracted; MIME decides image-ness later.
        assert_eq!(
            extract_urls("https://i.imgur.com/abc123"),
            vec!["https://i.imgur.com/abc123".to_string()]
        );
    }

    #[test]
    fn ignores_non_urls_and_dedups() {
        assert!(extract_urls("just text a.png here").is_empty());
        assert_eq!(
            extract_urls("https://e.com/x https://e.com/x"),
            vec!["https://e.com/x".to_string()]
        );
    }

    #[test]
    fn title_image_name_detection() {
        // Paste/screenshot pages whose title is an image filename → image page.
        assert!(title_looks_like_image("Captura de pantalla 2026-06-22.png — pastebin"));
        assert!(title_looks_like_image("shot.jpg"));
        // Real articles stay link cards.
        assert!(!title_looks_like_image("Breaking News Today"));
        assert!(!title_looks_like_image("the apng format explained"));
        assert!(!title_looks_like_image("report.pngx"));
    }

    #[test]
    fn first_img_src_extracted() {
        assert_eq!(
            extract_first_img_src(r#"<html><body><img src="/i/abc.png" alt="x"></body>"#),
            Some("/i/abc.png".to_string())
        );
    }

    #[test]
    fn url_resolution() {
        let base = "https://paste.priet.us/2a198d2bb1";
        assert_eq!(resolve_url(base, "https://cdn.x/y.png"), "https://cdn.x/y.png");
        assert_eq!(resolve_url(base, "//cdn.x/y.png"), "https://cdn.x/y.png");
        assert_eq!(resolve_url(base, "/raw/abc.png"), "https://paste.priet.us/raw/abc.png");
        assert_eq!(resolve_url(base, "img.png"), "https://paste.priet.us/img.png");
    }

    #[test]
    fn rows_for_preserves_aspect() {
        let (tx, _rx) = mpsc::channel(1);
        let imgs = Images::new(Picker::from_fontsize((10, 20)), tx);
        // 200x100 image over 40 cols: disp_w=400px, scale=2, disp_h=200px,
        // /20px-per-row = 10 rows.
        assert_eq!(imgs.rows_for(200, 100, 40), 10);
        // Square image is capped at 20 rows.
        assert_eq!(imgs.rows_for(100, 100, 60), 20);
    }

    /// Encode an `n`-frame GIF of size `w`×`h` for the animation-detection tests.
    fn make_gif(w: u32, h: u32, n: usize) -> Vec<u8> {
        use image::codecs::gif::{GifEncoder, Repeat};
        use image::{Delay, Frame, RgbaImage};
        let mut out = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut out);
            enc.set_repeat(Repeat::Infinite).unwrap();
            for i in 0..n {
                let px = if i % 2 == 0 { 255 } else { 0 };
                let img = RgbaImage::from_pixel(w, h, image::Rgba([px, px, px, 255]));
                let delay = Delay::from_numer_denom_ms(100, 1);
                enc.encode_frame(Frame::from_parts(img, 0, 0, delay)).unwrap();
            }
        }
        out
    }

    #[test]
    fn decode_frames_reads_animation_and_delays() {
        // A multi-frame GIF decodes to that many frames, each with a nonzero,
        // clamped delay and the right dimensions.
        let frames = decode_frames(&make_gif(20, 10, 4)).expect("animated");
        assert_eq!(frames.len(), 4);
        for (img, delay) in &frames {
            assert_eq!(img.dimensions(), (20, 10));
            assert!(*delay >= std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn decode_frames_rejects_still_and_non_animation() {
        // A single-frame GIF isn't animation — leave it to the still path.
        assert!(decode_frames(&make_gif(8, 8, 1)).is_none());
        // A PNG is not an animated GIF/WebP.
        let png = {
            use image::RgbaImage;
            let img = RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]));
            let mut buf = Vec::new();
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
                .unwrap();
            buf
        };
        assert!(decode_frames(&png).is_none());
    }
}

/// Return every http(s) URL in `text` (deduped, punctuation trimmed). Whether
/// each is actually an image is decided later by its Content-Type, not its
/// extension — so extension-less image links still render.
pub fn extract_urls(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        let t = word.trim_matches(|c: char| {
            matches!(
                c,
                '<' | '>' | '(' | ')' | '[' | ']' | '"' | '\'' | ',' | '.' | ';' | ':' | '!' | '?'
            )
        });
        if (t.starts_with("http://") || t.starts_with("https://")) && !out.iter().any(|u| u == t) {
            out.push(t.to_string());
        }
    }
    out
}
