mod file;

use crate::file::{Command, run_query, run_record};
use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser as _;
use etcetera::app_strategy::{AppStrategy as _, AppStrategyArgs, Xdg};
use jiff::Timestamp;
use rusqlite::{Connection, params};
use std::{
    fs,
    process::{self},
};

#[derive(clap::Parser)]
#[command(disable_help_subcommand = true)]
struct Args {
    #[command(subcommand)]
    command: Command,

    /// Path to database
    #[arg(long, env = "REACH_DB", value_name = "FILE")]
    db: Option<Utf8PathBuf>,

    /// Path to Git repo
    #[arg(long, env = "REACH_REPO", value_name = "DIRECTORY")]
    repo: Option<Utf8PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let db_path = match args.db {
        Some(db_path) => db_path,
        None => db_path()?,
    };

    let sqlite = sqlite_open(&db_path)?;

    let repo = match args.repo {
        Some(repo) => repo,
        None => repo()?,
    };

    match args.command {
        Command::Record(args) => run_record(&sqlite, &repo, args)?,
        Command::Query(args) => run_query(&sqlite, &repo, args)?,
    }

    sqlite_finish(&sqlite)?;

    Ok(())
}

fn db_path() -> anyhow::Result<Utf8PathBuf> {
    let xdg = Xdg::new(AppStrategyArgs {
        top_level_domain: String::from("com"),
        author: String::from("Evan Relf"),
        app_name: String::from("Reach"),
    })?;
    let state_dir = Utf8PathBuf::try_from(xdg.state_dir().unwrap())?;
    fs::create_dir_all(&state_dir)?;
    Ok(state_dir.join("state.sqlite3"))
}

fn sqlite_open(path: &Utf8Path) -> anyhow::Result<Connection> {
    let mut sqlite = Connection::open(path)?;

    sqlite.execute_batch(
        "
        pragma journal_mode = wal;
        pragma synchronous = normal;
        ",
    )?;

    sqlite_migrate(&mut sqlite)?;

    Ok(sqlite)
}

fn sqlite_migrate(sqlite: &mut Connection) -> anyhow::Result<()> {
    // SQLite's 12-step generalized `alter table` procedure:
    // https://www.sqlite.org/lang_altertable.html#otheralter

    const LATEST_VERSION: u16 = 2;

    loop {
        let user_version: u16 = sqlite.query_row("pragma user_version", [], |row| row.get(0))?;

        match user_version {
            0 => sqlite_migrate_0(sqlite)?,
            1 => sqlite_migrate_1(sqlite)?,
            LATEST_VERSION => break,
            _ => anyhow::bail!(
                "Database version {user_version} is newer than supported (max: {LATEST_VERSION})"
            ),
        }
    }

    Ok(())
}

// Initialize database
fn sqlite_migrate_0(sqlite: &mut Connection) -> anyhow::Result<()> {
    let tx = sqlite.transaction()?;

    let user_version: u16 = tx.query_row("pragma user_version;", [], |row| row.get(0))?;

    assert_eq!(user_version, 0);

    tx.execute(
        "
        create table if not exists file_events (
            repo text not null,
            path text not null,
            session text not null,
            event text not null,
            cwd text not null,
            time text not null
        ) strict;
        ",
        [],
    )?;

    tx.execute(
        "create index if not exists reach_repo_time_path on file_events (repo, time, path);",
        [],
    )?;

    tx.execute(&format!("pragma user_version = {};", user_version + 1), [])?;

    tx.commit()?;

    Ok(())
}

// Rewrite timestamps with fixed precision so they sort correctly as strings
fn sqlite_migrate_1(sqlite: &mut Connection) -> anyhow::Result<()> {
    let tx = sqlite.transaction()?;

    let user_version: u16 = tx.query_row("pragma user_version;", [], |row| row.get(0))?;

    assert_eq!(user_version, 1);

    let rows = tx
        .prepare("select rowid, time from file_events")?
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    {
        let mut stmt = tx.prepare("update file_events set time = ?2 where rowid = ?1")?;
        for (rowid, time) in rows {
            let time: Timestamp = time.parse()?;
            stmt.execute(params![rowid, sql_timestamp(&time)])?;
        }
    }

    tx.execute(&format!("pragma user_version = {};", user_version + 1), [])?;

    tx.commit()?;

    Ok(())
}

fn sqlite_finish(sqlite: &Connection) -> anyhow::Result<()> {
    sqlite.execute(
        "
        pragma optimize;
        ",
        [],
    )?;

    Ok(())
}

// Fixed nanosecond precision, so lexicographic order matches chronological order
fn sql_timestamp(time: &Timestamp) -> String {
    format!("{time:.9}")
}

fn repo() -> anyhow::Result<Utf8PathBuf> {
    let output = process::Command::new("git")
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()?;

    if !output.status.success() {
        anyhow::bail!("Failed to get Git repo");
    }

    let repo = Utf8PathBuf::from(str::from_utf8(&output.stdout)?.trim());

    Ok(repo)
}
