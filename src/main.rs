mod db;
mod version;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

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
        Commands::Update { scripts_dir } => update(&conn, scripts_dir),
        Commands::Run {
            version,
            scripts_dir,
            force,
        } => run(&conn, &version, scripts_dir, force),
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
            if db::last_success(conn, &script.version.raw)?.is_none() {
                pending.push(script);
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
    let id = db::start_execution(conn, version, &script_path_str)?;
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
        "{:<5} {:<12} {:<9} {:<8} {:<25} {:<12} {}",
        "id", "version", "status", "exit", "started_at", "duration_ms", "script_path"
    );
    for e in executions {
        println!(
            "{:<5} {:<12} {:<9} {:<8} {:<25} {:<12} {}",
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
