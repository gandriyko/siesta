//! `siesta run` / `siesta exec`: pick statements, resolve the connection, confirm, execute, print.

use std::io::{IsTerminal, Read, Write};
use std::time::Duration;
use std::path::{Path, PathBuf};

use clap::Args;
use db_conn::{Connection, Connections, Driver};
use db_exec::{DbError, Page, Session};
use serde_json::Value;
use siesta_settings::{ConfirmDestructive, Settings};
use sql_split::{Dialect, Kind, Severity, SqlFile};

use crate::output::{Format, Item, YELLOW, item_json, item_text, paint, print_sql_error, put};
use crate::{CliError, runtime};

/// Default row limit of the CLI (the GUI pages; an agent rarely wants more).
const DEFAULT_MAX_ROWS: usize = 100;

/// `-c`; shared by `run`, `exec` and `schema`.
#[derive(Args, Clone, Default)]
pub struct ConnArgs {
    /// Connection name (overrides `-- @connection`)
    #[arg(short = 'c', long)]
    pub connection: Option<String>,
    /// Database on that server (overrides `-- @database` and the connection's default)
    #[arg(short = 'd', long)]
    pub database: Option<String>,
}

#[derive(Args, Clone, Default)]
pub struct ExecOpts {
    #[command(flatten)]
    pub conn: ConnArgs,
    #[arg(long, value_enum)]
    pub output: Option<Format>,
    /// Rows to return per statement [default: 100]; 0 = the `results.max_rows` cap
    #[arg(long, value_name = "N")]
    pub max_rows: Option<usize>,
    /// Skip confirmations: every statement on a `prod` connection, `-- @confirm`, and destructive
    /// statements when `run.confirm_destructive` asks
    #[arg(long)]
    pub yes: bool,
    /// Force the connection read-only: a safety default of the session (the server rejects writes,
    /// siesta refuses switching to read-write), not a security boundary; use a read-only DB user
    /// for real protection
    #[arg(long)]
    pub read_only: bool,
}

#[derive(Args)]
pub struct RunArgs {
    /// `.sql` file
    pub file: PathBuf,
    /// `@name`, `:LINE` or `#N`; none = every statement in file order
    pub statements: Vec<String>,
    #[command(flatten)]
    pub opts: ExecOpts,
}

#[derive(Args)]
pub struct ExecArgs {
    /// SQL text (one or several statements); read from stdin when omitted or `-`
    pub sql: Option<String>,
    #[command(flatten)]
    pub opts: ExecOpts,
}

/// Settings and connections (both from the config dir).
pub(crate) struct Env {
    pub settings: Settings,
    pub conns: Connections,
}

impl Env {
    pub fn load() -> Env {
        Self::load_in(siesta_settings::config_dir().as_deref())
    }

    /// Like `load`, with the connections read from the config dir `config`.
    pub fn load_in(config: Option<&Path>) -> Env {
        let loaded = siesta_settings::Loaded::load();
        for e in &loaded.errors {
            eprintln!("{} {e}", paint("warning:", YELLOW, true));
        }
        if let (Some(config), Some(keys)) = (config, siesta_settings::data_dir()) {
            Self::migrate_names(config, &keys);
        }
        let conns = config.map(Connections::load).unwrap_or_default();
        for w in &conns.warnings {
            eprintln!("{} {w}", paint("warning:", YELLOW, true));
        }
        Env { settings: loaded.settings, conns }
    }

    /// Old connections whose names break the naming rules are renamed (and their `-- @connection`
    /// lines rewritten on disk); what happened goes to stderr, never stdout.
    fn migrate_names(config: &Path, keys: &Path) {
        let (renames, warnings) = db_conn::migrate_names(config, keys);
        for w in &warnings {
            eprintln!("{} {w}", paint("warning:", YELLOW, true));
        }
        if renames.is_empty() {
            return;
        }
        let (files, errors) = siesta_workspace::rewrite_connection_refs(&siesta_settings::collections_dir_in(config), siesta_settings::data_dir().as_deref(), &renames);
        let list: Vec<String> = renames.iter().map(|(o, n)| format!("{o} -> {n}")).collect();
        eprintln!("siesta: connections renamed to the new name rules: {}; updated `-- @connection` in {files} files", list.join(", "));
        for e in &errors {
            eprintln!("{} {e}", paint("warning:", YELLOW, true));
        }
    }

