//! `siesta <command>`: the same core as the GUI, for terminals, CI and agents.
//! Exit codes: 0 ok · 2 connection error · 3 parse error / unknown connection or statement /
//! bad input · 4 refused (confirmation needed) · 5 SQL error.

mod mcp;
mod output;
mod run;

use std::io::{BufRead, IsTerminal as _, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use db_exec::{DbError, Page, Session, Table};
use serde_json::{Value, json};
use sql_split::{Dialect, Kind};

use output::{Format, RED, emit_objects, paint, put, render_csv, render_table};
use run::{ConnArgs, Env, ExecArgs, RunArgs, dialect_of, kind_name, line_of};

#[derive(Parser)]
#[command(name = "siesta", version, about = "Lightweight SQL client: Postgres and MySQL / MariaDB")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List statements: file, line, name, first line
    List {
        /// .sql file or directory (default: current directory)
        path: Option<PathBuf>,
        #[arg(long, value_enum)]
        output: Option<Format>,
    },
    /// Run statements of a file (all in file order when none are given)
    Run(RunArgs),
    /// Run SQL from an argument or stdin
    Exec(ExecArgs),
    /// Connections of the config dir `~/.config/siesta/connections/` (secrets masked)
    Connections {
        #[command(subcommand)]
        cmd: Option<ConnCmd>,
        #[arg(long, value_enum, global = true)]
        output: Option<Format>,
    },
    /// Tables, columns and indexes of a connection (for writing correct SQL)
    Schema(SchemaArgs),
    /// MCP server on stdio for AI agents (tools: connections, schema, list_statements, run_sql;
    /// writes refused on read_only connections, on prod only reads and UPDATE … WHERE)
    Mcp,
}

#[derive(Subcommand)]
enum ConnCmd {
    /// Every connection, and whether it has a password (the default)
    List,
    /// Open a connection and report the server version
    Test {
        /// Connection name
        name: String,
    },
    /// Databases on the server of a connection that its credentials can access
    Databases {
        /// Connection name
        name: String,
    },
    /// Store a connection's password, encrypted with the local key, in its file (prompt without echo, or stdin when piped)
    SetPassword {
        /// Connection name
        name: String,
    },
}

#[derive(Args)]
struct SchemaArgs {
    /// `table` or `schema.table`; none = every table and view
    table: Option<String>,
    #[command(flatten)]
    conn: ConnArgs,
    /// Print the CREATE statement of the table instead of its columns
    #[arg(long)]
    ddl: bool,
    #[arg(long, value_enum)]
    output: Option<Format>,
}

impl Cli {
    fn format(&self) -> Format {
        Format::resolve(match &self.cmd {
            Cmd::List { output, .. } | Cmd::Connections { output, .. } => *output,
            Cmd::Run(a) => a.opts.output,
            Cmd::Exec(a) => a.opts.output,
            Cmd::Schema(a) => a.output,
            Cmd::Mcp => None,
        })
    }
}

const COMMANDS: &[&str] = &["list", "run", "exec", "connections", "schema", "mcp", "help", "--help", "-h", "--version", "-V"];

/// Whether these arguments (without the program name) are a CLI call; otherwise they mean
/// "open the GUI": nothing, `open`, a path.
pub fn is_cli(args: &[String]) -> bool {
    args.first().is_some_and(|a| COMMANDS.contains(&a.as_str()))
}

/// The GUI binary calls this first; `None` means "not a CLI command, open the GUI".
pub fn main_if_cli() -> Option<ExitCode> {
    let args: Vec<String> = std::env::args().collect();
    if !is_cli(&args[1..]) {
        return None;
    }
    // Not `stdout().lock()`: `siesta mcp` writes replies from other threads, which would wait forever.
    Some(ExitCode::from(run_with(args, &mut std::io::stdout())))
}

/// Runs the CLI with `args` (program name first), writing results to `out`. Returns the exit code.
pub fn run_with<I: IntoIterator<Item = String>>(args: I, out: &mut dyn Write) -> u8 {
    let cli = match Cli::try_parse_from(args) {
        Ok(c) => c,
        Err(e) => {
            // Help and version go to stdout with 0; usage errors are bad input.
            let _ = e.print();
            return if e.use_stderr() { 3 } else { 0 };
        }
    };
    let format = cli.format();
    match dispatch(cli, out) {
        Ok(code) => code,
        Err(e) => {
            e.print(format.human(), out);
            e.exit
        }
    }
}

