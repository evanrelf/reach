use camino::{Utf8Path, Utf8PathBuf, absolute_utf8};
use clap::Parser as _;
use etcetera::app_strategy::{AppStrategy as _, AppStrategyArgs, Xdg};
use jiff::Timestamp;
use parse_datetime::parse_datetime;
use pathdiff::diff_utf8_paths;
use rusqlite::{Connection, ToSql, params, types::ToSqlOutput};
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    env, fs,
    io::{self, Read as _, Write},
    process::{self, Stdio},
    thread,
};

#[derive(clap::Parser)]
#[command(disable_help_subcommand = true)]
struct Args {
    /// Run as if started in another Git repo instead of working directory
    #[arg(long)]
    repo: Option<Utf8PathBuf>,

    /// Path to database
    #[arg(long, env = "REACH_DB", value_name = "PATH")]
    db: Option<Utf8PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Record path access
    Record(RecordArgs),

    /// Query recorded paths
    Query(QueryArgs),
}

#[derive(clap::Args)]
struct RecordArgs {
    /// Record as if accessed from a different working directory
    #[arg(long, value_name = "PATH")]
    cwd: Option<Utf8PathBuf>,

    /// Record as if accessed at a different time
    #[arg(long, value_parser = parse_timestamp)]
    time: Option<Timestamp>,

    /// Record a specific event
    #[arg(long, requires = "session")]
    event: Option<EventKind>,

    /// Group events from the same session
    #[arg(long)]
    session: Option<String>,

    path: Utf8PathBuf,
}

#[derive(clap::Args)]
struct QueryArgs {
    /// Print absolute paths
    #[arg(long)]
    absolute: bool,

    /// Include ignored paths
    #[arg(long)]
    no_ignore: bool,

    /// Query at a different time
    #[arg(long, value_parser = parse_timestamp)]
    time: Option<Timestamp>,

    #[command(subcommand)]
    command: QueryCommand,
}

#[derive(clap::Subcommand)]
enum QueryCommand {
    /// Most frequent+recently accessed
    Frecent,

    /// Most recently accessed
    Recent,

    /// Most frequently accessed
    Frequent,
}

#[derive(Clone, Copy, clap::ValueEnum)]
#[value(rename_all = "snake_case")]
enum EventKind {
    Open,
    Close,
}

impl EventKind {
    fn as_str(self) -> &'static str {
        match self {
            EventKind::Open => "open",
            EventKind::Close => "close",
        }
    }
}

impl ToSql for EventKind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
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

fn run_record(sqlite: &Connection, repo: &Utf8Path, args: RecordArgs) -> anyhow::Result<()> {
    let RecordArgs {
        cwd,
        time,
        event,
        session,
        path,
    } = args;

    let cwd = absolute_utf8(match cwd {
        Some(cwd) => cwd,
        None => Utf8PathBuf::try_from(env::current_dir()?)?,
    })?;
    let time = time.unwrap_or_else(|| Timestamp::now());
    let path = absolute_utf8(path)?;

    // TODO: Allow recording files outside of repo? Need to exclude temporary files like
    // `*.jjdescription` and such.
    if path.starts_with(repo) {
        record(sqlite, repo, &path, &cwd, &time, event, session.as_deref())?;
    }

    Ok(())
}

