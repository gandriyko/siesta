//! `siesta mcp`: a Model Context Protocol server on stdio (newline-delimited JSON-RPC).
//! Each tool runs this binary's CLI command with `--output json` and returns what it printed,
//! so tools behave exactly like the commands the skill documents.
//! SQL never runs with `--yes` and stdin is closed: `read_only` connections refuse writes, and
//! whatever needs confirmation (`prod`, `-- @confirm`, `run.confirm_destructive`) is refused too.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

/// Set for the child commands: on a `prod` connection only reads and `UPDATE … WHERE` run
/// (`sql_split::prod_agent_violation`), the latter without confirmation.
pub(crate) const AGENT_ENV: &str = "SIESTA_MCP";

/// `tools/call` requests in progress: request id (JSON text) → child pid, once spawned.
type Calls = Arc<Mutex<HashMap<String, Option<u32>>>>;

/// Each `tools/call` runs on its own thread, so `notifications/cancelled` is read while it runs:
/// the child gets SIGTERM, which cancels its statement on the server, and the request gets no reply.
/// When the client goes away (stdin closes), every child gets SIGTERM.
pub(crate) fn serve() -> u8 {
    let stdin = std::io::stdin();
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let calls: Calls = Arc::default();
    let send = |out: &Mutex<std::io::Stdout>, r: Value| {
        let mut o = out.lock().unwrap();
        writeln!(o, "{r}").is_ok() && o.flush().is_ok()
    };
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let msg = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => msg,
            Err(e) => {
                if !send(&out, json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}})) {
                    break;
                }
                continue;
            }
        };
        match (msg["method"].as_str(), msg.get("id")) {
            (Some("notifications/cancelled"), _) => {
                // A call not yet spawned is dropped by its thread; a finished one is not in the map.
                if let Some(Some(pid)) = calls.lock().unwrap().remove(&msg["params"]["requestId"].to_string()) {
                    terminate(pid);
                }
            }
            (Some("tools/call"), Some(id)) => {
                let key = id.to_string();
                calls.lock().unwrap().insert(key.clone(), None);
                let (out, calls) = (out.clone(), calls.clone());
                std::thread::spawn(move || {
                    let on_spawn = |pid| match calls.lock().unwrap().get_mut(&key) {
                        Some(slot) => *slot = Some(pid),
                        None => terminate(pid),
                    };
                    let reply = handle(&msg, &on_spawn);
                    // Gone from the map = cancelled: no reply.
                    if calls.lock().unwrap().remove(&key).is_some()
                        && let Some(r) = reply
                    {
                        send(&out, r);
                    }
                });
            }
            _ => {
                if let Some(r) = handle(&msg, &|_| {})
                    && !send(&out, r)
                {
                    break;
                }
            }
        }
    }
    for pid in calls.lock().unwrap().values().flatten() {
        terminate(*pid);
    }
    0
}

/// SIGTERM: the CLI cancels its running statement on the server and exits.
// ponytail: `kill` command instead of a libc dependency. A pid freed between the child's exit and
// its removal from `Calls` could be reused; that window is microseconds.
fn terminate(pid: u32) {
    let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).status();
}

/// The reply to one message; `None` for notifications. `on_spawn` gets a tool's child pid.
fn handle(msg: &Value, on_spawn: &dyn Fn(u32)) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let result = match msg["method"].as_str().unwrap_or("") {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "siesta", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Query Postgres / MySQL databases and Elasticsearch clusters through Siesta connections. Start with \
                `connections` (its `driver` tells SQL from `elasticsearch`), then `schema` to see tables and columns (Elasticsearch: \
                indices and mapping fields) before writing a query. On an Elasticsearch connection `run_sql` takes Kibana console \
                requests instead of SQL: a `METHOD /path` line, then a JSON body. `run_sql` can write, except on `read_only` \
                connections; on a `prod` connection only reads and `UPDATE … WHERE` (Elasticsearch: `POST <index>/_update/<id>` with a `doc` body) \
                run. Anything refused (exit 4) is for the user to run.",
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => call(params["name"].as_str().unwrap_or(""), &params["arguments"], on_spawn),
        m => Err((-32601, format!("unknown method '{m}'"))),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    })
}