    pub fn pick(&self, name: &str) -> Result<Connection, CliError> {
        if let Some(c) = self.conns.connections.get(name) {
            return Ok(c.clone());
        }
        if let Some(e) = self.conns.errors.get(name) {
            return Err(CliError::new("invalid_connection", format!("connection '{name}': {e}"), 3));
        }
        let known = self.conns.names();
        let hint = if known.is_empty() { "no connections yet: create one in the Siesta app, or see `siesta connections`".to_string() } else { format!("available: {}", known.join(", ")) };
        Err(CliError::new("unknown_connection", format!("no connection '{name}'"), 3).hint(hint))
    }

    /// Flag, else the file's `-- @connection`, else an error.
    pub fn resolve(&self, flag: Option<&str>, directive: Option<&str>) -> Result<Connection, CliError> {
        match flag.or(directive) {
            Some(n) => self.pick(n),
            None => Err(CliError::new("no_connection", "no connection chosen", 3).hint("pass -c <name> or add `-- @connection <name>` at the top of the file")),
        }
    }
}

pub(crate) fn dialect_of(d: Driver) -> Dialect {
    match d {
        Driver::Postgres => Dialect::Postgres,
        Driver::MySql => Dialect::MySql,
    }
}

/// A statement ready to run.
pub(crate) struct Job {
    pub name: Option<String>,
    pub line: usize,
    pub kind: Kind,
    pub text: String,
    pub confirm: bool,
}

pub(crate) fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Query => "query",
        Kind::Read => "read",
        Kind::Other => "other",
    }
}

pub(crate) fn line_of(src: &str, offset: usize) -> usize {
    src[..offset.min(src.len())].matches('\n').count() + 1
}

/// Parse errors (unclosed string / comment / `$$`) are fatal, warnings go to stderr.
pub(crate) fn check_parse(src: &str, file: &SqlFile, path: Option<&Path>) -> Result<(), CliError> {
    for d in &file.diagnostics {
        let line = line_of(src, d.range.start);
        let at = |e: CliError| match path {
            Some(p) => e.at(p, Some(line)),
            None => e.at_line(line),
        };
        match d.severity {
            Severity::Error => return Err(at(CliError::new("parse_error", d.message.clone(), 3))),
            Severity::Warning => eprintln!("{} {}line {line}: {}", paint("warning:", YELLOW, true), path.map(|p| format!("{}: ", p.display())).unwrap_or_default(), d.message),
        }
    }
    Ok(())
}

/// Index of a statement: `@name` (the `@` is optional), `:LINE` (any line; a line between
/// statements belongs to the previous one, like the cursor in the editor) or `#N` (1-based).
pub(crate) fn find(src: &str, file: &SqlFile, addr: &str, path: &Path) -> Result<usize, CliError> {
    let sts = &file.statements;
    let found = if let Some(l) = addr.strip_prefix(':') {
        let lines = src.split('\n').count();
        l.parse::<usize>().ok().filter(|l| (1..=lines).contains(l)).filter(|_| !sts.is_empty()).map(|l| {
            let offset: usize = src.split_inclusive('\n').take(l - 1).map(str::len).sum();
            sts.partition_point(|s| s.range.start <= offset).saturating_sub(1)
        })
    } else if let Some(n) = addr.strip_prefix('#') {
        n.parse::<usize>().ok().filter(|n| (1..=sts.len()).contains(n)).map(|n| n - 1)
    } else {
        let name = addr.strip_prefix('@').unwrap_or(addr);
        sts.iter().position(|s| s.name.as_deref() == Some(name))
    };
    found.ok_or_else(|| {
        let names: Vec<&str> = sts.iter().filter_map(|s| s.name.as_deref()).collect();
        CliError::new("statement_not_found", format!("no statement '{addr}'"), 3)
            .at(path, None)
            .hint(format!("use a name ({}), :LINE or #N (1..={})", names.join(", "), sts.len()))
    })
}

/// Exit code of a failed statement: 2 when the connection is gone, 4 when siesta refused it
/// (the `read_only` guard), 5 for an SQL error.
pub(crate) fn sql_exit(e: &DbError) -> u8 {
    if e.connection_lost {
        2
    } else if e.refused {
        4
    } else {
        5
    }
}

/// The exit code of a batch is the first error's (later errors, with `stop_on_error` off, keep it).
fn first_exit(so_far: u8, e: &DbError) -> u8 {
    if so_far == 0 { sql_exit(e) } else { so_far }
}

fn ask(question: &str) -> bool {
    eprint!("{question} [y/N] ");
    let mut s = String::new();
    std::io::stdin().read_line(&mut s).is_ok() && matches!(s.trim(), "y" | "Y" | "yes")
}

