//! CLI against real servers: `scripts/test-db.sh` (Docker Postgres 25432 / MariaDB 23306).

use serde_json::{Value, json};

/// Tests share the process environment (`SIESTA_CONFIG_DIR`), so a fixture holds this for its lifetime.
static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Fixture {
    dir: tempfile::TempDir,
    driver: &'static str,
    _env: std::sync::MutexGuard<'static, ()>,
}

impl Fixture {
    fn new(driver: &'static str, url_var: &str) -> Fixture {
        let env = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        // The password stays out of the files: the url comes from the environment (`${env:}`).
        let cfg = dir.path().join("config");
        std::fs::create_dir_all(cfg.join("connections")).unwrap();
        unsafe { std::env::set_var("SIESTA_CONFIG_DIR", &cfg) };
        assert!(std::env::var(url_var).is_ok(), "{url_var} is not set; run scripts/test-db.sh");
        let url = format!("${{env:{url_var}}}");
        for (name, ro, prod) in [("c", false, false), ("prod-c", false, true), ("ro", true, false)] {
            let conn = json!({ "driver": driver, "url": url, "read_only": ro, "prod": prod });
            std::fs::write(cfg.join("connections").join(format!("{name}.json")), conn.to_string()).unwrap();
        }
        Fixture { dir, driver, _env: env }
    }

    fn file(&self, name: &str, sql: &str) -> String {
        let p = self.dir.path().join(name);
        std::fs::write(&p, sql).unwrap();
        p.display().to_string()
    }

    /// Runs the CLI; returns the exit code and stdout.
    fn go(&self, args: &[&str]) -> (u8, String) {
        let mut a = vec!["siesta".to_string()];
        a.extend(args.iter().map(|s| s.to_string()));
        let mut out = vec![];
        let code = siesta_cli::run_with(a, &mut out);
        (code, String::from_utf8(out).unwrap())
    }

    fn json(&self, args: &[&str]) -> (u8, Value) {
        let (code, out) = self.go(args);
        (code, serde_json::from_str(&out).unwrap_or_else(|e| panic!("not JSON ({e}): {out}")))
    }
}

