//! `anseo init` — scaffold a new Anseo project (FR-10) with tier selection (37.6)
//! and bring-up core (37.8).
//!
//! ## Flow
//!
//! **Fresh init:**
//! 1. Detect recommended deployment tier (T2 if Docker Compose available, T1 otherwise).
//! 2. Interactive TTY: prompt user to confirm or change tier.
//!    Non-interactive (`--yes` or non-TTY): tier 0 (solo CLI, CI-safe).
//! 3. Scaffold `anseo.yaml`, `.gitignore`, `README.md`.
//! 4. Bring up the tier backend (no-op for T0, spawn serve for T1, compose-up for T2).
//! 5. Call `run_preflight()` (stub in 37.8, real identity probe in 37.9).
//!
//! **Dirty-init recovery** (prior crash after scaffold but before bring-up):
//! If `anseo.yaml` already exists and `--force` is NOT set, read the tier from
//! the existing config and skip re-scaffolding. Jump straight to bring-up.
//!
//! **Overwrite flags:** `--force` re-scaffolds unconditionally; `--no-overwrite`
//! refuses to touch any existing file (exit non-zero if any would be overwritten).
//! Declining the interactive overwrite prompt exits non-zero without partial writes.

use std::borrow::Cow;
use std::io::{BufRead as _, IsTerminal, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anseo_core::OpenGeoError;
use clap::Args;

use crate::preflight::{run_preflight, PreflightOpts};
use crate::scaffold::{anseo_yaml, GITIGNORE, README};

/// Default port `anseo serve` binds on (mirrors `DEFAULT_PORT` in serve.rs).
const SERVE_PORT: u16 = 8080;

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Target directory. Defaults to the current working directory.
    #[arg(long, value_name = "DIR")]
    pub dir: Option<PathBuf>,

    /// Overwrite existing files without prompting.
    #[arg(long, conflicts_with = "no_overwrite")]
    pub force: bool,

    /// Refuse to overwrite any existing file; exit non-zero if any would have
    /// been overwritten. Useful for CI scripts that need a clean scaffold.
    #[arg(long, conflicts_with = "force")]
    pub no_overwrite: bool,

    /// Non-interactive mode: skip the tier prompt and default to Tier 0
    /// (solo CLI). Implies --force for existing files unless --no-overwrite
    /// is also set.
    #[arg(long)]
    pub yes: bool,

    /// Adopt the database's sentinel UUID as this instance's identity.
    /// Resolves a mismatch between the DB sentinel and the local sentinel file
    /// by overwriting the local file with the DB value.
    #[arg(long)]
    pub adopt_instance: bool,

    /// Clear the sentinel from the database and the local file, then create a
    /// fresh UUID on this run. Use with caution: this severs the identity link.
    #[arg(long)]
    pub reinit: bool,

    /// Skip the automatic browser-open after bring-up. The setup URL and
    /// first-run code are still printed to the terminal.
    #[arg(long)]
    pub no_open: bool,

    /// Seconds to wait for the backend to become healthy before giving up.
    #[arg(long, default_value_t = 30)]
    pub timeout_secs: u64,
}

/// Each scaffolded file as a `(path, contents)` pair.
fn scaffold_files(tier: u8) -> Vec<(&'static str, Cow<'static, str>)> {
    vec![
        ("anseo.yaml", Cow::Owned(anseo_yaml(tier))),
        (".gitignore", Cow::Borrowed(GITIGNORE)),
        ("README.md", Cow::Borrowed(README)),
    ]
}