/// Refuses (exit 4) or, in a terminal, asks, for every statement that needs confirmation.
/// Runs before anything executes, so a refusal never leaves a half-run batch.
fn confirm(env: &Env, conn: &Connection, jobs: &[Job], file: Option<&Path>, yes: bool, agent_on_prod: bool) -> Result<(), CliError> {
    let policy = env.settings.run.confirm_destructive;
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    for j in jobs {
        // `prod` connections confirm every non-read statement; the policy only matters elsewhere
        // (`prod` in the policy means that same flag, so it adds nothing for the others).
        let on_prod = conn.prod && !agent_on_prod && j.kind.needs_prod_confirm();
        let reason = if on_prod {
            Some("production connection")
        } else if j.confirm {
            Some("marked -- @confirm")
        } else if policy == ConfirmDestructive::Always {
            sql_split::destructive_reason(&j.text, dialect_of(conn.driver))
        } else {
            None
        };
        let Some(reason) = reason else { continue };
        if yes && on_prod && !interactive {
            // `--yes` cannot answer for a person: whoever is not at a terminal (an agent's shell, a
            // script) never gets a non-read statement through on prod; run it in a terminal.
            let err = CliError::new("prod_needs_terminal", format!("statement at line {} on '{}' changes a production connection: --yes is not accepted without a terminal", j.line, conn.name), 4);
            let err = match file {
                Some(p) => err.at(p, Some(j.line)),
                None => err.at_line(j.line),
            };
            return Err(err.hint("ask the user to run it in a terminal, where it asks [y/N]"));
        }
        if yes {
            continue;
        }
        let first = j.text.lines().next().unwrap_or("");
        if interactive && ask(&format!("{} on '{}' ({reason}): {first}\nRun?", paint("confirm", YELLOW, true), conn.name)) {
            continue;
        }
        let err = CliError::new(
            if interactive { "cancelled" } else { "confirmation_required" },
            format!("statement at line {} on '{}' needs confirmation: {reason}", j.line, conn.name),
            4,
        );
        let err = match file {
            Some(p) => err.at(p, Some(j.line)),
            None => err.at_line(j.line),
        };
        return Err(if interactive { err } else { err.hint("pass --yes to run it") });
    }
    Ok(())
}

/// Whether this process was started by `siesta mcp` (an agent, not a person at a shell).
fn agent_on_mcp() -> bool {
    std::env::var_os(crate::mcp::AGENT_ENV).is_some()
}

/// Effective row limit: the flag (default 100), never above `results.max_rows`.
pub(crate) fn row_limit(flag: Option<usize>, hard: usize) -> usize {
    let hard = hard.max(1);
    match flag {
        None => DEFAULT_MAX_ROWS.min(hard),
        Some(0) => hard,
        Some(n) => n.min(hard),
    }
}

