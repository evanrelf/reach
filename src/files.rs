use crate::sql_timestamp;
use camino::{Utf8Path, Utf8PathBuf, absolute_utf8};
use jiff::Timestamp;
use parse_datetime::parse_datetime;
use pathdiff::diff_utf8_paths;
use rusqlite::{Connection, params};
use std::{
    collections::{HashMap, HashSet},
    env,
    io::{self, Read as _, Write},
    process::{self, Stdio},
    thread,
};

#[derive(clap::Subcommand)]
pub enum Command {
    /// Record path event
    Record(RecordArgs),

    /// Query recorded paths
    Query(QueryArgs),
}

#[derive(clap::Args)]
pub struct RecordArgs {
    pub path: Utf8PathBuf,

    pub event: EventKind,

    /// Group events from the same session
    #[arg(long, env = "REACH_SESSION")]
    pub session: String,

    /// Record as if occurred from a different working directory
    #[arg(long, value_name = "DIRECTORY")]
    pub cwd: Option<Utf8PathBuf>,

    /// Record as if occurred at a different time
    #[arg(long, value_parser = parse_timestamp)]
    pub time: Option<Timestamp>,
}

#[derive(clap::Args)]
pub struct QueryArgs {
    /// Print absolute paths
    #[arg(long)]
    pub absolute: bool,

    /// Include ignored paths
    #[arg(long)]
    pub no_ignore: bool,

    /// Query at a different time
    #[arg(long, value_parser = parse_timestamp)]
    pub time: Option<Timestamp>,

    #[command(subcommand)]
    pub command: QueryCommand,
}

#[derive(clap::Subcommand)]
pub enum QueryCommand {
    /// Most frequent+recently accessed
    Frecent,

    /// Most recently accessed
    Recent,

    /// Most frequently accessed
    Frequent,
}

#[derive(Clone, Copy, clap::ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum EventKind {
    Open,
    Close,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Open => "open",
            EventKind::Close => "close",
        }
    }
}

fn parse_timestamp(input: &str) -> anyhow::Result<Timestamp> {
    let Some(zoned) = parse_datetime(input)?.into_zoned() else {
        anyhow::bail!("Unsupported year in timestamp (out of range)");
    };
    Ok(zoned.timestamp())
}

pub fn run_record(sqlite: &Connection, repo: &Utf8Path, args: RecordArgs) -> anyhow::Result<()> {
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
        record(sqlite, repo, &path, &cwd, &time, event, &session)?;
    }

    Ok(())
}

pub fn run_query(sqlite: &Connection, repo: &Utf8Path, args: QueryArgs) -> anyhow::Result<()> {
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
    event: EventKind,
    session: &str,
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
            sql_timestamp(time),
            event.as_str(),
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
        order by time
        ",
    )?;

    let rows = stmt.query_map(params![repo.as_str(), sql_timestamp(time)], |row| {
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

    items.sort_by(|(path_a, a), (path_b, b)| b.total_cmp(a).then_with(|| path_a.cmp(path_b)));

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
        order by max(time) desc, path
        ",
    )?;

    let rows = stmt
        .query_map(params![repo.as_str(), sql_timestamp(time)], |row| {
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
        order by count(*) desc, path
        ",
    )?;

    let rows = stmt
        .query_map(params![repo.as_str(), sql_timestamp(time)], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let paths = rows
        .into_iter()
        .map(|string| Utf8PathBuf::from(string))
        .collect();

    Ok(paths)
}