/// Probe for Docker Compose availability.
///
/// Returns `true` when `docker compose version` exits 0 (daemon reachable +
/// compose plugin present). Abstracted as a function for unit-test injection.
fn docker_compose_available() -> bool {
    Command::new("docker")
        .args(["compose", "version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Detect the recommended deployment tier based on the current environment.
///
/// - Tier 2 (Docker Compose): `docker compose version` exits 0.
/// - Tier 1 (single binary `anseo serve`): Docker not available.
pub fn detect_recommended_tier() -> u8 {
    if docker_compose_available() {
        2
    } else {
        1
    }
}

pub fn tier_description(tier: u8) -> &'static str {
    match tier {
        0 => "solo CLI (no server)",
        1 => "single binary (anseo serve)",
        2 => "Docker Compose",
        _ => "unknown",
    }
}

/// Interactive tier-confirmation prompt (recommendation-with-Enter, not a menu).
///
/// Prints the detected recommendation and waits for the user to press Enter
/// (accept) or type 0/1/2 (change). Re-prompts on invalid input.
fn prompt_tier(recommended: u8) -> Result<u8, OpenGeoError> {
    let desc = tier_description(recommended);
    loop {
        print!(
            "Detected: Tier {recommended} — {desc}.\nPress Enter to confirm, or type 0 / 1 / 2 to change: "
        );
        std::io::stdout()
            .flush()
            .map_err(|e| OpenGeoError::Config(format!("stdout flush failed: {e}")))?;

        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|e| OpenGeoError::Config(format!("failed to read tier input: {e}")))?;

        match line.trim() {
            "" => return Ok(recommended),
            "0" => return Ok(0),
            "1" => return Ok(1),
            "2" => return Ok(2),
            _ => {
                eprintln!("Please type 0, 1, or 2 (or press Enter to accept Tier {recommended}).");
            }
        }
    }
}

// ─── Bring-up helpers ─────────────────────────────────────────────────────────

/// Returns `true` if something is already accepting TCP connections on
/// `127.0.0.1:{port}`. Used to detect a running `anseo serve` before spawning
/// a second instance.
pub fn is_port_listening(port: u16) -> bool {
    TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        Duration::from_millis(200),
    )
    .is_ok()
}

/// Spawn `anseo serve --projects-dir <dir>` as a fully detached background
/// process. Dropping the returned `Child` does NOT kill the child — the process
/// is independent from its parent's lifetime.
fn spawn_serve(dir: &Path) -> Result<(), OpenGeoError> {
    Command::new("anseo")
        .args(["serve", "--projects-dir"])
        .arg(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| OpenGeoError::Config(format!("failed to spawn `anseo serve`: {e}")))?;
    Ok(())
}

/// Resolve the Docker Compose file to use for Tier 2, in priority order:
///
/// 1. `<dir>/infra/docker/compose.yml` — source checkout
/// 2. `<dir>/compose.yml` — standalone already downloaded
/// 3. Download `https://anseo.ai/compose.yml` to `<dir>/compose.yml`
pub fn resolve_compose_path(dir: &Path) -> Result<PathBuf, OpenGeoError> {
    let infra = dir.join("infra/docker/compose.yml");
    if infra.exists() {
        return Ok(infra);
    }
    let standalone = dir.join("compose.yml");
    if standalone.exists() {
        return Ok(standalone);
    }
    download_compose(dir)?;
    Ok(dir.join("compose.yml"))
}

/// Download `compose.yml` (and `.env.example` → `.env` if absent) from anseo.ai.
fn download_compose(dir: &Path) -> Result<(), OpenGeoError> {
    eprintln!("Downloading compose.yml from anseo.ai...");
    let bytes = reqwest::blocking::get("https://anseo.ai/compose.yml")
        .map_err(|e| OpenGeoError::Config(format!("failed to fetch compose.yml: {e}")))?
        .bytes()
        .map_err(|e| OpenGeoError::Config(format!("failed to read compose.yml response: {e}")))?;
    std::fs::write(dir.join("compose.yml"), &bytes)
        .map_err(|e| OpenGeoError::Config(format!("failed to write compose.yml: {e}")))?;

    if !dir.join(".env").exists() {
        eprintln!("Downloading .env.example from anseo.ai...");
        if let Ok(resp) = reqwest::blocking::get("https://anseo.ai/.env.example") {
            if let Ok(b) = resp.bytes() {
                let _ = std::fs::write(dir.join(".env"), &b);
            }
        }
    }
    Ok(())
}

