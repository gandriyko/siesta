//! CLI behaviour that needs a config dir but no database. Every test points `SIESTA_CONFIG_DIR` and
//! `SIESTA_DATA_DIR` (where the encryption key lives) at its own temp dir (never the real
//! `~/.config/siesta` / `~/.siesta`) and holds `ENV` meanwhile.

use serde_json::Value;

static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Env {
    dir: tempfile::TempDir,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl Env {
    /// A config dir with one file per `(name, json)` connection.
    fn new(conns: &[(&str, &str)]) -> Env {
        let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("config/connections")).unwrap();
        for (n, j) in conns {
            std::fs::write(dir.path().join(format!("config/connections/{n}.json")), j).unwrap();
        }
        unsafe { std::env::set_var("SIESTA_CONFIG_DIR", dir.path().join("config")) };
        unsafe { std::env::set_var("SIESTA_DATA_DIR", dir.path().join("data")) };
        Env { dir, _guard: guard }
    }

    fn go(&self, args: &[&str]) -> (u8, Value, String) {
        let mut a = vec!["siesta".to_string()];
        a.extend(args.iter().map(|s| s.to_string()));
        let mut out = vec![];
        let code = siesta_cli::run_with(a, &mut out);
        let text = String::from_utf8(out).unwrap();
        (code, serde_json::from_str(&text).unwrap_or(Value::Null), text)
    }
}

#[test]
fn errors_map_to_exit_codes_and_json() {
    let e = Env::new(&[("dead", r#"{"driver": "postgres", "host": "127.0.0.1", "port": 1, "database": "x", "user": "u"}"#)]);
    let f = e.dir.path().join("a.sql");
    std::fs::write(&f, "SELECT 1;").unwrap();
    let file = f.to_str().unwrap();
    let code_of = |args: &[&str]| {
        let (code, v, _) = e.go(args);
        (code, v["error"]["code"].as_str().map(String::from))
    };
    let expect = |args: &[&str], code: u8, name: &str| assert_eq!(code_of(args), (code, Some(name.to_string())), "{args:?}");
    // No connection chosen, unknown connection, missing file, unknown statement: all 3.
    expect(&["run", file, "--output", "json"], 3, "no_connection");
    expect(&["run", file, "-c", "nope", "--output", "json"], 3, "unknown_connection");
    expect(&["run", "/no/such.sql", "--output", "json"], 3, "file_not_found");
    // Unreachable server: 2.
    expect(&["run", file, "-c", "dead", "--output", "json"], 2, "connection_failed");
    expect(&["run", file, "-c", "dead", "#9", "--output", "json"], 3, "statement_not_found");
    // Unclosed string: parse error.
    std::fs::write(&f, "SELECT 'abc;").unwrap();
    expect(&["run", file, "-c", "dead", "--output", "json"], 3, "parse_error");
}

#[test]
fn agent_on_prod_runs_only_reads_and_update_where() {
    let e = Env::new(&[("live", r#"{"driver": "postgres", "host": "127.0.0.1", "port": 1, "database": "x", "user": "u", "prod": true}"#)]);
    unsafe { std::env::set_var("SIESTA_MCP", "1") };
    let code_of = |sql: &str| {
        let (code, v, _) = e.go(&["exec", "-c", "live", "--output", "json", "--", sql]);
        (code, v["error"]["code"].as_str().map(String::from))
    };
    // Refused before connecting (4); allowed ones get as far as the dead server (2), without --yes.
    for sql in ["DROP TABLE t", "TRUNCATE t", "DELETE FROM t WHERE id = 1", "UPDATE t SET a = 1", "INSERT INTO t VALUES (1)"] {
        assert_eq!(code_of(sql), (4, Some("prod_refused".into())), "{sql}");
    }
    for sql in ["SELECT 1", "UPDATE t SET a = 1 WHERE id = 1"] {
        assert_eq!(code_of(sql), (2, Some("connection_failed".into())), "{sql}");
    }
    assert_eq!(code_of("-- @confirm\nUPDATE t SET a = 1 WHERE id = 1"), (4, Some("confirmation_required".into())));
    unsafe { std::env::remove_var("SIESTA_MCP") };
    // A Claude Code shell is an agent too: same rules, and --yes answers nothing for it.
    unsafe { std::env::set_var("CLAUDECODE", "1") };
    assert_eq!(code_of("DELETE FROM t WHERE id = 1"), (4, Some("prod_refused".into())));
    let (code, v, _) = e.go(&["exec", "-c", "live", "--yes", "--output", "json", "--", "-- @confirm\nSELECT 1"]);
    assert_eq!((code, v["error"]["code"].as_str()), (4, Some("confirmation_required")));
    unsafe { std::env::remove_var("CLAUDECODE") };
    // A person's CLI keeps its own rule: prod writes need --yes.
    assert_eq!(code_of("UPDATE t SET a = 1 WHERE id = 1"), (4, Some("confirmation_required".into())));
}

#[test]
fn list_prints_statements() {
    let e = Env::new(&[]);
    std::fs::write(e.dir.path().join("a.sql"), "-- @name one\nSELECT 1;\nDELETE FROM t;\n").unwrap();
    std::fs::write(e.dir.path().join("b.txt"), "x").unwrap();
    let (code, _, text) = e.go(&["list", e.dir.path().to_str().unwrap(), "--output", "jsonl"]);
    assert_eq!(code, 0);
    let rows: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[0]["name"].as_str(), rows[0]["line"].as_u64(), rows[0]["kind"].as_str()), (Some("one"), Some(2), Some("query")));
    assert_eq!((rows[1]["statement"].as_str(), rows[1]["kind"].as_str()), (Some("DELETE FROM t"), Some("other")));
}

#[test]
fn connections_list_is_masked_and_shows_password_state() {
    let e = Env::new(&[
        ("cfg-url", r#"{"url": "mysql://u:s3cret@h/db"}"#),
        ("cfg-kc", r#"{"driver": "postgres", "host": "h", "password": "leaked"}"#),
        ("cfg-none", r#"{"driver": "postgres", "host": "h"}"#),
        ("cfg-bad", r#"{"driver": "oracle"}"#),
    ]);
    db_conn::set_password(&e.dir.path().join("config"), &e.dir.path().join("data"), "cfg-kc", "hunter2").unwrap();
    let (code, v, text) = e.go(&["connections", "--output", "json"]);
    assert_eq!(code, 0);
    assert!(!text.contains("s3cret") && !text.contains("hunter2") && !text.contains("leaked"), "{text}");
    let row = |n: &str| v.as_array().unwrap().iter().find(|c| c["name"] == n).unwrap().clone();
    assert_eq!((row("cfg-url")["password"].as_str(), row("cfg-kc")["password"].as_str(), row("cfg-none")["password"].as_str()), (Some("set"), Some("set"), Some("none")));
    assert!(row("cfg-bad")["error"].as_str().unwrap().contains("oracle"));
}

#[test]
fn undecryptable_password_exits_2_with_a_clear_message() {
    let e = Env::new(&[("cfg-enc", r#"{"driver": "postgres", "host": "127.0.0.1", "port": 1, "password_enc": "v1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}"#)]);
    let (code, _, text) = e.go(&["connections", "test", "cfg-enc", "--output", "json"]);
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("Can't decrypt the password of `cfg-enc`") && text.contains("Set the password again"), "{text}");
}