fn scenario(f: &Fixture, sql_error_state: &str) {
    fn with<'a>(a: &[&'a str]) -> Vec<&'a str> {
        [a, &["--output", "json"]].concat()
    }

    // connections: masked, with a prod flag.
    let (code, v) = f.json(&["connections", "--output", "json"]);
    assert_eq!(code, 0);
    let names: Vec<_> = v.as_array().unwrap().iter().map(|c| (c["name"].as_str().unwrap(), c["prod"].as_bool().unwrap())).collect();
    assert_eq!(names, [("c", false), ("prod-c", true), ("ro", false)]);
    assert!(!v.to_string().contains("siesta:siesta@"), "password leaked: {v}");
    let (code, v) = f.json(&["connections", "test", "c", "--output", "json"]);
    assert_eq!((code, v["ok"].as_bool()), (0, Some(true)), "{v}");
    assert!(!v["server"].as_str().unwrap().is_empty());
    let (code, v) = f.json(&["connections", "test", "nope", "--output", "json"]);
    assert_eq!((code, v["error"]["code"].as_str()), (3, Some("unknown_connection")));

    // exec: several statements -> an array; rows_affected, columns, NULL.
    let (code, v) = f.json(&with(&[
        "exec", "-c", "c",
        "DROP TABLE IF EXISTS siesta_cli_t; CREATE TABLE siesta_cli_t (id INT PRIMARY KEY, name VARCHAR(20)); \
         INSERT INTO siesta_cli_t VALUES (1,'a'),(2,'b'),(3,'c'),(4,'d'),(5,NULL);",
    ]));
    assert_eq!(code, 0, "{v}");
    let items = v.as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[2]["rows_affected"], 5);
    assert_eq!(items[2]["schema"], "siesta.run/v1");
    assert!(items[2]["file"].is_null() && items[2]["statement"]["line"].is_u64());

    let (code, v) = f.json(&with(&["exec", "-c", "c", "SELECT id, name FROM siesta_cli_t ORDER BY id"]));
    assert_eq!(code, 0);
    assert_eq!(v["rows"][4], json!([5, null]));
    assert_eq!(v["rows"].as_array().unwrap().len(), 5);
    assert_eq!((v["rows_truncated"].as_bool(), v["columns"][0]["name"].as_str(), v["connection"].as_str()), (Some(false), Some("id"), Some("c")));
    assert!(v["rows_affected"].is_null());

    // --max-rows cuts and says so.
    let (_, v) = f.json(&with(&["exec", "-c", "c", "--max-rows", "2", "SELECT id FROM siesta_cli_t ORDER BY id"]));
    assert_eq!((v["rows"].as_array().unwrap().len(), v["rows_truncated"].as_bool()), (2, Some(true)));

    // jsonl: one object per line; csv: header + rows, NULL empty; table: NULL as text.
    let (code, out) = f.go(&["exec", "-c", "c", "--output", "jsonl", "SELECT 1; SELECT 2"]);
    assert_eq!((code, out.lines().count()), (0, 2));
    let (_, out) = f.go(&["exec", "-c", "c", "--output", "csv", "SELECT id, name FROM siesta_cli_t WHERE id >= 4 ORDER BY id"]);
    assert_eq!(out, "id,name\n4,d\n5,\n");
    let (_, out) = f.go(&["exec", "-c", "c", "--output", "table", "SELECT id, name FROM siesta_cli_t WHERE id >= 4 ORDER BY id"]);
    assert!(out.contains("5  | NULL") && out.contains("(2 rows,"), "{out}");

    // SQL error: 5 with sqlstate; a later statement does not run.
    let (code, v) = f.json(&with(&["exec", "-c", "c", "SELECT * FROM siesta_cli_nope; SELECT 1"]));
    assert_eq!(code, 5);
    assert_eq!(v["error"]["sqlstate"], sql_error_state, "{v}");
    assert!(!v.is_array(), "the second statement must not run: {v}");

    // Refusal: every non-read statement on a prod connection, 4 and nothing runs; --yes runs.
    let (code, v) = f.json(&with(&["exec", "-c", "prod-c", "DELETE FROM siesta_cli_t"]));
    assert_eq!((code, v["error"]["code"].as_str()), (4, Some("confirmation_required")));
    let (code, v) = f.json(&with(&["exec", "-c", "prod-c", "UPDATE siesta_cli_t SET name = 'y'"]));
    assert_eq!((code, v["error"]["code"].as_str()), (4, Some("confirmation_required")));
    // Reads on prod run without --yes.
    let (code, v) = f.json(&with(&["exec", "-c", "prod-c", "SELECT 1"]));
    assert_eq!(code, 0, "{v}");
    let (_, v) = f.json(&with(&["exec", "-c", "c", "SELECT count(*) AS n FROM siesta_cli_t"]));
    assert_eq!(v["rows"][0][0], 5);
    let (code, v) = f.json(&with(&["exec", "-c", "prod-c", "--yes", "DELETE FROM siesta_cli_t WHERE id = 5"]));
    assert_eq!((code, v["rows_affected"].as_u64()), (0, Some(1)));
    let (code, v) = f.json(&with(&["exec", "-c", "prod-c", "--yes", "UPDATE siesta_cli_t SET name = 'z'"]));
    assert_eq!((code, v["rows_affected"].as_u64()), (0, Some(4)));
    // A batch is refused before anything runs.
    let (code, _) = f.json(&with(&["exec", "-c", "prod-c", "INSERT INTO siesta_cli_t VALUES (9, 'x'); TRUNCATE siesta_cli_t"]));
    assert_eq!(code, 4);
    let (_, v) = f.json(&with(&["exec", "-c", "c", "SELECT count(*) FROM siesta_cli_t"]));
    assert_eq!(v["rows"][0][0], 4);

    // read-only: siesta refuses any write itself (4), reads work; flag and config alike.
    let (code, v) = f.json(&with(&["exec", "-c", "c", "--read-only", "INSERT INTO siesta_cli_t VALUES (7, 'q')"]));
    assert_eq!((code, v["error"]["code"].as_str()), (4, Some("read_only_refused")), "{v}");
    let (code, v) = f.json(&with(&["exec", "-c", "ro", "INSERT INTO siesta_cli_t VALUES (7, 'q')"]));
    assert_eq!((code, v["error"]["code"].as_str()), (4, Some("read_only_refused")), "{v}");
    // The whole batch is checked first: the leading SELECT does not run either.
    let (code, v) = f.json(&with(&["exec", "-c", "ro", "SELECT 1; DROP TABLE siesta_cli_t"]));
    assert_eq!((code, v["error"]["code"].as_str()), (4, Some("read_only_refused")), "{v}");
    let (code, _) = f.json(&with(&["exec", "-c", "ro", "SELECT 1"]));
    assert_eq!(code, 0);
    // Switching the session to read-write is refused by siesta itself: exit 4, nothing sent.
    let (code, v) = f.json(&with(&["exec", "-c", "ro", "START TRANSACTION READ WRITE; DELETE FROM siesta_cli_t WHERE id = 1"]));
    assert_eq!(code, 4, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("not a security boundary"), "{v}");
    let (_, v) = f.json(&with(&["exec", "-c", "c", "SELECT count(*) FROM siesta_cli_t"]));
    assert_eq!(v["rows"][0][0], 4);

    // run: @connection, @name, :LINE, #N, whole file, @confirm.
    let file = f.file(
        "q.sql",
        "-- @connection c\n\n-- @name first\nSELECT id FROM siesta_cli_t WHERE id = 1;\n\n-- @name second\nSELECT\n  id FROM siesta_cli_t WHERE id = 2;\n\n-- @confirm\nUPDATE siesta_cli_t SET name = 'k' WHERE id = 1;\n",
    );
    let (code, v) = f.json(&with(&["run", &file, "second"]));
    assert_eq!((code, v["rows"][0][0].clone(), v["statement"].clone()), (0, json!(2), json!({"name": "second", "line": 7})), "{v}");
    assert_eq!(v["file"], file.as_str());
    let (_, v) = f.json(&with(&["run", &file, ":4"]));
    assert_eq!(v["rows"][0][0], 1);
    let (_, v) = f.json(&with(&["run", &file, ":8"]));
    assert_eq!(v["rows"][0][0], 2);
    let (code, v) = f.json(&with(&["run", &file, "#1", "@second"]));
    assert_eq!((code, v.as_array().map(Vec::len)), (0, Some(2)));
    let (code, v) = f.json(&with(&["run", &file, "#3"]));
    assert_eq!((code, v["error"]["code"].as_str()), (4, Some("confirmation_required")), "{v}");
    let (code, v) = f.json(&with(&["run", &file, "--yes", "#3"]));
    assert_eq!((code, v["rows_affected"].as_u64()), (0, Some(1)));
    let (code, _) = f.json(&with(&["run", &file]));
    assert_eq!(code, 4, "whole file includes the @confirm statement");
    let (code, v) = f.json(&with(&["run", &file, "-c", "nope", "#1"]));
    assert_eq!((code, v["error"]["code"].as_str()), (3, Some("unknown_connection")));
    let (code, v) = f.json(&with(&["run", &file, "nothere"]));
    assert_eq!((code, v["error"]["code"].as_str()), (3, Some("statement_not_found")));

    // list.
    let (_, v) = f.json(&["list", &file, "--output", "json"]);
    let rows = v.as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!((rows[1]["name"].as_str(), rows[1]["line"].as_u64()), (Some("second"), Some(7)));

    // schema: all tables, one table, DDL, unknown table.
    let (code, v) = f.json(&["schema", "-c", "c", "--output", "json"]);
    assert_eq!(code, 0);
    assert!(v.to_string().contains("siesta_cli_t"));
    let (code, v) = f.json(&["schema", "-c", "c", "siesta_cli_t", "--output", "json"]);
    assert_eq!(code, 0);
    let cols: Vec<_> = v["table"]["columns"].as_array().unwrap().iter().map(|c| (c["name"].as_str().unwrap(), c["primary_key"].as_bool().unwrap(), c["nullable"].as_bool().unwrap())).collect();
    assert_eq!(cols, [("id", true, false), ("name", false, true)]);
    let (code, v) = f.json(&["schema", "-c", "c", "siesta_cli_t", "--ddl", "--output", "json"]);
    assert_eq!(code, 0);
    assert!(v["table"]["ddl"].as_str().unwrap().to_uppercase().contains("CREATE TABLE"));
    let (_, out) = f.go(&["schema", "-c", "c", "siesta_cli_t", "--output", "table"]);
    assert!(out.contains("siesta_cli_t (table)") && out.contains("PRIMARY KEY"), "{out}");
    let (code, v) = f.json(&["schema", "-c", "c", "siesta_cli_zzz", "--output", "json"]);
    assert_eq!((code, v["error"]["code"].as_str()), (3, Some("table_not_found")));

    f.json(&with(&["exec", "-c", "c", "DROP TABLE siesta_cli_t"]));
    let _ = f.driver;
}

