use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Success,
    Failed,
    Missing,
    /// The process died while the script was running
    Interrupted,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Success => "success",
            Status::Failed => "failed",
            Status::Missing => "missing",
            Status::Interrupted => "interrupted",
        }
    }
}

#[derive(Debug)]
pub struct Execution {
    pub id: i64,
    pub version: String,
    pub script_path: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub duration_ms: Option<i64>,
    /// SHA-256 of the script that ran (empty for runs recorded before 0.2)
    pub script_sha256: Option<String>,
}

/// Resolves the sqlite database path, honoring XDG_DATA_HOME and falling
/// back to ~/.local/share. The astroarch config dir itself is a symlinked
/// folder and unsuitable for holding a database, so this is kept separate.
pub fn default_db_path() -> Result<PathBuf> {
    let data_dir = if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        PathBuf::from(xdg)
    } else {
        let home = std::env::var("HOME").context("HOME is not set")?;
        Path::new(&home).join(".local").join("share")
    };
    Ok(data_dir.join("astroarch-updates").join("astroarch.db"))
}

pub fn open(db_path: &Path) -> Result<Connection> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let conn = Connection::open(db_path)
        .with_context(|| format!("failed to open database at {}", db_path.display()))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS executions (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            version      TEXT NOT NULL,
            script_path  TEXT NOT NULL,
            status       TEXT NOT NULL,
            exit_code    INTEGER,
            started_at   TEXT NOT NULL,
            finished_at  TEXT,
            duration_ms  INTEGER,
            script_sha256 TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_executions_version ON executions(version);",
    )?;
    // Databases created before the checksums have no script_sha256 column
    let has_sha: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('executions') WHERE name = 'script_sha256'",
        [],
        |row| row.get(0),
    )?;
    if !has_sha {
        conn.execute("ALTER TABLE executions ADD COLUMN script_sha256 TEXT", [])?;
    }
    Ok(conn)
}

/// Marks the runs left in `running` by a process that died as `interrupted`.
/// Only for the commands that are about to run scripts: while `status` reads,
/// another process may really be running one.
pub fn recover_interrupted(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE executions SET status = ?1 WHERE status = ?2",
        (Status::Interrupted.as_str(), Status::Running.as_str()),
    )?)
}

pub fn start_execution(
    conn: &Connection,
    version: &str,
    script_path: &str,
    sha256: &str,
) -> Result<i64> {
    let now = now_rfc3339()?;
    conn.execute(
        "INSERT INTO executions (version, script_path, status, started_at, script_sha256)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        (version, script_path, Status::Running.as_str(), &now, sha256),
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn finish_execution(
    conn: &Connection,
    id: i64,
    status: Status,
    exit_code: Option<i32>,
    started_at: &str,
) -> Result<()> {
    let now = now_rfc3339()?;
    let duration_ms = OffsetDateTime::parse(&now, &time::format_description::well_known::Rfc3339)
        .ok()
        .zip(
            OffsetDateTime::parse(started_at, &time::format_description::well_known::Rfc3339)
                .ok(),
        )
        .map(|(end, start)| (end - start).whole_milliseconds() as i64);

    conn.execute(
        "UPDATE executions
         SET status = ?1, exit_code = ?2, finished_at = ?3, duration_ms = ?4
         WHERE id = ?5",
        (
            status.as_str(),
            exit_code,
            &now,
            duration_ms,
            id,
        ),
    )?;
    Ok(())
}

pub fn record_missing(conn: &Connection, version: &str, script_path: &str) -> Result<()> {
    let now = now_rfc3339()?;
    conn.execute(
        "INSERT INTO executions (version, script_path, status, exit_code, started_at, finished_at, duration_ms)
         VALUES (?1, ?2, ?3, NULL, ?4, ?4, 0)",
        (version, script_path, Status::Missing.as_str(), &now),
    )?;
    Ok(())
}

pub fn last_success(conn: &Connection, version: &str) -> Result<Option<Execution>> {
    let mut stmt = conn.prepare(
        "SELECT id, version, script_path, status, exit_code, started_at, finished_at, duration_ms, script_sha256
         FROM executions
         WHERE version = ?1 AND status = 'success'
         ORDER BY id DESC
         LIMIT 1",
    )?;
    let mut rows = stmt.query([version])?;
    if let Some(row) = rows.next()? {
        Ok(Some(row_to_execution(row)?))
    } else {
        Ok(None)
    }
}

/// The latest run of VERSION, whatever its outcome (`missing` records left
/// aside: a script absent then may exist now).
pub fn latest(conn: &Connection, version: &str) -> Result<Option<Execution>> {
    let mut stmt = conn.prepare(
        "SELECT id, version, script_path, status, exit_code, started_at, finished_at, duration_ms, script_sha256
         FROM executions
         WHERE version = ?1 AND status != 'missing'
         ORDER BY id DESC
         LIMIT 1",
    )?;
    let mut rows = stmt.query([version])?;
    if let Some(row) = rows.next()? {
        Ok(Some(row_to_execution(row)?))
    } else {
        Ok(None)
    }
}

pub fn list_executions(conn: &Connection, version: Option<&str>, limit: u32) -> Result<Vec<Execution>> {
    let mut out = Vec::new();
    if let Some(version) = version {
        let mut stmt = conn.prepare(
            "SELECT id, version, script_path, status, exit_code, started_at, finished_at, duration_ms, script_sha256
             FROM executions WHERE version = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let mut rows = stmt.query((version, limit))?;
        while let Some(row) = rows.next()? {
            out.push(row_to_execution(row)?);
        }
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, version, script_path, status, exit_code, started_at, finished_at, duration_ms, script_sha256
             FROM executions ORDER BY id DESC LIMIT ?1",
        )?;
        let mut rows = stmt.query([limit])?;
        while let Some(row) = rows.next()? {
            out.push(row_to_execution(row)?);
        }
    }
    Ok(out)
}

fn row_to_execution(row: &rusqlite::Row) -> Result<Execution> {
    Ok(Execution {
        id: row.get(0)?,
        version: row.get(1)?,
        script_path: row.get(2)?,
        status: row.get(3)?,
        exit_code: row.get(4)?,
        started_at: row.get(5)?,
        finished_at: row.get(6)?,
        duration_ms: row.get(7)?,
        script_sha256: row.get(8)?,
    })
}

fn now_rfc3339() -> Result<String> {
    Ok(OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)?)
}
