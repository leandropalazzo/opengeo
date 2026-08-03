//! Preflight identity probe — Story 37.9.
//!
//! Validates DB connectivity and sentinel UUID after `anseo init` brings the
//! tier backend up. Skipped when DATABASE_URL is not set (Tier 1/2 manage their
//! own Postgres; the URL is not known at init time).
//!
//! Sentinel design: a single-row table `anseo_sentinel` (PRIMARY KEY fixed to
//! `'instance'`) holds a UUID that binds this DB to this local installation.
//! A local copy at `~/.local/share/anseo/sentinel.uuid` is compared on every
//! init run; mismatches hard-abort with clear instructions.

use std::path::PathBuf;

use anseo_core::OpenGeoError;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

/// Options forwarded from `InitArgs` to the preflight check.
pub struct PreflightOpts {
    /// Value of `DATABASE_URL` env var, if set.
    pub database_url: Option<String>,
    /// `--adopt-instance`: adopt DB sentinel as local identity on mismatch.
    pub adopt_instance: bool,
    /// `--reinit`: clear sentinel from DB + local file, then start fresh.
    pub reinit: bool,
}

/// Run pre-handoff sanity checks after the tier backend is started.
///
/// Returns immediately when `opts.database_url` is `None` (no DB to check).
pub fn run_preflight(opts: PreflightOpts) -> Result<(), OpenGeoError> {
    let url = match opts.database_url {
        Some(ref u) => u.clone(),
        None => return Ok(()),
    };

    // main() is a plain sync fn — no active tokio runtime. Create a
    // lightweight single-thread runtime just for the async DB calls.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| OpenGeoError::Config(format!("preflight: runtime init failed: {e}")))?;

    rt.block_on(check_sentinel(&url, opts.adopt_instance, opts.reinit))
}

// ── Sentinel file helpers ─────────────────────────────────────────────────────

fn sentinel_dir() -> Result<PathBuf, OpenGeoError> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        let p = PathBuf::from(xdg);
        if p.is_absolute() {
            return Ok(p.join("anseo"));
        }
    }
    let home = std::env::var("HOME")
        .map_err(|_| OpenGeoError::Config("HOME is not set; cannot locate sentinel file".into()))?;
    Ok(PathBuf::from(home).join(".local/share/anseo"))
}

fn sentinel_path() -> Result<PathBuf, OpenGeoError> {
    Ok(sentinel_dir()?.join("sentinel.uuid"))
}

fn read_local_sentinel() -> Result<Option<Uuid>, OpenGeoError> {
    let path = sentinel_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| OpenGeoError::Config(format!("preflight: cannot read sentinel file: {e}")))?;
    let uuid = Uuid::parse_str(raw.trim()).map_err(|e| {
        OpenGeoError::Config(format!("preflight: sentinel file has invalid UUID: {e}"))
    })?;
    Ok(Some(uuid))
}

fn write_local_sentinel(uuid: Uuid) -> Result<(), OpenGeoError> {
    let dir = sentinel_dir()?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| OpenGeoError::Config(format!("preflight: cannot create sentinel dir: {e}")))?;
    std::fs::write(dir.join("sentinel.uuid"), uuid.to_string())
        .map_err(|e| OpenGeoError::Config(format!("preflight: cannot write sentinel file: {e}")))?;
    Ok(())
}

fn delete_local_sentinel() -> Result<(), OpenGeoError> {
    let path = sentinel_path()?;
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| {
            OpenGeoError::Config(format!("preflight: cannot delete sentinel file: {e}"))
        })?;
    }
    Ok(())
}

// ── DB sentinel logic ─────────────────────────────────────────────────────────