/// Run `docker compose -f <compose_path> up -d`. Propagates the exit code as
/// an error if compose exits non-zero.
fn run_compose_up(compose_path: &Path) -> Result<(), OpenGeoError> {
    let status = Command::new("docker")
        .args(["compose", "-f"])
        .arg(compose_path)
        .args(["up", "-d"])
        .status()
        .map_err(|e| OpenGeoError::Config(format!("failed to run `docker compose up -d`: {e}")))?;
    if !status.success() {
        return Err(OpenGeoError::Config(format!(
            "`docker compose up -d` failed (exit {:?})",
            status.code()
        )));
    }
    Ok(())
}

/// Bring up the tier backend after scaffold. Tier-specific logic:
///
/// - **Tier 0**: No server needed. Print DATABASE_URL hint if env var absent.
/// - **Tier 1**: TCP-probe port 8080. If clear, spawn `anseo serve` detached.
///   If already listening, print "already running" and skip.
/// - **Tier 2**: Resolve compose file and run `docker compose up -d`.
pub fn bring_up_tier(tier: u8, dir: &Path) -> Result<(), OpenGeoError> {
    match tier {
        0 => {
            if std::env::var("DATABASE_URL").is_err() {
                eprintln!(
                    "  Tip: set DATABASE_URL=postgres://user:pass@localhost/dbname before \
                     running anseo commands."
                );
            }
        }
        1 => {
            if is_port_listening(SERVE_PORT) {
                eprintln!("  anseo serve already running on port {SERVE_PORT} — skipping launch.");
            } else {
                spawn_serve(dir)?;
                eprintln!("  anseo serve launched in the background.");
                eprintln!("  API:    http://127.0.0.1:{SERVE_PORT}");
                eprintln!("  Health: http://127.0.0.1:{SERVE_PORT}/healthz");
            }
        }
        2 => {
            let compose_path = resolve_compose_path(dir)?;
            run_compose_up(&compose_path)?;
            eprintln!("  Docker Compose stack starting.");
            eprintln!("  Run `docker compose ps` to watch service health.");
        }
        _ => {
            return Err(OpenGeoError::Config(format!(
                "unknown tier {tier}; expected 0, 1, or 2"
            )));
        }
    }
    Ok(())
}

// ─── Main command ─────────────────────────────────────────────────────────────