/// Opens the session, runs the jobs in order and prints one result per statement.
/// JSON: one object, or an array for several statements; JSON lines: one object per line.
pub(crate) fn execute(env: &Env, mut conn: Connection, file: Option<&Path>, jobs: Vec<Job>, o: &ExecOpts, format: Format, out: &mut dyn Write) -> Result<u8, CliError> {
    if jobs.is_empty() {
        return Err(CliError::new("no_statements", "nothing to run", 3));
    }
    conn.read_only |= o.read_only;
    if conn.read_only {
        // Before any confirmation and before anything runs: a refusal never leaves a half-run batch.
        for j in &jobs {
            if let Err(e) = db_exec::read_only_check(&j.text, dialect_of(conn.driver)) {
                let err = CliError::new("read_only_refused", e.message, 4);
                return Err(match file {
                    Some(p) => err.at(p, Some(j.line)),
                    None => err.at_line(j.line),
                });
            }
        }
    }
    // An agent (MCP) on prod: reads and `UPDATE … WHERE` only, checked up front like `read_only`;
    // those then need no prod confirmation (nobody could give it), `-- @confirm` still refuses.
    let agent_on_prod = conn.prod && agent_on_mcp();
    if agent_on_prod {
        for j in &jobs {
            if let Some(m) = sql_split::prod_agent_violation(&j.text, dialect_of(conn.driver)) {
                let err = CliError::new("prod_refused", m, 4).hint("ask the user to run it");
                return Err(match file {
                    Some(p) => err.at(p, Some(j.line)),
                    None => err.at_line(j.line),
                });
            }
        }
    }
    confirm(env, &conn, &jobs, file, o.yes, agent_on_prod)?;
    let cap = row_limit(o.max_rows, env.settings.results.max_rows);
    let rt = runtime();
    let mut session = rt.block_on(Session::open_with(&conn, Duration::from_millis(env.settings.run.connect_timeout_ms))).map_err(|e| CliError::new("connection_failed", format!("connection '{}': {e}", conn.name), 2))?;
    session.max_rows = cap;
    let page = Page { size: cap as u64, ..Page::default() };
    let file_name = file.map(|p| p.display().to_string());
    let (mut exit, mut items) = (0, Vec::new());
    // Every statement that ran is recorded like a GUI run, marked `cli` or `mcp`; SQL from `exec`
    // has no file. A history that cannot be written only warns.
    let source = if agent_on_mcp() { "mcp" } else { "cli" };
    let history_file = file.map(|p| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())).unwrap_or_default();
    let history = siesta_settings::data_dir().and_then(|b| db_exec::history::Store::open(&b).inspect_err(|e| eprintln!("{} history disabled: {e}", paint("warning:", YELLOW, true))).ok()).map(|mut h| {
        h.total = env.settings.history.total;
        h
    });
    // From here SIGINT / SIGTERM cancel the running statement on the server instead of killing the
    // process (the server would go on running it); the run then stops. A second signal exits at once.
    let mut signals = {
        use tokio::signal::unix::{SignalKind, signal};
        let _rt = rt.enter();
        signal(SignalKind::interrupt()).ok().zip(signal(SignalKind::terminate()).ok())
    };
    let cancel = session.cancel_handle();
    let mut stopped = false;
    for (n, j) in jobs.iter().enumerate() {
        // Shown as running in the app's History until this iteration ends (after `record`).
        let _running = history.as_ref().and_then(|h| {
            let entry = db_exec::history::Entry { source: source.into(), line: j.line, connection: conn.name.clone(), database: conn.database.clone(), sql: j.text.clone(), ..Default::default() };
            h.begin(&history_file, entry).inspect_err(|e| eprintln!("{} history: {e}", paint("warning:", YELLOW, true))).ok()
        });
        let result = rt.block_on(async {
            let run = session.run(&j.text, j.kind, &page);
            tokio::pin!(run);
            tokio::select! {
                r = &mut run => return r,
                _ = stop_signal(&mut signals) => stopped = true,
            }
            // Repeated: a cancel that reaches the server before the statement does is a no-op.
            loop {
                let _ = cancel.cancel().await;
                tokio::select! {
                    r = &mut run => return r,
                    _ = stop_signal(&mut signals) => std::process::exit(130),
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }
        });
        let item = Item { file: file_name.as_deref(), name: j.name.as_deref(), line: j.line, connection: &conn.name, database: conn.database.as_deref(), result: &result, max_rows: cap };
        if let Err(e) = &result {
            exit = first_exit(exit, e);
        }
        if let Some(h) = &history {
            let entry = db_exec::history::Entry {
                source: source.into(),
                line: j.line,
                connection: conn.name.clone(),
                database: conn.database.clone(),
                sql: j.text.clone(),
                duration_ms: result.as_ref().map_or(0, |o| o.duration.as_millis() as u64),
                rows: result.as_ref().ok().filter(|o| o.rows_affected.is_none()).map(|o| o.rows.len() as u64),
                affected: result.as_ref().ok().and_then(|o| o.rows_affected),
                error: result.as_ref().err().map(|e| e.to_string()),
                ..Default::default()
            };
            if let Err(e) = h.record(&history_file, entry) {
                eprintln!("{} history: {e}", paint("warning:", YELLOW, true));
            }
        }
        match format {
            Format::Table | Format::Csv => {
                let heading = (jobs.len() > 1).then(|| format!("-- {} (line {})", j.name.as_deref().unwrap_or_else(|| j.text.lines().next().unwrap_or("")), j.line));
                if n > 0 {
                    put(out, "");
                }
                item_text(&item, format, heading, out);
                if let Err(e) = &result {
                    print_sql_error(e);
                }
            }
            Format::Jsonl => put(out, item_json(&item).to_string()),
            Format::Json => items.push(item_json(&item)),
        }
        if stopped || (result.is_err() && env.settings.run.stop_on_error) {
            break;
        }
    }
    if format == Format::Json {
        let v = match <[Value; 1]>::try_from(items) {
            Ok([one]) => one,
            Err(many) => Value::Array(many),
        };
        put(out, v.to_string());
    }
    rt.block_on(session.close());
    Ok(exit)
}