async fn check_sentinel(url: &str, adopt: bool, reinit: bool) -> Result<(), OpenGeoError> {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(url)
        .await
        .map_err(|e| OpenGeoError::Config(format!("preflight: cannot connect to database: {e}")))?;

    // Print DB identity for operator visibility.
    let (db_name, db_user): (String, String) =
        sqlx::query_as("SELECT current_database()::text, current_user::text")
            .fetch_one(&pool)
            .await
            .map_err(|e| {
                OpenGeoError::Config(format!("preflight: DB identity query failed: {e}"))
            })?;
    eprintln!("  DB: {db_name} (user: {db_user})");

    // Ensure sentinel table exists (idempotent bootstrap DDL).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS anseo_sentinel (\
            id            TEXT        PRIMARY KEY DEFAULT 'instance',\
            instance_uuid UUID        NOT NULL,\
            created_at    TIMESTAMPTZ NOT NULL DEFAULT now()\
        )",
    )
    .execute(&pool)
    .await
    .map_err(|e| OpenGeoError::Config(format!("preflight: cannot create sentinel table: {e}")))?;

    // --reinit: clear both sides, then fall through to first-run creation.
    if reinit {
        sqlx::query("DELETE FROM anseo_sentinel WHERE id = 'instance'")
            .execute(&pool)
            .await
            .map_err(|e| OpenGeoError::Config(format!("preflight: cannot clear sentinel: {e}")))?;
        delete_local_sentinel()?;
    }

    // Read current DB sentinel (if any).
    let db_row: Option<(Uuid,)> =
        sqlx::query_as("SELECT instance_uuid FROM anseo_sentinel WHERE id = 'instance'")
            .fetch_optional(&pool)
            .await
            .map_err(|e| OpenGeoError::Config(format!("preflight: cannot read sentinel: {e}")))?;

    let local_uuid = read_local_sentinel()?;

    match (db_row, local_uuid) {
        // First run (or post-reinit): create a fresh UUID in both places.
        (None, None) => {
            let new_uuid = Uuid::new_v4();
            sqlx::query("INSERT INTO anseo_sentinel (id, instance_uuid) VALUES ('instance', $1)")
                .bind(new_uuid)
                .execute(&pool)
                .await
                .map_err(|e| {
                    OpenGeoError::Config(format!("preflight: cannot write sentinel: {e}"))
                })?;
            write_local_sentinel(new_uuid)?;
            let verb = if reinit { "reinitialised" } else { "created" };
            eprintln!("  Sentinel: {verb} ({new_uuid})");
        }

        // DB has sentinel, local file absent: write local file (adopt from DB).
        (Some((db_uuid,)), None) => {
            write_local_sentinel(db_uuid)?;
            eprintln!("  Sentinel: adopted from DB ({db_uuid})");
        }

        // Local file exists, DB row absent (DB was wiped/migrated): restore into DB.
        (None, Some(local_uuid)) => {
            sqlx::query("INSERT INTO anseo_sentinel (id, instance_uuid) VALUES ('instance', $1)")
                .bind(local_uuid)
                .execute(&pool)
                .await
                .map_err(|e| {
                    OpenGeoError::Config(format!("preflight: cannot restore sentinel: {e}"))
                })?;
            eprintln!("  Sentinel: restored to DB ({local_uuid})");
        }

        // Both sides present: check for match.
        (Some((db_uuid,)), Some(local_uuid)) => {
            if db_uuid == local_uuid {
                eprintln!("  Sentinel: OK ({db_uuid})");
            } else if adopt {
                write_local_sentinel(db_uuid)?;
                eprintln!("  Sentinel: adopted DB value ({db_uuid})");
            } else {
                pool.close().await;
                return Err(OpenGeoError::Config(format!(
                    "preflight: DB sentinel mismatch — this database appears to belong to a \
                     different anseo instance.\n  DB:    {db_uuid}\n  Local: {local_uuid}\n\
                     Re-run with --adopt-instance to adopt the DB's identity, or \
                     --reinit to start fresh."
                )));
            }
        }
    }

    pool.close().await;
    Ok(())
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    // Env-var tests mutate XDG_DATA_HOME; serialise them so parallel test
    // threads don't race each other.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn skip_when_no_database_url() {
        let result = run_preflight(PreflightOpts {
            database_url: None,
            adopt_instance: false,
            reinit: false,
        });
        assert!(result.is_ok(), "no DATABASE_URL → must return Ok(())");
    }

    #[test]
    fn sentinel_dir_uses_xdg_when_absolute() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("XDG_DATA_HOME", tmp.path());
        let dir = sentinel_dir().unwrap();
        std::env::remove_var("XDG_DATA_HOME");
        assert_eq!(dir, tmp.path().join("anseo"));
    }

    #[test]
    fn sentinel_dir_uses_home_fallback() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::remove_var("XDG_DATA_HOME");
        let home = std::env::var("HOME").expect("HOME must be set for this test");
        let dir = sentinel_dir().unwrap();
        assert_eq!(dir, PathBuf::from(&home).join(".local/share/anseo"));
    }

    #[test]
    fn sentinel_dir_ignores_relative_xdg() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::set_var("XDG_DATA_HOME", "relative/path");
        let home = std::env::var("HOME").expect("HOME must be set for this test");
        let dir = sentinel_dir().unwrap();
        std::env::remove_var("XDG_DATA_HOME");
        assert_eq!(dir, PathBuf::from(&home).join(".local/share/anseo"));
    }

    #[test]
    fn read_write_local_sentinel_roundtrip() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("XDG_DATA_HOME", tmp.path());

        let uuid = Uuid::new_v4();
        write_local_sentinel(uuid).unwrap();
        let read_back = read_local_sentinel().unwrap();
        std::env::remove_var("XDG_DATA_HOME");
        assert_eq!(read_back, Some(uuid));
    }

    #[test]
    fn delete_local_sentinel_is_idempotent() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("XDG_DATA_HOME", tmp.path());

        // Delete when file doesn't exist → no error
        delete_local_sentinel().unwrap();

        // Write then delete
        write_local_sentinel(Uuid::new_v4()).unwrap();
        delete_local_sentinel().unwrap();
        let after = read_local_sentinel().unwrap();
        std::env::remove_var("XDG_DATA_HOME");
        assert_eq!(after, None);
    }
}