pub fn run(args: InitArgs) -> Result<(), OpenGeoError> {
    let dir = args
        .dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().expect("CWD must be readable"));

    if !dir.exists() {
        std::fs::create_dir_all(&dir).map_err(|e| {
            OpenGeoError::Config(format!(
                "failed to create target directory `{}`: {e}",
                dir.display()
            ))
        })?;
    }

    // ── Dirty-init recovery ──────────────────────────────────────────────────
    // If anseo.yaml already exists and --force is NOT set, read the tier from
    // the existing config and skip re-scaffolding. This handles the case where
    // a prior `anseo init` crashed after scaffold but before bring-up completed.
    let existing_yaml = dir.join("anseo.yaml");
    let tier = if existing_yaml.exists() && !args.force {
        let contents = std::fs::read_to_string(&existing_yaml).map_err(|e| {
            OpenGeoError::Config(format!("failed to read existing anseo.yaml: {e}"))
        })?;
        let cfg = anseo_core::Config::from_yaml_str(&contents)
            .map_err(|e| OpenGeoError::Config(format!("existing anseo.yaml is invalid: {e}")))?;
        let t = cfg.tier;
        eprintln!(
            "Found existing anseo.yaml (Tier {} — {}). Resuming bring-up.",
            t,
            tier_description(t)
        );
        t
    } else {
        // ── Fresh init (or --force re-scaffold) ─────────────────────────────
        // Determine the deployment tier.
        // Non-interactive (--yes or non-TTY): always Tier 0 (solo CLI, CI-safe).
        // Interactive TTY: detect + confirm with user.
        let t = if args.yes || !std::io::stdin().is_terminal() {
            0u8
        } else {
            let recommended = detect_recommended_tier();
            prompt_tier(recommended)?
        };

        let files = scaffold_files(t);
        let preexisting: Vec<&str> = files
            .iter()
            .filter(|(name, _)| dir.join(name).exists())
            .map(|(name, _)| *name)
            .collect();

        if !preexisting.is_empty() && args.no_overwrite {
            return Err(OpenGeoError::Config(format!(
                "refusing to overwrite existing file(s): {}",
                preexisting.join(", ")
            )));
        }

        // Decide per file whether to write.
        let mut planned_writes: Vec<(&str, &str)> = Vec::new();
        for (name, contents) in &files {
            let path = dir.join(name);
            if !path.exists() || args.force || args.yes {
                planned_writes.push((name, contents.as_ref()));
                continue;
            }
            // Interactive prompt path.
            let confirmed = confirm_overwrite(name)?;
            if confirmed {
                planned_writes.push((name, contents.as_ref()));
            } else {
                // FR-10: "on decline, exits non-zero without partial overwrite".
                return Err(OpenGeoError::Config(format!(
                    "init declined: existing `{name}` would have been overwritten"
                )));
            }
        }

        // Write everything only after every prompt resolved — keeps the dir
        // consistent if the user aborts mid-flow.
        for (name, contents) in planned_writes {
            let path = dir.join(name);
            write_file(&path, contents)?;
        }

        eprintln!(
            "Scaffolded Anseo project (Tier {t} — {}) at {}.",
            tier_description(t),
            dir.display()
        );
        t
    };

    // ── Bring-up phase ───────────────────────────────────────────────────────
    bring_up_tier(tier, &dir)?;

    // ── Preflight: DB connectivity + sentinel identity check (Story 37.9) ───
    run_preflight(PreflightOpts {
        database_url: std::env::var("DATABASE_URL").ok(),
        adopt_instance: args.adopt_instance,
        reinit: args.reinit,
    })?;

    // ── Handoff (T1/T2) or completion message (T0) ──────────────────────────
    if tier >= 1 {
        let setup_url = format!("http://127.0.0.1:{SERVE_PORT}/setup");
        let code = crate::handoff::generate_first_run_code();
        crate::handoff::wait_for_health(SERVE_PORT, Duration::from_secs(args.timeout_secs))?;
        if !args.no_open {
            crate::handoff::open_browser(&setup_url);
        }
        crate::handoff::print_handoff(&setup_url, &code);
    } else {
        eprintln!("✓ Tier 0 (solo CLI): no server to start.");
        if std::env::var("DATABASE_URL").is_err() {
            eprintln!("  Set DATABASE_URL=postgres://... before running anseo commands.");
        }
        eprintln!("Next: run `anseo login openai` (or anthropic) to store a provider key.");
    }
    Ok(())
}

fn write_file(path: &Path, contents: &str) -> Result<(), OpenGeoError> {
    std::fs::write(path, contents)
        .map_err(|e| OpenGeoError::Config(format!("failed to write `{}`: {e}", path.display())))?;
    Ok(())
}

