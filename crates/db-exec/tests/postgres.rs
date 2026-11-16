//! Needs a running Postgres: `scripts/test-db.sh`.
mod common;

use std::time::{Duration, Instant};

use common::*;
use db_conn::Driver;
use db_exec::{Page, Session, Value};
use serde_json::json;
use sql_split::Kind;

const PG: Driver = Driver::Postgres;

async fn open_pg() -> Session {
    open(PG).await
}

fn other(ty: &str, text: &str) -> Value {
    Value::Other { type_name: ty.into(), text: text.into() }
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn every_value_type() {
    let mut s = open_pg().await;
    run(&mut s, "CREATE TYPE pg_temp.siesta_mood AS ENUM ('sad', 'happy')").await;
    let sql = "SELECT 1::int2 a_i2, 2::int4 a_i4, 3::int8 a_i8, 1.50::numeric(10,2) a_num, 1.5::float4 a_f4, 2.25::float8 a_f8, \
        true a_bool, 'héllo'::text a_text, 'v'::varchar(5) a_var,'{\"a\":[1,2]}'::json a_json, '{\"b\": null}'::jsonb a_jsonb, \
        '\\xdeadbeef'::bytea a_bytea, '2026-01-02'::date a_date, '2026-01-02 03:04:05'::timestamp a_ts, \
        '2026-01-02 03:04:05+00'::timestamptz a_tstz, '03:04:05'::time a_time, '550e8400-e29b-41d4-a716-446655440000'::uuid a_uuid, \
        ARRAY[1,2,NULL]::int4[] a_arr, ARRAY['a','b \"c\"']::text[] a_tarr, ARRAY[[1,2],[3,4]] a_arr2, \
        'happy'::pg_temp.siesta_mood a_enum, ARRAY['sad']::pg_temp.siesta_mood[] a_enum_arr, interval '1 day 2 hours' a_interval, \
        '10.0.0.1/8'::inet a_inet, NULL::int a_null, '1'::\"char\" a_qchar";
    let o = run(&mut s, sql).await;
    let g = |n: &str| col(&o, n);
    assert_eq!(g("a_i2"), ("int2", &Value::Int(1)));
    assert_eq!(g("a_i4"), ("int4", &Value::Int(2)));
    assert_eq!(g("a_i8"), ("int8", &Value::Int(3)));
    assert_eq!(g("a_num"), ("numeric", &Value::Decimal("1.50".into())));
    assert_eq!(g("a_f4"), ("float4", &Value::Float(1.5)));
    assert_eq!(g("a_f8"), ("float8", &Value::Float(2.25)));
    assert_eq!(g("a_bool"), ("bool", &Value::Bool(true)));
    assert_eq!(g("a_text"), ("text", &Value::Text("héllo".into())));
    assert_eq!(g("a_var"), ("varchar", &Value::Text("v".into())));
    // bpchar keeps its padding unfiltered; the filtered path's `::text` cast trims it.
    assert_eq!(col(&run(&mut s, "SELECT 'ab'::char(4) c").await, "c"), ("char", &Value::Text("ab  ".into())));
    assert_eq!(g("a_json"), ("json", &Value::Json(json!({"a": [1, 2]}))));
    assert_eq!(g("a_jsonb"), ("jsonb", &Value::Json(json!({"b": null}))));
    assert_eq!(g("a_bytea"), ("bytea", &Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef])));
    assert_eq!(g("a_date"), ("date", &Value::DateTime("2026-01-02".into())));
    assert_eq!(g("a_ts"), ("timestamp", &Value::DateTime("2026-01-02 03:04:05".into())));
    assert_eq!(g("a_tstz").0, "timestamptz");
    assert!(matches!(g("a_tstz").1, Value::DateTime(t) if t.starts_with("2026-01-02 03:04:05")));
    assert_eq!(g("a_time"), ("time", &Value::DateTime("03:04:05".into())));
    assert_eq!(g("a_uuid"), ("uuid", &Value::Text("550e8400-e29b-41d4-a716-446655440000".into())));
    assert_eq!(g("a_arr"), ("int4[]", &Value::Json(json!([1, 2, null]))));
    assert_eq!(g("a_tarr"), ("text[]", &Value::Json(json!(["a", "b \"c\""]))));
    assert_eq!(g("a_arr2"), ("int4[]", &Value::Json(json!([[1, 2], [3, 4]]))));
    assert_eq!(g("a_enum"), ("siesta_mood", &other("siesta_mood", "happy")));
    assert_eq!(g("a_enum_arr").1, &Value::Json(json!(["sad"])));
    assert_eq!(g("a_interval"), ("interval", &other("interval", "1 day 02:00:00")));
    assert_eq!(g("a_inet"), ("inet", &other("inet", "10.0.0.1/8")));
    assert_eq!(g("a_null"), ("int4", &Value::Null));
    assert!(matches!(g("a_qchar").1, Value::Text(_) | Value::Other { .. }));
    assert!(o.executed_sql.starts_with("SELECT * FROM (SELECT 1::int2"));
    // The same row through the filtered (extended protocol) path decodes identically.
    let filtered = run_page(&mut s, sql, &page(&[("a_i8", "=3"), ("a_enum", "happy"), ("a_inet", "10.0")])).await;
    assert_eq!(filtered.rows, o.rows);
    assert!(!filtered.params.is_empty());
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn paging_has_more() {
    let mut s = open_pg().await;
    let sql = "SELECT g AS n FROM generate_series(1, 25) g";
    let p = |offset, size| Page { offset, size, ..Page::default() };
    let o = run_page(&mut s, sql, &p(0, 10)).await;
    assert_eq!(ints(&o), (1..=10).collect::<Vec<_>>());
    assert!(o.has_more && !o.truncated);
    assert!(o.executed_sql.contains("LIMIT 11 OFFSET 0"), "{}", o.executed_sql);
    let o = run_page(&mut s, sql, &p(20, 10)).await;
    assert_eq!(ints(&o), vec![21, 22, 23, 24, 25]);
    assert!(!o.has_more);
    let o = run_page(&mut s, sql, &p(0, 25)).await;
    assert_eq!((o.rows.len(), o.has_more), (25, false));
    // An inner LIMIT and a trailing `;` / comment survive the wrapper.
    let o = run_page(&mut s, "SELECT g FROM generate_series(1, 25) g LIMIT 12 -- c", &p(10, 10)).await;
    assert_eq!((o.rows.len(), o.has_more), (2, false));
    let (sql, kind) = ("SELECT 1;", Kind::Query);
    assert_eq!(s.run(sql, kind, &Page::default()).await.unwrap().rows.len(), 1);
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn empty_result_keeps_columns() {
    let mut s = open_pg().await;
    let o = run(&mut s, "SELECT 1 AS x, 'a'::text AS y WHERE false").await;
    assert!(o.rows.is_empty());
    assert_eq!(o.columns.iter().map(|c| (c.name.as_str(), c.type_name.as_str())).collect::<Vec<_>>(), [("x", "int4"), ("y", "text")]);
}

async fn people(s: &mut Session) {
    run(s, "CREATE TEMP TABLE people (id int, name text, doc jsonb, born date, score numeric, ok bool, tag text, \"we\"\"ird\" int)").await;
    run(
        s,
        "INSERT INTO people VALUES \
         (1, 'alice', '{\"k\": \"x\"}', '2025-05-05', 1.5, true, 'a', 10), \
         (9, 'Bob 50%', '{\"k\": \"y\"}', '2026-01-01', 9, false, NULL, 20), \
         (10, 'carol_x', '{}', '2026-06-06', 10.25, true, 'b', 30), \
         (100, 'dave', NULL, NULL, NULL, NULL, 'c', 40)",
    )
    .await;
}

async fn ids(s: &mut Session, filters: &[(&str, &str)]) -> Vec<i64> {
    let mut p = page(filters);
    p.sort = vec![("id".into(), false)];
    ints(&run_page(s, "SELECT * FROM people", &p).await)
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn every_filter_form() {
    let mut s = open_pg().await;
    people(&mut s).await;
    assert_eq!(ids(&mut s, &[("name", "ALI")]).await, [1]);
    assert_eq!(ids(&mut s, &[("name", "50%")]).await, [9]); // literal %
    assert_eq!(ids(&mut s, &[("name", "%")]).await, [9]);
    assert_eq!(ids(&mut s, &[("name", "_")]).await, [10]);
    assert_eq!(ids(&mut s, &[("id", "=9")]).await, [9]);
    assert_eq!(ids(&mut s, &[("id", "!=9")]).await, [1, 10, 100]);
    assert_eq!(ids(&mut s, &[("id", ">9")]).await, [10, 100]); // numeric, not text, ordering
    assert_eq!(ids(&mut s, &[("id", ">=9")]).await, [9, 10, 100]);
    assert_eq!(ids(&mut s, &[("id", "<10")]).await, [1, 9]);
    assert_eq!(ids(&mut s, &[("id", "<=10")]).await, [1, 9, 10]);
    assert_eq!(ids(&mut s, &[("born", "<=2026-01-01")]).await, [1, 9]);
    assert_eq!(ids(&mut s, &[("score", ">9.5")]).await, [10]);
    assert_eq!(ids(&mut s, &[("ok", "=true")]).await, [1, 10]);
    assert_eq!(ids(&mut s, &[("tag", "null")]).await, [9]);
    assert_eq!(ids(&mut s, &[("tag", "!null")]).await, [1, 10, 100]);
    assert_eq!(ids(&mut s, &[("id", "1, 10")]).await, [1, 10]);
    assert_eq!(ids(&mut s, &[("name", "alice,dave")]).await, [1, 100]);
    assert_eq!(ids(&mut s, &[("doc", "\"y\"")]).await, [9]); // json compared as text
    assert_eq!(ids(&mut s, &[("doc", "={}")]).await, [10]);
    assert_eq!(ids(&mut s, &[("we\"ird", ">=30")]).await, [10, 100]);
    assert_eq!(ids(&mut s, &[("id", ">1"), ("name", "o")]).await, [9, 10]);
    assert_eq!(ids(&mut s, &[("id", "")]).await, [1, 9, 10, 100]);
    // Values are parameters: this is just a string that matches nothing.
    assert!(ids(&mut s, &[("name", "'; DROP TABLE people; --")]).await.is_empty());
    assert!(ids(&mut s, &[("name", "=x' OR '1'='1")]).await.is_empty());
    assert_eq!(ids(&mut s, &[]).await.len(), 4);
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn bad_filter_value_is_a_server_error() {
    let mut s = open_pg().await;
    people(&mut s).await;
    let e = try_run_page(&mut s, "SELECT id FROM people", &page(&[("id", ">abc")])).await.unwrap_err();
    assert_eq!(e.code.as_deref(), Some("22P02"));
    // ...and the session survives.
    assert_eq!(ints(&run(&mut s, "SELECT 1").await), [1]);
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn sort_multi_column_and_count() {
    let mut s = open_pg().await;
    let sql = "SELECT g % 3 AS m, g AS n FROM generate_series(1, 9) g";
    let o = run_page(&mut s, sql, &sorted(&[("m", false), ("n", true)])).await;
    let rows: Vec<(i64, i64)> = o.rows.iter().map(|r| (r[0].display().parse().unwrap(), r[1].display().parse().unwrap())).collect();
    assert_eq!(rows, [(0, 9), (0, 6), (0, 3), (1, 7), (1, 4), (1, 1), (2, 8), (2, 5), (2, 2)]);
    assert!(o.executed_sql.contains("ORDER BY q.\"m\", q.\"n\" DESC"));
    // Sort works together with typed filters (outer text casts must not change the ordering type).
    let mut p = sorted(&[("n", true)]);
    p.filters = vec![("m".into(), "=1".into())];
    assert_eq!(ints(&run_page(&mut s, "SELECT g AS n, g % 3 AS m FROM generate_series(7, 12) g", &p).await), [10, 7]);
    let o = run_page(&mut s, "SELECT g AS n FROM generate_series(1, 100) g", &sorted(&[("n", true)])).await;
    assert_eq!(ints(&o)[..2], [100, 99]);
    // 9 ordered as text would come after 10.
    let o = run_page(&mut s, "SELECT g AS n FROM generate_series(1, 12) g", &Page { sort: vec![("n".into(), false)], filters: vec![("n".into(), ">=9".into())], ..Page::default() }).await;
    assert_eq!(ints(&o), [9, 10, 11, 12]);

    assert_eq!(s.count(sql, &[]).await.unwrap(), 9);
    assert_eq!(s.count("SELECT g AS n FROM generate_series(1, 9) g;", &[("n".into(), ">=7".into())]).await.unwrap(), 3);
    assert_eq!(s.count("SELECT g AS n FROM generate_series(1, 9) g", &[("n".into(), "null".into())]).await.unwrap(), 0);
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn dml_rows_affected_and_returning() {
    let mut s = open_pg().await;
    let o = run(&mut s, "CREATE TEMP TABLE d (id serial, v int)").await;
    assert_eq!((o.rows_affected, o.rows.len()), (Some(0), 0));
    let o = run(&mut s, "INSERT INTO d (v) SELECT g FROM generate_series(1, 5) g").await;
    assert_eq!(o.rows_affected, Some(5));
    assert!(!o.has_more);
    let o = run(&mut s, "UPDATE d SET v = v * 10 WHERE id <= 3 RETURNING id, v").await;
    assert_eq!(o.rows_affected, Some(3));
    assert_eq!(o.rows.len(), 3);
    assert_eq!(o.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "v"]);
    assert_eq!(o.rows[0], [Value::Int(1), Value::Int(10)]);
    let o = run(&mut s, "UPDATE d SET v = 0 WHERE id > 100 RETURNING id").await;
    assert_eq!((o.rows_affected, o.rows.len()), (Some(0), 0));
    assert_eq!(o.columns.len(), 1); // columns even without rows
    assert_eq!(run(&mut s, "DELETE FROM d").await.rows_affected, Some(5));
    // A script of several statements is one run.
    let o = s.run("INSERT INTO d (v) VALUES (1); INSERT INTO d (v) VALUES (2), (3)", Kind::Other, &Page::default()).await.unwrap();
    assert_eq!(o.rows_affected, Some(3));
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn show_explain_and_truncation() {
    let mut s = open_pg().await;
    let o = run(&mut s, "SHOW server_version").await;
    assert_eq!(o.rows.len(), 1);
    assert_eq!(o.columns[0].name, "server_version");
    assert_eq!(o.rows_affected, None);
    let o = run(&mut s, "EXPLAIN SELECT 1").await;
    assert!(o.rows[0][0].display().contains("Result"), "{:?}", o.rows);
    assert!(o.executed_sql.starts_with("EXPLAIN"));

    s.max_rows = 5;
    let o = run(&mut s, "SHOW ALL").await;
    assert_eq!(o.rows.len(), 5);
    assert!(o.truncated);
    // The session is usable right after a cut-off stream.
    assert_eq!(ints(&run(&mut s, "SELECT 7").await), [7]);
    s.max_rows = 10_000;
    let o = run(&mut s, "SHOW ALL").await;
    assert!(!o.truncated && o.rows.len() > 5);
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn session_state_and_transactions() {
    let mut s = open_pg().await;
    run(&mut s, "SET application_name = 'siesta_state'").await;
    run(&mut s, "CREATE TEMP TABLE keep (a int)").await;
    run(&mut s, "INSERT INTO keep VALUES (1)").await;
    assert_eq!(texts(&run(&mut s, "SHOW application_name").await), ["siesta_state"]);
    assert_eq!(ints(&run(&mut s, "SELECT count(*) FROM keep").await), [1]);

    assert!(!s.in_transaction());
    assert!(run(&mut s, "BEGIN").await.in_transaction);
    assert!(s.in_transaction());
    run(&mut s, "INSERT INTO keep VALUES (2)").await;
    let o = run(&mut s, "SELECT count(*) FROM keep").await;
    assert!(o.in_transaction);
    assert_eq!(ints(&o), [2]);
    // An error leaves the transaction open (aborted) until ROLLBACK.
    assert!(try_run(&mut s, "SELECT nope").await.is_err());
    assert!(s.in_transaction());
    assert!(!run(&mut s, "ROLLBACK").await.in_transaction);
    assert_eq!(ints(&run(&mut s, "SELECT count(*) FROM keep").await), [1]);

    run(&mut s, "START TRANSACTION").await;
    run(&mut s, "INSERT INTO keep VALUES (3)").await;
    assert!(!run(&mut s, "COMMIT").await.in_transaction);
    assert_eq!(ints(&run(&mut s, "SELECT count(*) FROM keep").await), [2]);
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn read_only_schema_and_init_sql() {
    let mut c = connection(PG);
    c.read_only = true;
    c.schema = Some("pg_catalog, public".into());
    c.init_sql = Some("SET application_name = 'from_init'; SET extra_float_digits = 0;".into());
    let mut s = Session::open(&c).await.unwrap();
    assert_eq!(texts(&run(&mut s, "SHOW application_name").await), ["from_init"]);
    assert_eq!(texts(&run(&mut s, "SHOW search_path").await), ["pg_catalog, public"]);
    assert_eq!(texts(&run(&mut s, "SHOW default_transaction_read_only").await), ["on"]);
    let e = try_run(&mut s, "CREATE TABLE ro_denied (a int)").await.unwrap_err();
    assert_eq!(e.code.as_deref(), Some("25006"), "{e:?}");
    assert_eq!(ints(&run(&mut s, "SELECT 1").await), [1]);
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn error_position_is_mapped_through_the_wrapper() {
    let mut s = open_pg().await;
    let e = try_run(&mut s, "SELECT * FROM nope_missing;").await.unwrap_err();
    assert_eq!(e.code.as_deref(), Some("42P01"));
    assert!(e.message.contains("nope_missing"));
    assert_eq!(e.position, Some(15));
    // Chars, not bytes.
    let e = try_run(&mut s, "SELECT 'é' FROM nope_missing").await.unwrap_err();
    assert_eq!(e.position, Some(17));
    // Also behind a filter wrapper (longer prefix).
    let e = try_run_page(&mut s, "SELECT 1 AS a FROM nope_missing", &page(&[("a", "=1")])).await;
    let e = e.unwrap_err();
    assert_eq!((e.code.as_deref(), e.position), (Some("42P01"), Some(20)));
    // Unwrapped statements keep the server's position.
    let e = try_run(&mut s, "SHOW nope_missing").await.unwrap_err();
    assert_eq!(e.code.as_deref(), Some("42704"));
    // An error in the wrapper part has no position in the user's text.
    let e = try_run_page(&mut s, "SELECT 1 AS a", &sorted(&[("zz", false)])).await.unwrap_err();
    assert_eq!((e.code.as_deref(), e.position), (Some("42703"), None));
    // Hint and detail come through.
    run(&mut s, "CREATE TEMP TABLE u (a int PRIMARY KEY)").await;
    run(&mut s, "INSERT INTO u VALUES (1)").await;
    let e = try_run(&mut s, "INSERT INTO u VALUES (1)").await.unwrap_err();
    assert_eq!(e.code.as_deref(), Some("23505"));
    assert!(e.detail.as_deref().unwrap_or("").contains("already exists"), "{e:?}");
    let e = try_run(&mut s, "SELECT * FROM generate_series(1, 2) g ORDER BY nope(1").await.unwrap_err();
    assert_eq!(e.code.as_deref(), Some("42601"));
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn cancel_returns_quickly_and_session_survives() {
    let mut s = open_pg().await;
    run(&mut s, "SET application_name = 'survivor'").await;
    let handle = s.cancel_handle();
    let started = Instant::now();
    let (res, cancelled) = tokio::join!(try_run(&mut s, "SELECT pg_sleep(10)"), async {
        tokio::time::sleep(Duration::from_millis(400)).await;
        handle.cancel().await
    });
    cancelled.unwrap();
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    assert_eq!(res.unwrap_err().code.as_deref(), Some("57014"));
    // Same session: state is still there.
    assert_eq!(texts(&run(&mut s, "SHOW application_name").await), ["survivor"]);
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn reconnects_after_the_backend_is_killed() {
    let mut s = open_pg().await;
    run(&mut s, "SET application_name = 'before_kill'").await;
    let old = s.backend_id();
    let mut other = open_pg().await;
    run(&mut other, &format!("SELECT pg_terminate_backend({old})")).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let o = run(&mut s, "SHOW application_name").await;
    assert!(o.notices.iter().any(|n| n.contains("lost")), "{:?}", o.notices);
    assert_eq!(texts(&o), ["siesta"]); // fresh connection: session state is gone
    assert_ne!(s.backend_id(), old);
    assert!(run(&mut s, "SELECT 1").await.notices.is_empty());
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn lost_connection_does_not_rerun_dml() {
    let mut s = open_pg().await;
    let old = s.backend_id();
    let mut other = open_pg().await;
    run(&mut other, &format!("SELECT pg_terminate_backend({old})")).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let e = try_run(&mut s, "CREATE TABLE IF NOT EXISTS siesta_never_rerun (a int)").await.unwrap_err();
    assert!(e.connection_lost && e.message.contains("may or may not have run"), "{e:?}");
    assert_ne!(s.backend_id(), old);
    assert_eq!(ints(&run(&mut s, "SELECT 1").await), [1]); // session is usable
    let o = run(&mut s, "SELECT to_regclass('siesta_never_rerun')::text").await;
    assert_eq!(o.rows[0][0], Value::Null); // the statement was not rerun
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn bad_credentials_are_an_error_not_a_panic() {
    let mut c = connection(PG);
    c.password = Some("wrong".into());
    let e = Session::open(&c).await.err().expect("must fail");
    assert!(e.code.is_some() || !e.message.is_empty());
}
