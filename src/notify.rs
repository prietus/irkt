//! Desktop notifications. A thin wrapper over `notify-rust`, which speaks the
//! native mechanism on each OS: D-Bus (`org.freedesktop.Notifications`) on
//! Linux, NSUserNotification on macOS, and a WinRT toast on Windows.

/// Pin the application identity once at startup. Only matters on macOS, where
/// `mac-notification-sys` otherwise falls back to an AppleScript bundle-id
/// lookup that can pop an "open as script" prompt on the first notification
/// from an unsigned terminal binary. Setting it skips that path. The id need
/// not resolve to an installed `.app` for the cosmetic fallback to be avoided.
pub fn init() {
    #[cfg(target_os = "macos")]
    {
        let _ = notify_rust::set_application("com.github.prietus.irkt");
    }
}

/// Show a desktop notification. `.show()` blocks (on Linux it waits for the
/// D-Bus reply), so it runs on a detached thread — fire-and-forget. Any error
/// (no notification daemon, etc.) is swallowed; a missing toast must never
/// disrupt the client.
pub fn show(title: String, body: String) {
    std::thread::spawn(move || {
        let _ = notify_rust::Notification::new()
            .summary(&title)
            .body(&body)
            .appname("irkt")
            .show();
    });
}
