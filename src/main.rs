mod db;
mod version;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use sha2::{Digest, Sha256};

use db::Status;
use version::Version;

/// Looks up, runs and tracks AstroArch version-migration scripts.
#[derive(Parser)]
#[command(name = "astroarch-updates", version)]
struct Cli {
    /// Path to the sqlite database (defaults to ~/.local/share/astroarch-updates/astroarch.db)
    #[arg(long, global = true)]
    db_path: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Scan the scripts directory, and run every version that hasn't
    /// completed successfully yet, in ascending version order. This is
    /// the single entry point `update-astroarch` should call.
    Update {
        /// Directory containing versioned scripts (defaults to ~/.astroarch/scripts/migrations)
        #[arg(long)]
        scripts_dir: Option<PathBuf>,
    },
    /// Look up scripts/migrations/<version>.sh and run it directly, recording the outcome.
    Run {
        /// The target AstroArch version (matches scripts/migrations/<version>.sh)
        version: String,
        /// Directory containing versioned scripts (defaults to ~/.astroarch/scripts/migrations)
        #[arg(long)]
        scripts_dir: Option<PathBuf>,
        /// Run even if this version already completed successfully before
        #[arg(long)]
        force: bool,
    },
    /// Show, without changing anything, the state of every migration script:
    /// applied, pending, to retry (its last run failed or was interrupted),
    /// or changed since it was applied.
    Check {
        /// Directory containing versioned scripts (defaults to ~/.astroarch/scripts/migrations)
        #[arg(long)]
        scripts_dir: Option<PathBuf>,
        /// Print it as JSON, for programs (the Control Center's Update page)
        #[arg(long)]
        json: bool,
    },
    /// Show past executions, most recent first.
    Status {
        /// Only show executions for this version
        version: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let db_path = match cli.db_path {
        Some(path) => path,
        None => db::default_db_path()?,
    };
    let conn = db::open(&db_path)?;

    match cli.command {
        Commands::Update { scripts_dir } => {
            db::recover_interrupted(&conn)?;
            update(&conn, scripts_dir)
        }
        Commands::Run {
            version,
            scripts_dir,
            force,
        } => {
            db::recover_interrupted(&conn)?;
            run(&conn, &version, scripts_dir, force)
        }
        Commands::Check { scripts_dir, json } => check(&conn, scripts_dir, json),
        Commands::Status { version, limit } => show_status(&conn, version.as_deref(), limit),
    }
}

fn default_scripts_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    // Only the migrations: scripts/ also holds tools (create_ap.sh, resize_partition.sh...)
    // that must never run as one
    Ok(PathBuf::from(home).join(".astroarch").join("scripts").join("migrations"))
}

/// A migration script found on disk, named `<version>.sh`.
struct DiscoveredScript {
    version: Version,
    path: PathBuf,
}

/// Lists `<scripts_dir>/*.sh`, parses each file stem as a version, and
/// returns them sorted in ascending version order.
fn discover_scripts(scripts_dir: &Path) -> Result<Vec<DiscoveredScript>> {
    let mut scripts = Vec::new();
    let entries = std::fs::read_dir(scripts_dir)
        .with_context(|| format!("failed to read {}", scripts_dir.display()))?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("sh") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        scripts.push(DiscoveredScript {
            version: Version::parse(stem),
            path,
        });
    }

    scripts.sort_by(|a, b| a.version.cmp(&b.version));
    Ok(scripts)
}

/// Discovers every migration script, and runs each one that hasn't
/// completed successfully yet, in ascending version order. Stops at the
/// first failure so the caller can roll back.
fn update(conn: &rusqlite::Connection, scripts_dir: Option<PathBuf>) -> Result<()> {
    let scripts_dir = match scripts_dir {
        Some(dir) => dir,
        None => default_scripts_dir()?,
    };

    let scripts = discover_scripts(&scripts_dir)?;
    let pending: Vec<_> = {
        let mut pending = Vec::new();
        for script in scripts {
            match db::last_success(conn, &script.version.raw)? {
                None => pending.push(script),
                Some(prev) => {
                    // Applied already: not run again, but say so if the script was
                    // edited since (runs recorded before the checksums have none)
                    if let Some(old) = prev.script_sha256.as_deref() {
                        if old != sha256_file(&script.path)? {
                            println!(
                                "version {} changed since it was applied (execution #{}); \
                                 it is not run again, use `run {} --force` to re-run it",
                                script.version.raw, prev.id, script.version.raw
                            );
                        }
                    }
                }
            }
        }
        pending
    };

    if pending.is_empty() {
        println!("no pending migrations in {}", scripts_dir.display());
        return Ok(());
    }

    for script in &pending {
        run_script(conn, &script.version.raw, &script.path)?;
    }

    Ok(())
}