/// SIGINT or SIGTERM; never, if the handlers could not be installed.
async fn stop_signal(s: &mut Option<(tokio::signal::unix::Signal, tokio::signal::unix::Signal)>) {
    match s {
        Some((int, term)) => tokio::select! { _ = int.recv() => {}, _ = term.recv() => {} },
        None => std::future::pending().await,
    }
}

/// `siesta run`.
pub(crate) fn run(a: RunArgs, out: &mut dyn Write) -> Result<u8, CliError> {
    let format = Format::resolve(a.opts.output);
    let src = std::fs::read_to_string(&a.file).map_err(|e| CliError::new("file_not_found", e.to_string(), 3).at(&a.file, None))?;
    let env = Env::load();
    // The directive is the same in both dialects; the real dialect needs the connection.
    let top = sql_split::split(&src, Dialect::Postgres);
    let conn = env.resolve(a.opts.conn.connection.as_deref(), top.connection.as_deref()).map_err(|e| e.at(&a.file, None))?;
    // Flag, else `-- @database`, else the connection's own default.
    let conn = conn.with_database(a.opts.conn.database.as_deref().or(top.database.as_deref()));
    let file = sql_split::split(&src, dialect_of(conn.driver));
    check_parse(&src, &file, Some(&a.file))?;
    let picked: Vec<usize> = if a.statements.is_empty() {
        (0..file.statements.len()).collect()
    } else {
        a.statements.iter().map(|s| find(&src, &file, s, &a.file)).collect::<Result<_, _>>()?
    };
    if picked.is_empty() {
        return Err(CliError::new("no_statements", "no statements in file", 3).at(&a.file, None));
    }
    let jobs = picked.into_iter().map(|i| job(&src, &file, i)).collect();
    execute(&env, conn, Some(&a.file), jobs, &a.opts, format, out)
}

fn job(src: &str, file: &SqlFile, i: usize) -> Job {
    let st = &file.statements[i];
    Job { name: st.name.clone(), line: st.line, kind: st.kind, text: st.text(src).to_string(), confirm: st.confirm }
}