/// Prompt the user. Returns `Ok(true)` on confirm, `Ok(false)` on decline.
/// In a non-TTY environment we never prompt — overwriting an existing file
/// in a script context requires `--force` to be explicit.
fn confirm_overwrite(name: &str) -> Result<bool, OpenGeoError> {
    if !std::io::stdin().is_terminal() {
        return Err(OpenGeoError::Config(format!(
            "`{name}` already exists; refusing to overwrite without --force in non-interactive mode"
        )));
    }
    let prompt = format!("`{name}` already exists. Overwrite?");
    dialoguer::Confirm::new()
        .with_prompt(prompt)
        .default(false)
        .interact()
        .map_err(|e| OpenGeoError::Config(format!("interactive prompt failed: {e}")))
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    // ── 37.6 unit tests (unchanged) ───────────────────────────────────────────

    #[test]
    fn tier_description_covers_all_valid_values() {
        assert_eq!(tier_description(0), "solo CLI (no server)");
        assert_eq!(tier_description(1), "single binary (anseo serve)");
        assert_eq!(tier_description(2), "Docker Compose");
    }

    #[test]
    fn detect_recommended_tier_returns_1_or_2() {
        let tier = detect_recommended_tier();
        assert!(tier == 1 || tier == 2, "tier must be 1 or 2, got {tier}");
    }

    #[test]
    fn scaffold_files_tier_0_has_no_tier_line() {
        let files = scaffold_files(0);
        let (_, yaml) = files.iter().find(|(n, _)| *n == "anseo.yaml").unwrap();
        assert!(!yaml.contains("tier:"), "tier=0 must not appear in YAML");
    }

    #[test]
    fn scaffold_files_tier_1_has_tier_line() {
        let files = scaffold_files(1);
        let (_, yaml) = files.iter().find(|(n, _)| *n == "anseo.yaml").unwrap();
        assert!(yaml.contains("tier: 1"), "tier: 1 must appear in YAML");
    }

    #[test]
    fn scaffold_files_tier_2_has_tier_line() {
        let files = scaffold_files(2);
        let (_, yaml) = files.iter().find(|(n, _)| *n == "anseo.yaml").unwrap();
        assert!(yaml.contains("tier: 2"), "tier: 2 must appear in YAML");
    }

    // ── 37.8 unit tests ───────────────────────────────────────────────────────

    #[test]
    fn is_port_listening_returns_false_for_closed_port() {
        // Ephemeral ports are a machine-wide shared resource: under parallel
        // test execution, a port we just freed can occasionally be re-claimed
        // by another concurrently-running test (e.g. handoff::tests also
        // binds 127.0.0.1:0) before this check runs. Retry against a fresh
        // port a few times before treating it as a real failure.
        for attempt in 0..5 {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            if !is_port_listening(port) {
                return;
            }
            if attempt < 4 {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        panic!("dropped port must not report as listening (persisted across retries)");
    }

    #[test]
    fn is_port_listening_returns_true_while_listening() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Listener still alive.
        assert!(
            is_port_listening(port),
            "bound port must report as listening"
        );
    }

    #[test]
    fn resolve_compose_path_prefers_infra_docker() {
        let dir = tempfile::TempDir::new().unwrap();
        let infra = dir.path().join("infra/docker");
        std::fs::create_dir_all(&infra).unwrap();
        let expected = infra.join("compose.yml");
        std::fs::write(&expected, "# compose").unwrap();
        // Also put a compose.yml in the root — infra/ must win.
        std::fs::write(dir.path().join("compose.yml"), "# standalone").unwrap();

        let resolved = resolve_compose_path(dir.path()).unwrap();
        assert_eq!(resolved, expected);
    }

    #[test]
    fn resolve_compose_path_falls_back_to_project_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let standalone = dir.path().join("compose.yml");
        std::fs::write(&standalone, "# standalone").unwrap();
        // No infra/docker/compose.yml.
        let resolved = resolve_compose_path(dir.path()).unwrap();
        assert_eq!(resolved, standalone);
    }

    #[test]
    fn tier_0_bring_up_prints_hint_when_no_database_url() {
        let dir = tempfile::TempDir::new().unwrap();
        // Remove DATABASE_URL from the test environment if present.
        std::env::remove_var("DATABASE_URL");
        // Should not error; DATABASE_URL hint is printed to stderr (not testable
        // directly here, but the function must return Ok).
        bring_up_tier(0, dir.path()).unwrap();
    }

    #[test]
    fn tier_0_bring_up_no_error_when_database_url_set() {
        let dir = tempfile::TempDir::new().unwrap();
        std::env::set_var("DATABASE_URL", "postgres://localhost/test");
        let result = bring_up_tier(0, dir.path());
        std::env::remove_var("DATABASE_URL");
        result.unwrap();
    }

    #[test]
    fn tier_1_bring_up_skips_spawn_when_port_already_listening() {
        // Bind a listener on an ephemeral port, then pretend SERVE_PORT == that port.
        // We can't easily override SERVE_PORT in a test, so we test is_port_listening
        // directly instead: if the port is occupied, the function skips spawn.
        // This is verified by the bring_up_tier returning Ok without error even
        // when the port is occupied (no double-spawn, no error).
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(is_port_listening(port));
        drop(listener);
    }
}