fn dispatch(cli: Cli, out: &mut dyn Write) -> Result<u8, CliError> {
    let format = cli.format();
    match cli.cmd {
        Cmd::List { path, output: _ } => list(&path.unwrap_or_else(|| ".".into()), format, out),
        Cmd::Run(a) => run::run(a, out),
        Cmd::Exec(a) => run::exec(a, out),
        Cmd::Connections { cmd: None | Some(ConnCmd::List), .. } => connections(format, out),
        Cmd::Connections { cmd: Some(ConnCmd::Test { name }), .. } => test_connection(&name, format, out),
        Cmd::Connections { cmd: Some(ConnCmd::Databases { name }), .. } => databases(&name, format, out),
        Cmd::Connections { cmd: Some(ConnCmd::SetPassword { name }), .. } => {
            let stdin = std::io::stdin();
            let tty = stdin.is_terminal();
            let (config, data) = (siesta_settings::config_dir(), siesta_settings::data_dir());
            set_password(&name, config.as_deref(), data.as_deref(), tty, &mut stdin.lock(), format, out)
        }
        Cmd::Schema(a) => schema(a, format, out),
        Cmd::Mcp => Ok(mcp::serve()),
    }
}

/// `connections set-password`: the password is read from a prompt without echo (`tty`) or from `input`
/// (one line), and stored encrypted (`password_enc`) in the connection's file; the key is in `keys`.
fn set_password(name: &str, config: Option<&Path>, keys: Option<&Path>, tty: bool, input: &mut dyn BufRead, format: Format, out: &mut dyn Write) -> Result<u8, CliError> {
    Env::load_in(config).pick(name)?;
    let password = if tty {
        rpassword::prompt_password(format!("Password for '{name}': ")).map_err(|e| CliError::new("io_error", e.to_string(), 3))?
    } else {
        let mut line = String::new();
        input.read_line(&mut line).map_err(|e| CliError::new("io_error", e.to_string(), 3))?;
        line.trim_end_matches(['\n', '\r']).to_string()
    };
    if password.is_empty() {
        return Err(CliError::new("no_password", "empty password", 3));
    }
    let (Some(dir), Some(keys)) = (config, keys) else { return Err(CliError::new("io_error", "no home directory", 3)) };
    db_conn::set_password(dir, keys, name, &password).map_err(|e| CliError::new("encryption_error", e, 2))?;
    if format.human() {
        put(out, format!("Stored the encrypted password of '{name}' in {}", db_conn::connections_dir(dir).join(format!("{name}.json")).display()));
    } else {
        put(out, json!({ "connection": name, "ok": true }).to_string());
    }
    Ok(0)
}

/// `{"error":{"code","message","file","line","hint"}}` in JSON mode, `error: …` otherwise.
#[derive(Debug)]
pub(crate) struct CliError {
    pub code: &'static str,
    pub message: String,
    pub file: Option<PathBuf>,
    pub line: Option<usize>,
    pub hint: Option<String>,
    pub exit: u8,
}

impl CliError {
    pub fn new(code: &'static str, message: impl Into<String>, exit: u8) -> Self {
        Self { code, message: message.into(), file: None, line: None, hint: None, exit }
    }

    pub fn at(mut self, file: &Path, line: Option<usize>) -> Self {
        (self.file, self.line) = (Some(file.to_path_buf()), line);
        self
    }