/// Where a migration script stands, for `check`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Applied, and the script is the one that ran
    Applied,
    /// Never run
    Pending,
    /// Its last run failed or was interrupted: update runs it again
    Retry,
    /// Applied, but the script was edited since: update does not run it again
    Changed,
}

impl State {
    fn key(self) -> &'static str {
        match self {
            State::Applied => "applied",
            State::Pending => "pending",
            State::Retry => "retry",
            State::Changed => "changed",
        }
    }
}

fn state_of(conn: &rusqlite::Connection, version: &str, path: &Path) -> Result<State> {
    if let Some(applied) = db::last_success(conn, version)? {
        let changed = match applied.script_sha256.as_deref() {
            Some(old) => old != sha256_file(path)?,
            None => false,
        };
        return Ok(if changed { State::Changed } else { State::Applied });
    }
    Ok(match db::latest(conn, version)? {
        None => State::Pending,
        Some(_) => State::Retry,
    })
}

/// One line of `check --json`. The shape is the `versions` part of the
/// report of the full update tool (Stéphane's astroarch-updates 0.5), so
/// other parts (repository, packages) can be added beside it later.
#[derive(serde::Serialize)]
struct VersionInfo {
    version: String,
    /// applied | pending | retry | changed
    state: &'static str,
}

#[derive(serde::Serialize)]
struct Report {
    versions: Vec<VersionInfo>,
}

fn check(conn: &rusqlite::Connection, scripts_dir: Option<PathBuf>, json: bool) -> Result<()> {
    let scripts_dir = match scripts_dir {
        Some(dir) => dir,
        None => default_scripts_dir()?,
    };
    let mut versions = Vec::new();
    for script in discover_scripts(&scripts_dir)? {
        let state = state_of(conn, &script.version.raw, &script.path)?;
        versions.push(VersionInfo { version: script.version.raw, state: state.key() });
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&Report { versions })?);
        return Ok(());
    }
    let applied = versions.iter().filter(|v| v.state == "applied").count();
    for v in versions.iter().filter(|v| v.state != "applied") {
        println!("{:<12} {}", v.version, v.state);
    }
    println!("{applied} version(s) applied, in {}", scripts_dir.display());
    Ok(())
}

fn run(
    conn: &rusqlite::Connection,
    version: &str,
    scripts_dir: Option<PathBuf>,
    force: bool,
) -> Result<()> {
    let scripts_dir = match scripts_dir {
        Some(dir) => dir,
        None => default_scripts_dir()?,
    };
    let script_path = scripts_dir.join(format!("{version}.sh"));

    if !force {
        if let Some(prev) = db::last_success(conn, version)? {
            println!(
                "version {version} already applied successfully at {} (execution #{}), skipping. Use --force to re-run.",
                prev.finished_at.unwrap_or_default(),
                prev.id
            );
            return Ok(());
        }
    }

    if !script_path.is_file() {
        db::record_missing(conn, version, &script_path.to_string_lossy())?;
        bail!("no migration script found at {}", script_path.display());
    }

    run_script(conn, version, &script_path)
}

