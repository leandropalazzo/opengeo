//! T1/T2 handoff: health-wait, browser-open, print URL + first-run code (Story 37.10).
//!
//! Story 37.11 replaces `generate_first_run_code()` with a real API-minted token.

use std::process::Command;
use std::time::{Duration, Instant};

use anseo_core::OpenGeoError;

/// Poll `http://127.0.0.1:{port}/healthz` every 500 ms until it returns 200
/// or `timeout` elapses.
pub fn wait_for_health(port: u16, timeout: Duration) -> Result<(), OpenGeoError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(400))
        .build()
        .map_err(|e| OpenGeoError::Config(format!("handoff: failed to build HTTP client: {e}")))?;
    let url = format!("http://127.0.0.1:{port}/healthz");
    let deadline = Instant::now() + timeout;
    eprintln!("  Waiting for service health at {url}...");
    loop {
        if let Ok(resp) = client.get(&url).send() {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err(OpenGeoError::Config(format!(
                "timed out waiting for anseo serve to become healthy \
                 (checked {url} for {} s)",
                timeout.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Attempt to open `url` in the default browser using the platform-appropriate
/// command. Errors are silently ignored — terminal output is the fallback.
pub fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = Command::new("cmd").args(["/C", "start", "", url]).spawn();
    // Unsupported platforms: silently skip.
}

/// Print the handoff URL and first-run code unconditionally to stderr.
/// Called even when auto-open succeeds, so the terminal always has a receipt.
pub fn print_handoff(url: &str, code: &str) {
    eprintln!("  URL:  {url}");
    eprintln!("  Code: {code}");
}

/// Stub first-run code. Story 37.11 replaces this with a real API-minted
/// single-use session token.
pub fn generate_first_run_code() -> String {
    "SETUP".to_string()
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_for_health_times_out_when_nothing_listening() {
        // Port 1 is almost always closed (requires root); 1 s timeout keeps test fast.
        let result = wait_for_health(1, Duration::from_secs(1));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("timed out"), "error message: {msg}");
    }

    #[test]
    fn wait_for_health_succeeds_when_server_responds() {
        let port = spawn_mock_health_server();
        // Give the thread a moment to bind and start accepting.
        std::thread::sleep(Duration::from_millis(5));
        let result = wait_for_health(port, Duration::from_secs(3));
        assert!(result.is_ok(), "health check should succeed: {result:?}");
    }

    #[test]
    fn generate_first_run_code_returns_nonempty_string() {
        let code = generate_first_run_code();
        assert!(!code.is_empty());
    }

    #[test]
    fn open_browser_does_not_panic_on_invalid_url() {
        // Should silently ignore errors, not panic.
        open_browser("not-a-real-url");
    }

    // Spin up a minimal HTTP/1.1 server that returns 200 OK for any request.
    fn spawn_mock_health_server() -> u16 {
        use std::io::Write;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut s in listener.incoming().take(10).flatten() {
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
            }
        });
        port
    }
}