/// `siesta exec`.
pub(crate) fn exec(a: ExecArgs, out: &mut dyn Write) -> Result<u8, CliError> {
    let format = Format::resolve(a.opts.output);
    let sql = match a.sql.as_deref() {
        Some(s) if s != "-" => s.to_string(),
        _ if std::io::stdin().is_terminal() => return Err(CliError::new("no_sql", "no SQL given", 3).hint("pass it as an argument or on stdin")),
        _ => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s).map_err(|e| CliError::new("io_error", e.to_string(), 3))?;
            s
        }
    };
    let env = Env::load();
    let conn = env.resolve(a.opts.conn.connection.as_deref(), None)?.with_database(a.opts.conn.database.as_deref());
    let file = sql_split::split(&sql, dialect_of(conn.driver));
    check_parse(&sql, &file, None)?;
    let jobs = (0..file.statements.len()).map(|i| job(&sql, &file, i)).collect();
    execute(&env, conn, None, jobs, &a.opts, format, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PG: Dialect = Dialect::Postgres;

    #[test]
    fn statement_addresses() {
        let src = "-- @name one\nSELECT 1;\n\n-- @name two\nSELECT 2\n  FROM t;\n\nSELECT 3;\n";
        let file = sql_split::split(src, PG);
        let p = Path::new("a.sql");
        let at = |a: &str| find(src, &file, a, p).ok();
        assert_eq!([at("one"), at("@two"), at("#3"), at("#1")], [Some(0), Some(1), Some(2), Some(0)]);
        // :LINE: any line of the statement, a blank line belongs to the previous one.
        assert_eq!([at(":2"), at(":3"), at(":5"), at(":6"), at(":7"), at(":8"), at(":1")], [Some(0), Some(0), Some(1), Some(1), Some(1), Some(2), Some(0)]);
        assert_eq!([at("#0"), at("#4"), at(":0"), at(":99"), at("nope"), at(":x")], [None; 6]);
        let e = find(src, &file, "nope", p).unwrap_err();
        assert_eq!(e.exit, 3);
        assert!(e.hint.unwrap().contains("one, two"));
    }

    #[test]
    fn exit_codes_of_sql_errors() {
        assert_eq!(sql_exit(&DbError { code: Some("42P01".into()), ..Default::default() }), 5);
        assert_eq!(sql_exit(&DbError { message: "decode".into(), ..Default::default() }), 5);
        assert_eq!(sql_exit(&DbError { connection_lost: true, ..Default::default() }), 2);
        assert_eq!(sql_exit(&DbError { refused: true, ..Default::default() }), 4);
    }

    #[test]
    fn batch_exit_code_is_the_first_errors() {
        let sql = DbError { code: Some("42P01".into()), ..Default::default() };
        let lost = DbError { connection_lost: true, ..Default::default() };
        // stop_on_error = false: a later, "bigger" code must not replace the first one.
        assert_eq!(first_exit(first_exit(0, &sql), &lost), 5);
        assert_eq!(first_exit(first_exit(0, &lost), &sql), 2);
        assert_eq!(first_exit(0, &sql), 5);
    }

    #[test]
    fn row_limits() {
        assert_eq!([row_limit(None, 10_000), row_limit(None, 50), row_limit(Some(0), 500), row_limit(Some(7), 500), row_limit(Some(9999), 500)], [100, 50, 500, 7, 500]);
    }

    fn env() -> Env {
        Env { settings: Settings::default(), conns: Connections::default() }
    }

    fn conn(name: &str, read_only: bool, prod: bool) -> Connection {
        let dir = std::env::temp_dir().join(format!("siesta-cli-unit-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(dir.join("connections")).unwrap();
        std::fs::write(dir.join(format!("connections/{name}.json")), format!(r#"{{"driver":"postgres","host":"h","prod":{prod}}}"#)).unwrap();
        let mut c = Connections::load(&dir).connections.remove(name).unwrap();
        c.read_only = read_only;
        std::fs::remove_dir_all(&dir).ok();
        c
    }

    fn job_of(sql: &str, confirm: bool) -> Job {
        Job { name: None, line: 1, kind: Kind::Other, text: sql.into(), confirm }
    }

    #[test]
    fn confirmation_rules() {
        // Not a terminal in tests: confirmation means refusal (exit 4) unless --yes.
        let refused = |e: &Env, c: &Connection, j: Job, yes| confirm(e, c, &[j], None, yes, false).err().map(|e| e.exit);
        let e = env();
        let prod = conn("prod-db", false, true);
        let dev = conn("dev-db", false, false);
        let ro = conn("dev-ro", true, false);
        // A prod connection confirms every non-read statement; reads (Query / Read) run freely.
        assert!(prod.prod && !dev.prod);
        let read = |sql: &str, kind| Job { kind, ..job_of(sql, false) };
        assert_eq!(refused(&e, &prod, read("SELECT 1", Kind::Query), false), None);
        assert_eq!(refused(&e, &prod, read("SHOW x", Kind::Read), false), None);
        assert_eq!(refused(&e, &prod, job_of("SELECT 1", true), false), Some(4)); // `-- @confirm` still asks
        assert_eq!(refused(&e, &prod, job_of("DELETE FROM t WHERE id = 1", false), false), Some(4));
        // `--yes` is not accepted on prod without a terminal (tests have none); elsewhere it is.
        assert_eq!(refused(&e, &prod, job_of("DELETE FROM t", false), true), Some(4));
        // A read marked `-- @confirm` is not a prod change: `--yes` answers it.
        assert_eq!(refused(&e, &prod, Job { confirm: true, ..read("SELECT 1", Kind::Query) }, true), None);
        // Neither a read-only nor a plain connection does, by default.
        assert_eq!(refused(&e, &ro, job_of("DROP TABLE t", false), false), None);
        assert_eq!(refused(&e, &dev, job_of("DROP TABLE t", false), false), None);
        assert_eq!(refused(&e, &dev, job_of("SELECT 1", true), false), Some(4));
        assert_eq!(refused(&e, &dev, job_of("SELECT 1", true), true), None);
        let mut always = env();
        always.settings.run.confirm_destructive = ConfirmDestructive::Always;
        assert_eq!(refused(&always, &dev, job_of("TRUNCATE t", false), false), Some(4));
        assert_eq!(refused(&always, &dev, job_of("SELECT 1", false), false), None);
        // `never` does not switch the prod rule off.
        let mut never = env();
        never.settings.run.confirm_destructive = ConfirmDestructive::Never;
        assert_eq!(refused(&never, &prod, job_of("TRUNCATE t", false), false), Some(4));
        assert_eq!(refused(&never, &dev, job_of("TRUNCATE t", false), false), None);
    }
}
