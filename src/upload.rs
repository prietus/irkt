//! File uploads (`/upload`). Two backends, mirroring murmur:
//!
//! * **FILEHOST** — POST the raw file bytes to the IRC server's advertised
//!   `soju.im/FILEHOST` endpoint, authenticating with the same SASL
//!   credentials, and read the resulting URL from the 201 `Location` header.
//! * **Custom** — a user-configured HTTP uploader (pastebin / 0x0-style),
//!   sending the file as a multipart field or a raw body, then extracting the
//!   URL from a JSON key, the `Location` header, or the response body.
//!
//! Uploads run on a tokio task and report back to the UI loop via an
//! [`UploadMsg`] on a channel, just like inline-image fetches.

use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::mpsc;

/// A resolved upload destination for one file.
pub enum UploadJob {
    /// IRC server's advertised FILEHOST endpoint.
    Filehost {
        endpoint: String,
        auth: Option<(String, String)>,
    },
    /// User-configured custom HTTP uploader.
    Custom {
        url: String,
        token: Option<String>,
        field: String,
        response_kind: String,
        response_key: String,
    },
}

/// Result of an upload task, delivered back to the UI loop. `net` and `buf`
/// are the network/buffer indices the upload was started from, so the outcome
/// lands in the right place even if the user switched buffers meanwhile.
pub struct UploadMsg {
    pub net: usize,
    pub buf: usize,
    /// The uploaded URL on success, or an error message.
    pub result: Result<String, String>,
}

/// Spawn an upload task; the outcome arrives on `tx` as an [`UploadMsg`].
pub fn spawn(tx: mpsc::Sender<UploadMsg>, net: usize, buf: usize, job: UploadJob, path: PathBuf) {
    tokio::spawn(async move {
        let result = run(job, path).await;
        let _ = tx.send(UploadMsg { net, buf, result }).await;
    });
}

async fn run(job: UploadJob, path: PathBuf) -> Result<String, String> {
    match job {
        UploadJob::Filehost { endpoint, auth } => upload_filehost(endpoint, auth, path).await,
        UploadJob::Custom { url, token, field, response_kind, response_key } => {
            upload_custom(url, token, field, response_kind, response_key, path).await
        }
    }
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(10))
        .user_agent(concat!("irkt/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())
}

/// Read a file and return `(bytes, header-safe filename, guessed MIME)`.
async fn read_file(path: &PathBuf) -> Result<(Vec<u8>, String, String), String> {
    let bytes = tokio::fs::read(path).await.map_err(|e| format!("read file: {e}"))?;
    if bytes.is_empty() {
        return Err("file is empty".into());
    }
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .map(header_safe_filename)
        .unwrap_or_else(|| "file".into());
    let mime = mime_guess::from_path(path)
        .first_or_octet_stream()
        .essence_str()
        .to_string();
    Ok((bytes, filename, mime))
}

/// IRCv3 FILEHOST upload: POST raw bytes, expect a 201 with a `Location` URL.
async fn upload_filehost(
    endpoint: String,
    auth: Option<(String, String)>,
    path: PathBuf,
) -> Result<String, String> {
    let (bytes, filename, mime) = read_file(&path).await?;
    let mut req = client()?
        .post(&endpoint)
        .header(reqwest::header::CONTENT_TYPE, &mime)
        .header(
            reqwest::header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .body(bytes);
    if let Some((user, pass)) = auth {
        req = req.basic_auth(user, Some(pass));
    }

    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    if status.as_u16() != 201 {
        return Err(http_error(status, resp).await);
    }
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .ok_or("server returned 201 without a Location header")?;
    Ok(resolve_url(&endpoint, location))
}

/// Custom HTTP uploader: multipart field or raw body, URL from JSON/header/body.
async fn upload_custom(
    url: String,
    token: Option<String>,
    field: String,
    response_kind: String,
    response_key: String,
    path: PathBuf,
) -> Result<String, String> {
    let (bytes, filename, mime) = read_file(&path).await?;
    let mut req = client()?.post(&url);
    if let Some(t) = token.as_ref().filter(|t| !t.is_empty()) {
        req = req.bearer_auth(t);
    }
    if field.trim().is_empty() {
        req = req
            .header(reqwest::header::CONTENT_TYPE, &mime)
            .header(
                reqwest::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            )
            .body(bytes);
    } else {
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename)
            .mime_str(&mime)
            .map_err(|e| e.to_string())?;
        let form = reqwest::multipart::Form::new().part(field.trim().to_string(), part);
        req = req.multipart(form);
    }

    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() {
        return Err(http_error(status, resp).await);
    }

    match response_kind.trim() {
        "location" => {
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or("response had no Location header")?;
            Ok(resolve_url(&url, loc))
        }
        "text" => {
            let body = resp.text().await.map_err(|e| e.to_string())?;
            let u = body.trim();
            if u.is_empty() {
                Err("empty response body".into())
            } else {
                Ok(resolve_url(&url, u))
            }
        }
        _ => {
            let body = resp.text().await.map_err(|e| e.to_string())?;
            let v: serde_json::Value =
                serde_json::from_str(&body).map_err(|e| format!("parse json: {e}"))?;
            let key = if response_key.trim().is_empty() { "url" } else { response_key.trim() };
            let found = v
                .get(key)
                .and_then(|x| x.as_str())
                .ok_or_else(|| format!("key {key:?} not found in JSON response"))?;
            Ok(resolve_url(&url, found))
        }
    }
}

/// Build an error string from a failed response, including a short body snippet.
async fn http_error(status: reqwest::StatusCode, resp: reqwest::Response) -> String {
    let body = resp.text().await.unwrap_or_default();
    let snippet: String = body
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(160)
        .collect();
    if snippet.is_empty() {
        format!("HTTP {}", status.as_u16())
    } else {
        format!("HTTP {} — {snippet}", status.as_u16())
    }
}

fn header_safe_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != '\\')
        .collect();
    if cleaned.is_empty() { "file".into() } else { cleaned }
}

fn url_host(u: &str) -> &str {
    let after_scheme = u.find("://").map(|p| p + 3).unwrap_or(0);
    let rest = &u[after_scheme..];
    let end = rest.find('/').unwrap_or(rest.len());
    &rest[..end]
}

/// Resolve a possibly-relative `target` URL against the request `base`.
pub fn resolve_url(base: &str, target: &str) -> String {
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
    // Same-dir relative: keep the base up to its last '/'.
    let path_start = base.find("://").map(|p| p + 3).unwrap_or(0);
    let after_host = base[path_start..].find('/').map(|p| path_start + p).unwrap_or(base.len());
    let dir_end = after_host + base[after_host..].rfind('/').map(|p| p + 1).unwrap_or(1);
    format!("{}{}", &base[..dir_end.min(base.len())], t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_absolute_and_relative() {
        assert_eq!(resolve_url("https://0x0.st", "https://0x0.st/aB.png"), "https://0x0.st/aB.png");
        assert_eq!(resolve_url("https://x.io/api/up", "/files/9.png"), "https://x.io/files/9.png");
        assert_eq!(resolve_url("https://x.io/api/up", "9.png"), "https://x.io/api/9.png");
        assert_eq!(resolve_url("https://x.io/up", "//cdn.x.io/9.png"), "https://cdn.x.io/9.png");
    }

    #[test]
    fn filename_is_header_safe() {
        assert_eq!(header_safe_filename("a\"b\\c.png"), "abc.png");
        assert_eq!(header_safe_filename(""), "file");
    }
}