fn call(name: &str, args: &Value, on_spawn: &dyn Fn(u32)) -> Result<Value, (i64, String)> {
    let (argv, input) = argv(name, args).map_err(|e| (-32602, e))?;
    let mut child = Command::new(std::env::current_exe().map_err(|e| (-32603, e.to_string()))?)
        .args(&argv)
        .env(AGENT_ENV, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| (-32603, e.to_string()))?;
    on_spawn(child.id());
    // Always close stdin: no command may wait for input (and no prompt can be answered).
    if let Some(mut s) = child.stdin.take() {
        let _ = s.write_all(input.unwrap_or_default().as_bytes());
    }
    let o = child.wait_with_output().map_err(|e| (-32603, e.to_string()))?;
    let code = o.status.code().unwrap_or(-1);
    let mut text = String::from_utf8_lossy(&o.stdout).trim_end().to_string();
    let err = String::from_utf8_lossy(&o.stderr);
    if !err.trim().is_empty() {
        text += &format!("\n\nstderr:\n{}", err.trim_end());
    }
    if code != 0 {
        text += &format!("\n\nexit code {code} (2 connection error · 3 bad input · 4 refused: read-only connection or needs confirmation, ask the user · 5 SQL / Elasticsearch error)");
    }
    Ok(json!({"content": [{"type": "text", "text": text}], "isError": code != 0}))
}

/// The agent may name only query files (`.sql` / `.es`), also behind a symlink: the tools print
/// statement text, so any other path would read an arbitrary file (`~/.ssh/…`, `.env`).
fn query_file(p: &str) -> Result<(), String> {
    let path = Path::new(p);
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if siesta_workspace::is_sql(path) && siesta_workspace::is_sql(&target) {
        Ok(())
    } else {
        Err(format!("'{p}' is not a .sql or .es file"))
    }
}

/// CLI arguments for a tool call, plus stdin for the child.
fn argv(name: &str, a: &Value) -> Result<(Vec<String>, Option<String>), String> {
    let str_of = |k: &str| a.get(k).and_then(Value::as_str).map(String::from);
    let mut v: Vec<String> = vec![];
    let opt = |v: &mut Vec<String>, k: &str, flag: &str| {
        if let Some(x) = a.get(k).filter(|x| !x.is_null()) {
            v.push(flag.into());
            v.push(x.as_str().map_or_else(|| x.to_string(), String::from));
        }
    };
    let conn = |v: &mut Vec<String>| {
        opt(v, "connection", "--connection");
        opt(v, "database", "--database");
    };
    let json = ["--output".to_string(), "json".into()];
    let mut input = None;
    match name {
        "connections" => {
            v.push("connections".into());
            v.extend(json);
        }
        "list_statements" => {
            v.push("list".into());
            if let Some(p) = str_of("path") {
                if !Path::new(&p).is_dir() {
                    query_file(&p)?;
                }
                v.push(p);
            }
            v.extend(json);
        }
        "schema" => {
            v.push("schema".into());
            v.extend(str_of("table"));
            conn(&mut v);
            if a.get("ddl").and_then(Value::as_bool) == Some(true) {
                v.push("--ddl".into());
            }
            v.extend(json);
        }
        "run_sql" => {
            match (str_of("sql"), str_of("file")) {
                // SQL via stdin: text starting with `--` would otherwise parse as a flag.
                (Some(sql), None) => {
                    input = Some(sql);
                    v.extend(["exec".into(), "-".into()]);
                }
                (None, Some(file)) => {
                    query_file(&file)?;
                    v.extend(["run".into(), file]);
                    let sts = a.get("statements").and_then(Value::as_array);
                    v.extend(sts.into_iter().flatten().filter_map(|s| s.as_str().map(String::from)));
                }
                _ => return Err("give exactly one of 'sql' or 'file'".into()),
            }
            conn(&mut v);
            opt(&mut v, "max_rows", "--max-rows");
            v.extend(json);
        }
        _ => return Err(format!("unknown tool '{name}'")),
    }
    Ok((v, input))
}

