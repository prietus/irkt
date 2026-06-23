//! Inline media: detect URLs in messages, fetch them off the UI path, and hold
//! either a decoded terminal-graphics image (Kitty / iTerm2 / Sixel via
//! `ratatui-image`, halfblocks fallback) or an unfurled link-preview card
//! (OpenGraph / Twitter / `<title>`), keyed by URL.

use std::collections::HashMap;

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
            && self.map.values().any(|s| matches!(s, ImageState::Ready { .. }))
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
                if let Ok((proto, w, h)) = decode_image_url(http, picker, &img_url).await {
                    return Ok(Fetched::Image(proto, w, h));
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

/// GET a URL and decode it as an image (used for og:image and image-wrapper
/// `<img src>` resolution — these are separate from the original page fetch).
async fn decode_image_url(
    http: &reqwest::Client,
    picker: &Picker,
    url: &str,
) -> Result<(StatefulProtocol, u32, u32), String> {
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
    decode_image(picker, bytes.to_vec()).await
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