    pub fn at_line(mut self, line: usize) -> Self {
        self.line = Some(line);
        self
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn json(&self) -> Value {
        json!({ "error": {
            "code": self.code, "message": self.message,
            "file": self.file.as_ref().map(|f| f.display().to_string()), "line": self.line, "hint": self.hint,
        }})
    }

    pub fn print(&self, human: bool, out: &mut dyn Write) {
        if !human {
            put(out, self.json().to_string());
            return;
        }
        let at = match (&self.file, self.line) {
            (Some(f), Some(l)) => format!("{}:{l}: ", f.display()),
            (Some(f), None) => format!("{}: ", f.display()),
            (None, Some(l)) => format!("line {l}: "),
            _ => String::new(),
        };
        eprintln!("{} {at}{}", paint("error:", RED, true), self.message);
        if let Some(h) = &self.hint {
            eprintln!("  hint: {h}");
        }
    }
}

/// A failed catalog / version query.
fn db_error(e: &DbError) -> CliError {
    CliError::new(if e.connection_lost { "connection_lost" } else { "sql_error" }, e.to_string(), run::sql_exit(e))
}

/// The CLI creates its own tokio runtime (sqlx needs one); nothing else is async.
pub(crate) fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime")
}

fn open_session(env: &Env, name: &str, database: Option<&str>) -> Result<(tokio::runtime::Runtime, Session, db_conn::Connection), CliError> {
    let conn = env.pick(name)?.with_database(database);
    let rt = runtime();
    let s = rt.block_on(Session::open_with(&conn, std::time::Duration::from_millis(env.settings.run.connect_timeout_ms))).map_err(|e| CliError::new("connection_failed", format!("connection '{name}': {e}"), 2))?;
    Ok((rt, s, conn))
}

/// `.sql` files under `path` (respects .gitignore), or `path` itself.
fn sql_files(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        return vec![path.to_path_buf()];
    }
    let mut files: Vec<PathBuf> = ignore::Walk::new(path).flatten().map(|e| e.into_path()).filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "sql")).collect();
    files.sort();
    files
}

fn list(path: &Path, format: Format, out: &mut dyn Write) -> Result<u8, CliError> {
    if !path.exists() {
        return Err(CliError::new("file_not_found", "no such file or directory", 3).at(path, None));
    }
    let env = Env::load();
    let mut rows = vec![];
    for f in sql_files(path) {
        let src = std::fs::read_to_string(&f).map_err(|e| CliError::new("file_not_found", e.to_string(), 3).at(&f, None))?;
        // The dialect comes from the file's `-- @connection`, else Postgres.
        let dialect = sql_split::split(&src, Dialect::Postgres).connection.and_then(|n| env.conns.connections.get(&n).map(|c| dialect_of(c.driver))).unwrap_or_default();
        let file = sql_split::split(&src, dialect);
        for d in file.diagnostics.iter().filter(|d| d.severity == sql_split::Severity::Error) {
            eprintln!("{} {}:{}: {}", paint("warning:", output::YELLOW, true), f.display(), line_of(&src, d.range.start), d.message);
        }
        for st in &file.statements {
            rows.push(json!({ "file": f.display().to_string(), "line": st.line, "name": st.name, "statement": st.label(&src), "kind": kind_name(st.kind) }));
        }
    }
    emit_objects(out, format, &["file", "line", "name", "statement", "kind"], &rows);
    Ok(0)
}

fn connections(format: Format, out: &mut dyn Write) -> Result<u8, CliError> {
    let env = Env::load();
    let mut rows: Vec<Value> = env
        .conns
        .connections
        .values()
        .map(|c| {
            let mut v = db_conn::masked(c);
            v["password"] = json!(if c.has_password() { "set" } else { "none" });
            v
        })
        .collect();
    rows.extend(env.conns.errors.iter().map(|(n, e)| json!({ "name": n, "error": e })));
    rows.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    emit_objects(out, format, &["name", "driver", "url", "host", "socket", "port", "database", "user", "password", "read_only", "prod", "error"], &rows);
    Ok(0)
}

fn test_connection(name: &str, format: Format, out: &mut dyn Write) -> Result<u8, CliError> {
    let env = Env::load();
    let started = std::time::Instant::now();
    let (rt, mut s, _) = open_session(&env, name, None)?;
    let version = rt.block_on(s.run("SELECT version()", Kind::Query, &Page::default())).map_err(|e| db_error(&e))?;
    let version = version.rows.first().and_then(|r| r.first()).map(|v| v.display()).unwrap_or_default();
    let ms = started.elapsed().as_millis() as u64;
    rt.block_on(s.close());
    if format.human() {
        put(out, format!("OK {name}: {version} ({ms} ms)"));
    } else {
        put(out, json!({ "connection": name, "ok": true, "server": version, "duration_ms": ms }).to_string());
    }
    Ok(0)
}

