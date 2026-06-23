//! `/upload` glue: resolve which backend to use, spawn the upload task, and
//! handle the result. The async work itself lives in [`crate::upload`].

use super::state::*;
use crate::upload::{UploadJob, UploadMsg};

impl App {
    /// True when there's a usable upload destination: a configured custom
    /// uploader, or a FILEHOST endpoint advertised by the active network.
    pub fn has_upload_target(&self) -> bool {
        if self.upload_cfg.use_custom {
            self.upload_cfg
                .custom
                .as_ref()
                .is_some_and(|c| !c.url.trim().is_empty())
        } else {
            self.active_net()
                .is_some_and(|n| n.isupport.filehost.is_some())
        }
    }

    /// Build the concrete upload job for the active backend. `Ok(None)` means
    /// no target is configured; `Err(msg)` means a target exists but can't be
    /// used (e.g. a plaintext endpoint over a TLS connection).
    fn resolve_upload_job(&self) -> Result<Option<UploadJob>, String> {
        if self.upload_cfg.use_custom {
            let Some(c) = self.upload_cfg.custom.as_ref() else {
                return Ok(None);
            };
            if c.url.trim().is_empty() {
                return Ok(None);
            }
            return Ok(Some(UploadJob::Custom {
                url: c.url.trim().to_string(),
                token: c.token.clone().filter(|t| !t.is_empty()),
                field: c.field.clone(),
                response_kind: c.response_kind.clone(),
                response_key: c.response_key.clone(),
            }));
        }
        let Some(net) = self.active_net() else {
            return Ok(None);
        };
        let Some(endpoint) = net.isupport.filehost.clone() else {
            return Ok(None);
        };
        if net.cfg.use_tls && endpoint.starts_with("http://") {
            return Err(
                "upload refused: FILEHOST endpoint is plaintext http:// over a TLS connection"
                    .into(),
            );
        }
        // FILEHOST authenticates with the same SASL credentials as the IRC link.
        let auth = {
            let pass = net.cfg.sasl_password.clone().unwrap_or_default();
            if pass.is_empty() {
                None
            } else {
                Some((net.cfg.sasl_user().to_string(), pass))
            }
        };
        Ok(Some(UploadJob::Filehost { endpoint, auth }))
    }

    /// Start uploading `path` (already tilde-expanded) on the active backend.
    pub fn start_upload(&mut self, path: std::path::PathBuf) {
        if self.uploading {
            self.set_status("an upload is already in progress");
            return;
        }
        if !path.is_file() {
            self.set_status(format!("not a file: {}", path.display()));
            return;
        }
        let job = match self.resolve_upload_job() {
            Ok(Some(job)) => job,
            Ok(None) => {
                self.set_status(
                    "no upload target: this network advertises no FILEHOST; set [upload.custom] in config",
                );
                return;
            }
            Err(msg) => {
                self.set_status(msg);
                return;
            }
        };
        let Some(tx) = self.up_tx.clone() else {
            return; // headless/tests: no result channel wired up
        };
        let (net, buf) = (self.active.net, self.active.buf);
        self.uploading = true;
        self.set_status(format!("uploading {}…", path.display()));
        crate::upload::spawn(tx, net, buf, job, path);
    }

    /// Apply an upload result: on success, drop the URL into the composer so
    /// the user can add a caption and send it; on failure, surface the error.
    pub fn on_upload_finished(&mut self, msg: UploadMsg) {
        self.uploading = false;
        match msg.result {
            Ok(url) => {
                if self.input.is_empty() {
                    self.input = url;
                } else {
                    if !self.input.ends_with(' ') {
                        self.input.push(' ');
                    }
                    self.input.push_str(&url);
                }
                self.cursor = self.input.len();
                self.set_status("upload complete — press Enter to send");
            }
            Err(e) => {
                let text = format!("upload failed: {e}");
                if let Some(net) = self.networks.get_mut(msg.net) {
                    if let Some(b) = net.buffers.get_mut(msg.buf) {
                        b.push(Line::system(text.clone()));
                    }
                }
                self.set_status(text);
            }
        }
    }
}