/// Runs a single migration script and records its outcome. Bails with the
/// script's exit code folded into the error if it failed.
fn run_script(conn: &rusqlite::Connection, version: &str, script_path: &Path) -> Result<()> {
    let script_path_str = script_path.to_string_lossy().to_string();
    let sha256 = sha256_file(script_path)?;
    let id = db::start_execution(conn, version, &script_path_str, &sha256)?;
    let started_at = executions_started_at(conn, id)?;

    println!("running {}", script_path.display());
    let status = Command::new("zsh")
        .arg(script_path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to spawn zsh for {}", script_path.display()))?;

    let exit_code = status.code();
    let outcome = if status.success() {
        Status::Success
    } else {
        Status::Failed
    };
    db::finish_execution(conn, id, outcome, exit_code, &started_at)?;

    if !status.success() {
        bail!(
            "script for version {version} failed with exit code {:?}",
            exit_code
        );
    }

    println!("version {version} applied successfully");
    Ok(())
}

/// SHA-256 of a script, to notice one that was edited after it was applied.
fn sha256_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn executions_started_at(conn: &rusqlite::Connection, id: i64) -> Result<String> {
    let started_at: String = conn.query_row(
        "SELECT started_at FROM executions WHERE id = ?1",
        [id],
        |row| row.get(0),
    )?;
    Ok(started_at)
}

fn show_status(conn: &rusqlite::Connection, version: Option<&str>, limit: u32) -> Result<()> {
    let executions = db::list_executions(conn, version, limit)?;
    if executions.is_empty() {
        println!("no recorded executions");
        return Ok(());
    }
    println!(
        "{:<5} {:<12} {:<11} {:<8} {:<25} {:<12} {}",
        "id", "version", "status", "exit", "started_at", "duration_ms", "script_path"
    );
    for e in executions {
        println!(
            "{:<5} {:<12} {:<11} {:<8} {:<25} {:<12} {}",
            e.id,
            e.version,
            e.status,
            e.exit_code.map(|c| c.to_string()).unwrap_or_default(),
            e.started_at,
            e.duration_ms.map(|d| d.to_string()).unwrap_or_default(),
            e.script_path,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_is_recorded_and_old_databases_get_the_column() {
        let dir = std::env::temp_dir().join(format!("astroarch-updates-sha-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("astroarch.db");
        // A database as 0.1.0 created it, with one run
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE executions (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, version TEXT NOT NULL,
                    script_path TEXT NOT NULL, status TEXT NOT NULL, exit_code INTEGER,
                    started_at TEXT NOT NULL, finished_at TEXT, duration_ms INTEGER);
                 INSERT INTO executions (version, script_path, status, exit_code, started_at)
                 VALUES ('2.0.9', '/x/2.0.9.sh', 'success', 0, '2026-11-01T00:00:00Z');",
            )
            .unwrap();
        }
        let conn = db::open(&db_path).unwrap();
        let old = db::last_success(&conn, "2.0.9").unwrap().unwrap();
        assert_eq!(old.script_sha256, None);

        let script = dir.join("3.0.0.sh");
        std::fs::write(&script, "true\n").unwrap();
        let sha = sha256_file(&script).unwrap();
        let id = db::start_execution(&conn, "3.0.0", "/x/3.0.0.sh", &sha).unwrap();
        let started = executions_started_at(&conn, id).unwrap();
        db::finish_execution(&conn, id, Status::Success, Some(0), &started).unwrap();
        let new = db::last_success(&conn, "3.0.0").unwrap().unwrap();
        assert_eq!(new.script_sha256.as_deref(), Some(sha.as_str()));
        assert_eq!(sha.len(), 64);

        // A run left in `running` by a dead process
        db::start_execution(&conn, "3.0.1", "/x/3.0.1.sh", &sha).unwrap();
        assert_eq!(db::recover_interrupted(&conn).unwrap(), 1);
        let rows = db::list_executions(&conn, Some("3.0.1"), 1).unwrap();
        assert_eq!(rows[0].status, "interrupted");
        assert!(db::last_success(&conn, "3.0.1").unwrap().is_none());

        // check: each state (3.0.0.sh is the script that ran above)
        std::fs::write(dir.join("3.1.0.sh"), "true\n").unwrap();
        let state = |v: &str| state_of(&conn, v, &dir.join(format!("{v}.sh"))).unwrap();
        assert_eq!(state("3.0.0"), State::Applied);
        assert_eq!(state("3.1.0"), State::Pending);
        std::fs::write(dir.join("3.0.1.sh"), "true\n").unwrap();
        assert_eq!(state("3.0.1"), State::Retry); // interrupted above
        std::fs::write(dir.join("3.0.0.sh"), "echo edited\n").unwrap();
        assert_eq!(state("3.0.0"), State::Changed);
        check(&conn, Some(dir.clone()), true).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