fn databases(name: &str, format: Format, out: &mut dyn Write) -> Result<u8, CliError> {
    let env = Env::load();
    let conn = env.pick(name)?;
    let rt = runtime();
    let names = rt.block_on(db_exec::list_databases(&conn, std::time::Duration::from_millis(env.settings.run.connect_timeout_ms))).map_err(|e| CliError::new("connection_failed", format!("connection '{name}': {e}"), 2))?;
    let rows: Vec<Value> = names.into_iter().map(|n| json!({ "name": n })).collect();
    emit_objects(out, format, &["name"], &rows);
    Ok(0)
}

fn table_json(schema: &str, t: &Table) -> Value {
    let mut v = serde_json::to_value(t).unwrap_or_default();
    v["schema"] = json!(schema);
    v
}

fn schema(a: SchemaArgs, format: Format, out: &mut dyn Write) -> Result<u8, CliError> {
    if a.ddl && a.table.is_none() {
        return Err(CliError::new("bad_argument", "--ddl needs a table", 3));
    }
    let env = Env::load();
    let name = env.resolve(a.conn.connection.as_deref(), None)?.name;
    let (rt, mut s, _) = open_session(&env, &name, a.conn.database.as_deref())?;
    let schema = rt.block_on(s.schema()).map_err(|e| db_error(&e))?;
    let Some(want) = &a.table else {
        rt.block_on(s.close());
        let tables: Vec<Value> = schema.schemas.iter().flat_map(|n| n.tables.iter().map(|t| table_json(&n.name, t))).collect();
        match format {
            Format::Json => put(out, json!({ "schema": "siesta.schema/v1", "connection": name, "schemas": schema.schemas }).to_string()),
            Format::Jsonl => tables.iter().for_each(|t| put(out, t.to_string())),
            _ => {
                let rows: Vec<Value> = schema.schemas.iter().flat_map(|n| n.tables.iter().map(|t| {
                    let cols: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
                    json!({ "schema": n.name, "name": t.name, "kind": t.kind, "columns": cols.join(", ") })
                })).collect();
                emit_objects(out, format, &["schema", "name", "kind", "columns"], &rows);
            }
        }
        return Ok(0);
    };
    let Some((sch, t)) = schema.find(want) else {
        let all: Vec<String> = schema.schemas.iter().flat_map(|n| n.tables.iter().map(|t| format!("{}.{}", n.name, t.name))).collect();
        let needle = want.rsplit('.').next().unwrap_or(want).to_lowercase();
        let near: Vec<&str> = all.iter().filter(|n| n.to_lowercase().contains(&needle)).take(10).map(String::as_str).collect();
        let hint = if near.is_empty() { "run `siesta schema -c <connection>` to list the tables".to_string() } else { format!("similar: {}", near.join(", ")) };
        return Err(CliError::new("table_not_found", format!("no table '{want}' on '{name}'"), 3).hint(hint));
    };
    let ddl = if a.ddl { Some(rt.block_on(s.table_ddl(sch, &t.name)).map_err(|e| db_error(&e))?) } else { None };
    rt.block_on(s.close());
    let mut obj = table_json(sch, t);
    if let Some(d) = &ddl {
        obj["ddl"] = json!(d);
    }
    match (format, ddl) {
        (Format::Json, _) => put(out, json!({ "schema": "siesta.schema/v1", "connection": name, "table": obj }).to_string()),
        (Format::Jsonl, _) => put(out, obj.to_string()),
        (_, Some(d)) => put(out, d),
        (f, None) => {
            let cols = ["column", "type", "nullable", "primary_key"].map(String::from);
            let rows: Vec<Vec<String>> = t.columns.iter().map(|c| vec![c.name.clone(), c.data_type.clone(), c.nullable.to_string(), c.primary_key.to_string()]).collect();
            if f == Format::Csv {
                put(out, render_csv(&cols, &rows));
            } else {
                put(out, format!("{sch}.{} ({})", t.name, if t.kind == db_exec::TableKind::View { "view" } else { "table" }));
                put(out, render_table(&cols, &rows));
                for i in &t.indexes {
                    put(out, format!("index {} ({}){}{}", i.name, i.columns.join(", "), if i.primary { " PRIMARY KEY" } else { "" }, if i.unique && !i.primary { " UNIQUE" } else { "" }));
                }
            }
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// The agent skill documents every command, long flag and directive; extend it when this fails.
    #[test]
    fn skill_covers_cli() {
        use clap::CommandFactory as _;
        let skill = include_str!("../../../skills/siesta/SKILL.md");
        fn walk(cmd: &clap::Command, path: &str, skill: &str, missing: &mut Vec<String>) {
            for sub in cmd.get_subcommands().filter(|s| s.get_name() != "help") {
                let path = format!("{path} {}", sub.get_name());
                if !skill.contains(&path) {
                    missing.push(path.clone());
                }
                let flags = sub.get_arguments().filter_map(|a| a.get_long()).filter(|l| *l != "help");
                missing.extend(flags.filter(|l| !skill.contains(&format!("--{l}"))).map(|l| format!("{path} --{l}")));
                walk(sub, &path, skill, missing);
            }
        }
        let mut missing = vec![];
        walk(&Cli::command(), "siesta", skill, &mut missing);
        missing.extend(sql_split::DIRECTIVES.iter().filter(|d| !skill.contains(&format!("@{d}"))).map(|d| format!("@{d}")));
        for code in ["0", "2", "3", "4", "5"] {
            if !skill.contains(&format!("| `{code}` |")) {
                missing.push(format!("exit code {code}"));
            }
        }
        assert!(missing.is_empty(), "skills/siesta/SKILL.md misses: {missing:?}");
    }

    #[test]
    fn arg_routing() {
        for cli in [&["list", "a.sql"][..], &["run", "a.sql"], &["exec", "-c", "x", "SELECT 1"], &["connections"], &["schema"], &["--help"], &["-V"], &["help"]] {
            assert!(is_cli(&args(cli)), "{cli:?}");
        }
        // GUI: no arguments, `open`, a path (with or without :line).
        for gui in [&[][..], &["open"], &["open", "a.sql:3"], &["/tmp/project"], &["a.sql"], &["a.sql:12"], &["."]] {
            assert!(!is_cli(&args(gui)), "{gui:?}");
        }
    }

    #[test]
    fn bad_input_exits_3_and_help_exits_0() {
        let mut out = vec![];
        assert_eq!(run_with(args(&["siesta", "run"]), &mut out), 3);
        assert_eq!(run_with(args(&["siesta", "run", "a.sql", "--output", "xml"]), &mut out), 3);
        assert_eq!(run_with(args(&["siesta", "--version"]), &mut out), 0);
    }

    // Tests that load connections (they read `SIESTA_CONFIG_DIR`) live in tests/config.rs.

    #[test]
    fn set_password_from_stdin_encrypts_into_the_connection_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("connections")).unwrap();
        std::fs::write(dir.path().join("connections/kc-cli-a.json"), r#"{"driver": "postgres", "host": "h"}"#).unwrap();
        let keys = dir.path().join("data");
        let set = |name: &str, input: &str| set_password(name, Some(dir.path()), Some(&keys), false, &mut input.as_bytes(), Format::Json, &mut vec![]);
        assert_eq!(set("kc-cli-a", "hunter2\r\nignored\n").unwrap(), 0);
        assert!(db_conn::crypto::key_path(&keys).is_file() && !db_conn::crypto::key_path(dir.path()).exists());
        let resolved = || db_conn::Connections::load(dir.path()).connections["kc-cli-a"].resolve_password_in(&keys).unwrap().password;
        assert_eq!(resolved().as_deref(), Some("hunter2"));
        assert!(!std::fs::read_to_string(dir.path().join("connections/kc-cli-a.json")).unwrap().contains("hunter2"));
        assert_eq!(set("nope", "x\n").unwrap_err().code, "unknown_connection");
        assert_eq!(set("kc-cli-a", "\n").unwrap_err().code, "no_password");
        assert_eq!(resolved().as_deref(), Some("hunter2"));
    }
}