fn run_query(sqlite: &Connection, repo: &Utf8Path, args: QueryArgs) -> anyhow::Result<()> {
    let QueryArgs {
        absolute,
        no_ignore,
        time,
        command,
    } = args;

    let time = time.unwrap_or_else(|| Timestamp::now());

    // Querying Git is the bottleneck, so we spawn it as early as possible
    let tracked_child = if no_ignore {
        None
    } else {
        Some(tracked_spawn(repo)?)
    };

    let paths = match command {
        QueryCommand::Frecent => frecent(sqlite, repo, &time)?,
        QueryCommand::Recent => recent(sqlite, repo, &time)?,
        QueryCommand::Frequent => frequent(sqlite, repo, &time)?,
    };

    let paths = paths
        .into_iter()
        .filter(|path| path.exists())
        .collect::<Vec<_>>();

    // Tracked files are never ignored, so only untracked files need checking
    let ignored = if let Some(tracked_child) = tracked_child {
        let tracked = tracked_wait(repo, tracked_child)?;
        let untracked = paths
            .iter()
            .filter(|path| !tracked.contains(*path))
            .cloned()
            .collect::<Vec<_>>();
        ignored(repo, &untracked)?
    } else {
        HashSet::new()
    };

    let current_dir = Utf8PathBuf::try_from(env::current_dir()?)?;

    let mut stdout = io::stdout().lock();

    for path in paths {
        if ignored.contains(&path) {
            continue;
        }
        let path = if absolute {
            path
        } else {
            diff_utf8_paths(path, &current_dir).unwrap()
        };
        if writeln!(stdout, "{path}").is_err() {
            break;
        }
    }

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

fn parse_timestamp(input: &str) -> anyhow::Result<Timestamp> {
    let zoned = parse_datetime(input)?;
    Ok(zoned.timestamp())
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

    const LATEST_VERSION: u16 = 1;

    loop {
        let user_version: u16 = sqlite.query_row("pragma user_version", [], |row| row.get(0))?;

        match user_version {
            0 => sqlite_migrate_0(sqlite)?,
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
            cwd text not null,
            time text not null,
            event text,
            session text
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

fn sqlite_finish(sqlite: &Connection) -> anyhow::Result<()> {
    sqlite.execute(
        "
        pragma optimize;
        ",
        [],
    )?;

    Ok(())
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

fn tracked_spawn(repo: &Utf8Path) -> anyhow::Result<process::Child> {
    let child = process::Command::new("git")
        .arg("ls-files")
        .arg("--cached")
        .arg("-z")
        .stdout(Stdio::piped())
        .current_dir(repo)
        .spawn()?;

    Ok(child)
}

fn tracked_wait(repo: &Utf8Path, child: process::Child) -> anyhow::Result<HashSet<Utf8PathBuf>> {
    let output = child.wait_with_output()?;

    if !output.status.success() {
        anyhow::bail!("`git ls-files` failed");
    }

    let tracked = str::from_utf8(&output.stdout)?
        .split_terminator('\0')
        .map(|path| repo.join(path))
        .collect();

    Ok(tracked)
}

fn ignored(repo: &Utf8Path, paths: &[Utf8PathBuf]) -> anyhow::Result<HashSet<Utf8PathBuf>> {
    if paths.is_empty() {
        return Ok(HashSet::new());
    }

    let mut child = process::Command::new("git")
        .arg("check-ignore")
        .arg("--stdin")
        .arg("-z")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .current_dir(repo)
        .spawn()?;

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    let (write_result, read_result) = thread::scope(|scope| {
        let writer = scope.spawn(move || -> io::Result<()> {
            for path in paths {
                stdin.write_all(path.as_str().as_bytes())?;
                stdin.write_all(b"\0")?;
            }
            Ok(())
        });
        let mut output = Vec::new();
        let read_result = stdout.read_to_end(&mut output).map(|_| output);
        drop(stdout);
        (writer.join().unwrap(), read_result)
    });

    let exit_status = child.wait()?;

    match exit_status.code() {
        Some(0 | 1) => {}
        Some(128) => anyhow::bail!("`git check-ignore` encountered a fatal error"),
        code => anyhow::bail!("`git check-ignore` returned unexpected exit code: {code:?}"),
    }

    write_result?;
    let output = read_result?;

    let ignored = str::from_utf8(&output)?
        .split_terminator('\0')
        .map(Utf8PathBuf::from)
        .collect();

    Ok(ignored)
}

fn record(
    sqlite: &Connection,
    repo: &Utf8Path,
    path: &Utf8Path,
    cwd: &Utf8Path,
    time: &Timestamp,
    event: Option<EventKind>,
    session: Option<&str>,
) -> anyhow::Result<()> {
    sqlite.execute(
        "
        insert into file_events (repo, path, cwd, time, event, session)
        values (?1, ?2, ?3, ?4, ?5, ?6)
        ",
        params![
            repo.as_str(),
            path.as_str(),
            cwd.as_str(),
            time.to_string(),
            event,
            session
        ],
    )?;

    Ok(())
}

// https://wiki.mozilla.org/User:Jesse/NewFrecency
fn frecent(
    sqlite: &Connection,
    repo: &Utf8Path,
    time: &Timestamp,
) -> anyhow::Result<Vec<Utf8PathBuf>> {
    let mut stmt = sqlite.prepare(
        "
        select
            path,
            julianday(?2) - julianday(time) as age_days
        from file_events
        where repo = ?1
          and time <= ?2
          and (event is null or event = 'open')
        ",
    )?;

    let rows = stmt.query_map(params![repo.as_str(), time.to_string()], |row| {
        let path: String = row.get(0)?;
        let age_days: f64 = row.get(1)?;
        Ok((path, age_days))
    })?;

    let half_life_days = 1.0;

    let mut scores = HashMap::new();

    for row in rows {
        let (path, age_days) = row?;
        let weight = 2f64.powf(-age_days / half_life_days);
        *scores.entry(path).or_insert(0.0) += weight;
    }

    let mut items = scores.into_iter().collect::<Vec<_>>();

    items.sort_by(|(_, a), (_, b)| b.partial_cmp(a).unwrap_or(Ordering::Equal));

    let paths = items
        .into_iter()
        .map(|(path, _)| Utf8PathBuf::from(path))
        .collect();

    Ok(paths)
}

fn recent(
    sqlite: &Connection,
    repo: &Utf8Path,
    time: &Timestamp,
) -> anyhow::Result<Vec<Utf8PathBuf>> {
    let mut stmt = sqlite.prepare(
        "
        select path
        from file_events
        where repo = ?1
          and time <= ?2
        group by path
        order by max(time) desc
        ",
    )?;

    let rows = stmt
        .query_map(params![repo.as_str(), time.to_string()], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let paths = rows
        .into_iter()
        .map(|string| Utf8PathBuf::from(string))
        .collect();

    Ok(paths)
}

fn frequent(
    sqlite: &Connection,
    repo: &Utf8Path,
    time: &Timestamp,
) -> anyhow::Result<Vec<Utf8PathBuf>> {
    let mut stmt = sqlite.prepare(
        "
        select path
        from file_events
        where repo = ?1
          and time <= ?2
          and (event is null or event = 'open')
        group by path
        order by count(*) desc
        ",
    )?;

    let rows = stmt
        .query_map(params![repo.as_str(), time.to_string()], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let paths = rows
        .into_iter()
        .map(|string| Utf8PathBuf::from(string))
        .collect();

    Ok(paths)
}