fn tools() -> Value {
    let s = |d: &str| json!({"type": "string", "description": d});
    let connection = s("Connection name (see `connections`); for a file, overrides its `-- @connection`");
    let database = s("Database on that server (overrides `-- @database` and the connection's default)");
    let tool = |name: &str, d: &str, props: Value, required: &[&str], read_only: bool| {
        json!({"name": name, "description": d, "inputSchema": {"type": "object", "properties": props, "required": required},
               "annotations": {"readOnlyHint": read_only}})
    };
    json!([
        tool("connections", "Every connection: name, driver, host, database, user, read_only, prod; secrets masked.", json!({}), &[], true),
        tool("list_statements", "Statements of a .sql / .es file or every such file under a directory: file, line, name, first line, kind.",
            json!({"path": s("File or directory (default: current directory)")}), &[], true),
        tool("schema", "Tables and views with columns and indexes of a connection (Elasticsearch: indices with their mapping fields); one table with `table`, its CREATE statement with `ddl` (Elasticsearch: `PUT /index` with the mapping).",
            json!({"table": s("`table` or `schema.table`; none = every table and view"), "connection": connection, "database": database,
                   "ddl": json!({"type": "boolean", "description": "Return the table's CREATE statement"})}), &[], true),
        tool("run_sql", "Run SQL (or Elasticsearch requests) and return siesta.run/v1 JSON per statement (columns, rows, rows_truncated, rows_affected, duration_ms). \
            Give `sql` (with `connection`) or a `file` (with optional `statements`). On an Elasticsearch connection `sql` holds \
            requests in Kibana console format (`GET /index/_search` then a JSON body); search hits come back as rows (`_id` + `_source` fields), \
            other responses as one `response` JSON cell. Refused with exit 4 before anything runs: \
            writes on a `read_only` connection; on a `prod` connection everything but reads and `UPDATE` with a `WHERE` \
            (Elasticsearch: `POST <index>/_update/<id>` with a `doc` body, no script or upsert; DELETE, INSERT, DROP, TRUNCATE, ALTER…: `prod_refused`); statements that need confirmation (`-- @confirm`, \
            destructive ones when `run.confirm_destructive` is `always`) — ask the user to run those.",
            json!({"sql": s("SQL text, or Elasticsearch requests on an Elasticsearch connection; one or several statements"), "file": s("A .sql / .es file to run instead of `sql`"),
                   "statements": json!({"type": "array", "items": {"type": "string"}, "description": "Statements of `file`: `@name`, `:LINE` or `#N`; none = all"}),
                   "connection": connection, "database": database,
                   "max_rows": json!({"type": "integer", "description": "Rows per statement (default 100; 0 = the results.max_rows cap)"})}),
            &[], false),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_and_arguments() {
        let handle = |m: &Value| handle(m, &|_| {});
        let init = handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}})).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert!(handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none());
        assert_eq!(handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "nope"})).unwrap()["error"]["code"], -32601);

        for t in tools().as_array().unwrap() {
            let n = t["name"].as_str().unwrap();
            assert!(argv(n, &json!({"sql": "SELECT 1"})).is_ok(), "{n}");
        }
        let (v, input) = argv("run_sql", &json!({"sql": "-- x\nSELECT 1", "connection": "dev", "max_rows": 5})).unwrap();
        assert_eq!(v.join(" "), "exec - --connection dev --max-rows 5 --output json");
        assert_eq!(input.as_deref(), Some("-- x\nSELECT 1"));
        let (v, _) = argv("run_sql", &json!({"file": "a.sql", "statements": ["@users", ":3"]})).unwrap();
        assert_eq!(v.join(" "), "run a.sql @users :3 --output json");
        assert!(!v.contains(&"--yes".to_string()));
        assert!(argv("run_sql", &json!({})).is_err());
        assert!(argv("run_sql", &json!({"sql": "x", "file": "a.sql"})).is_err());
        assert_eq!(argv("schema", &json!({"table": "t", "ddl": true})).unwrap().0.join(" "), "schema t --ddl --output json");
    }

    #[test]
    fn only_query_files_can_be_named() {
        let dir = tempfile::tempdir().unwrap();
        let p = |n: &str| dir.path().join(n).to_string_lossy().into_owned();
        std::fs::write(p("secret.env"), "TOKEN=x").unwrap();
        std::fs::write(p("q.es"), "GET /").unwrap();
        std::os::unix::fs::symlink(p("secret.env"), p("trap.sql")).unwrap();
        assert!(argv("run_sql", &json!({"file": p("q.es")})).is_ok());
        assert!(argv("list_statements", &json!({"path": p("q.es")})).is_ok());
        assert!(argv("list_statements", &json!({"path": dir.path()})).is_ok());
        for bad in [p("secret.env"), p("trap.sql"), "/etc/hosts".into()] {
            assert!(argv("run_sql", &json!({"file": bad})).unwrap_err().contains("not a .sql or .es file"), "{bad}");
            assert!(argv("list_statements", &json!({"path": bad})).is_err(), "{bad}");
        }
    }
}