#[test]
#[ignore = "needs docker: scripts/test-db.sh"]
fn postgres() {
    let f = Fixture::new("postgres", "SIESTA_TEST_PG_URL");
    scenario(&f, "42P01");
    // JSON / JSONB are nested values, not strings.
    let (code, v) = f.json(&["exec", "-c", "c", "SELECT '{\"a\": [1, {\"b\": null}]}'::jsonb AS j, 1.50::numeric AS n", "--output", "json"]);
    assert_eq!(code, 0);
    assert_eq!(v["rows"][0], json!([{"a": [1, {"b": null}]}, "1.50"]));
    assert_eq!(v["columns"][0]["type"], "jsonb");
}

#[test]
#[ignore = "needs docker: scripts/test-db.sh"]
fn mysql() {
    let f = Fixture::new("mysql", "SIESTA_TEST_MYSQL_URL");
    scenario(&f, "42S02");
}

#[test]
#[ignore = "needs docker: scripts/test-db.sh"]
fn mariadb_json_column_is_nested_json() {
    let f = Fixture::new("mysql", "SIESTA_TEST_MYSQL_URL");
    let (code, v) = f.json(&[
        "exec", "-c", "c", "--output", "json",
        "DROP TABLE IF EXISTS siesta_cli_json; CREATE TABLE siesta_cli_json (id INT, doc JSON); \
         INSERT INTO siesta_cli_json VALUES (1, '{\"a\": [1, {\"b\": null}]}'); SELECT doc, CAST(0xDEAD AS BINARY) AS bin FROM siesta_cli_json; \
         DROP TABLE siesta_cli_json",
    ]);
    assert_eq!(code, 0, "{v}");
    let sel = &v.as_array().unwrap()[3];
    assert_eq!(sel["rows"][0][0], json!({"a": [1, {"b": null}]}), "{sel}");
    assert_eq!(sel["rows"][0][1], "dead", "real binary stays bytes: {sel}");
}
