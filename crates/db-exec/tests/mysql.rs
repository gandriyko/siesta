//! Needs a running MariaDB / MySQL: `scripts/test-db.sh`.
mod common;

use std::time::{Duration, Instant};

use common::*;
use db_conn::Driver;
use db_exec::{Page, Session, Value};
use sql_split::Kind;

const MY: Driver = Driver::MySql;

async fn open_my() -> Session {
    open(MY).await
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn every_value_type() {
    let mut s = open_my().await;
    run(
        &mut s,
        "CREATE TEMPORARY TABLE vt (id INT, a_tiny TINYINT, a_small SMALLINT, a_int INT, a_big BIGINT, a_ubig BIGINT UNSIGNED, \
         a_dec DECIMAL(10,2), a_bigdec DECIMAL(40,10), a_float FLOAT, a_double DOUBLE, a_bool BOOLEAN, a_varchar VARCHAR(10), a_text TEXT, \
         a_json JSON, a_blob BLOB, a_varbin VARBINARY(8), a_date DATE, a_dt DATETIME(3), a_ts TIMESTAMP NULL, a_time TIME, a_year YEAR, \
         a_enum ENUM('x','y'), a_set SET('p','q'), a_bit BIT(8), a_null INT)",
    )
    .await;
    run(
        &mut s,
        "INSERT INTO vt VALUES (1, -1, -2, -3, -4, 18446744073709551615, 1.50, 12345678901234567890123456789.1234567890, 0.1, 2.5, true, \
         'héllo', 'long text', '{\"a\": [1, 2]}', 0xDEADBEEF, 0x0102, '2026-01-02', '2026-01-02 03:04:05.123', '2026-01-02 03:04:05', '03:04:05', 2026, \
         'y', 'p,q', b'101', NULL)",
    )
    .await;
    let o = run(&mut s, "SELECT * FROM vt").await;
    let g = |n: &str| col(&o, n);
    assert_eq!(g("a_tiny"), ("TINYINT", &Value::Int(-1)));
    assert_eq!(g("a_small"), ("SMALLINT", &Value::Int(-2)));
    assert_eq!(g("a_int"), ("INT", &Value::Int(-3)));
    assert_eq!(g("a_big"), ("BIGINT", &Value::Int(-4)));
    assert_eq!(g("a_ubig"), ("BIGINT UNSIGNED", &Value::Decimal("18446744073709551615".into())));
    assert_eq!(g("a_dec"), ("DECIMAL", &Value::Decimal("1.50".into())));
    assert_eq!(g("a_bigdec").1, &Value::Decimal("12345678901234567890123456789.1234567890".into()));
    assert_eq!(g("a_float"), ("FLOAT", &Value::Float(0.1)));
    assert_eq!(g("a_double"), ("DOUBLE", &Value::Float(2.5)));
    assert_eq!(g("a_bool"), ("BOOLEAN", &Value::Bool(true)));
    assert_eq!(g("a_varchar"), ("VARCHAR", &Value::Text("héllo".into())));
    assert_eq!(g("a_text"), ("TEXT", &Value::Text("long text".into())));
    // MySQL reports a real JSON type; MariaDB's JSON is a utf8mb4_bin LONGTEXT that arrives as BLOB.
    match g("a_json") {
        ("JSON", Value::Json(j)) => assert_eq!(j, &serde_json::json!({"a": [1, 2]})),
        ("BLOB", Value::Bytes(b)) => assert_eq!(b, b"{\"a\": [1, 2]}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(g("a_blob"), ("BLOB", &Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef])));
    assert_eq!(g("a_varbin"), ("VARBINARY", &Value::Bytes(vec![1, 2])));
    assert_eq!(g("a_date"), ("DATE", &Value::DateTime("2026-01-02".into())));
    assert_eq!(g("a_dt"), ("DATETIME", &Value::DateTime("2026-01-02 03:04:05.123".into())));
    assert_eq!(g("a_ts"), ("TIMESTAMP", &Value::DateTime("2026-01-02 03:04:05".into())));
    assert_eq!(g("a_time"), ("TIME", &Value::DateTime("03:04:05".into())));
    assert_eq!(g("a_year"), ("YEAR", &Value::Int(2026)));
    assert_eq!(g("a_enum").1, &Value::Text("y".into()));
    assert_eq!(g("a_set").1, &Value::Text("p,q".into()));
    assert_eq!(g("a_bit"), ("BIT", &Value::Int(5)));
    assert_eq!(g("a_null"), ("INT", &Value::Null));
    assert!(o.executed_sql.starts_with("SELECT * FROM vt\nLIMIT 101"), "{}", o.executed_sql);
    // The same row through the filtered (prepared, binary protocol) path decodes identically.
    let filtered = run_page(&mut s, "SELECT * FROM vt", &page(&[("id", "=1"), ("a_varchar", "ELL")])).await;
    assert_eq!(filtered.rows, o.rows);
    assert!(!filtered.params.is_empty());
    let o = run(&mut s, "SELECT CAST('-100:59:59.5' AS TIME(3)) AS t").await;
    assert_eq!(o.rows[0][0], Value::DateTime("-100:59:59.500000".into()));
    // Out-of-range / zero dates and geometry never fail.
    let o = run(&mut s, "SELECT ST_GeomFromText('POINT(1 2)') AS pt, CAST('2026-01-02' AS DATE) AS d, 1 = 1 AS b").await;
    assert!(matches!(col(&o, "pt").1, Value::Other { .. } | Value::Bytes(_)), "{:?}", o.rows);
    assert_eq!(col(&o, "d").1, &Value::DateTime("2026-01-02".into()));
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn paging_has_more_and_empty_columns() {
    let mut s = open_my().await;
    let sql = "SELECT seq AS n FROM seq_1_to_25";
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
    let o = run_page(&mut s, "SELECT seq FROM seq_1_to_25 LIMIT 12 -- c", &p(10, 10)).await;
    assert_eq!((o.rows.len(), o.has_more), (2, false));
    let o = run(&mut s, "SELECT 1 AS x, 'a' AS y FROM dual WHERE 1 = 0").await;
    assert!(o.rows.is_empty());
    assert_eq!(o.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["x", "y"]);
}

async fn people(s: &mut Session) {
    run(s, "CREATE TEMPORARY TABLE people (id INT, name VARCHAR(50), doc TEXT, born DATE, score DECIMAL(10,2), ok BOOLEAN, tag VARCHAR(5), `we``ird` INT)").await;
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
    let mut s = open_my().await;
    people(&mut s).await;
    assert_eq!(ids(&mut s, &[("name", "ALI")]).await, [1]);
    assert_eq!(ids(&mut s, &[("name", "50%")]).await, [9]);
    assert_eq!(ids(&mut s, &[("name", "%")]).await, [9]);
    assert_eq!(ids(&mut s, &[("name", "_")]).await, [10]);
    assert_eq!(ids(&mut s, &[("id", "=9")]).await, [9]);
    assert_eq!(ids(&mut s, &[("id", "!=9")]).await, [1, 10, 100]);
    assert_eq!(ids(&mut s, &[("id", ">9")]).await, [10, 100]);
    assert_eq!(ids(&mut s, &[("id", ">=9")]).await, [9, 10, 100]);
    assert_eq!(ids(&mut s, &[("id", "<10")]).await, [1, 9]);
    assert_eq!(ids(&mut s, &[("id", "<=10")]).await, [1, 9, 10]);
    assert_eq!(ids(&mut s, &[("born", "<=2026-01-01")]).await, [1, 9]);
    assert_eq!(ids(&mut s, &[("score", ">9.5")]).await, [10]);
    assert_eq!(ids(&mut s, &[("ok", "=1")]).await, [1, 10]);
    assert_eq!(ids(&mut s, &[("tag", "null")]).await, [9]);
    assert_eq!(ids(&mut s, &[("tag", "!null")]).await, [1, 10, 100]);
    assert_eq!(ids(&mut s, &[("id", "1, 10")]).await, [1, 10]);
    assert_eq!(ids(&mut s, &[("name", "alice,dave")]).await, [1, 100]);
    assert_eq!(ids(&mut s, &[("doc", "\"y\"")]).await, [9]);
    assert_eq!(ids(&mut s, &[("doc", "={}")]).await, [10]);
    assert_eq!(ids(&mut s, &[("we`ird", ">=30")]).await, [10, 100]);
    assert_eq!(ids(&mut s, &[("id", ">1"), ("name", "o")]).await, [9, 10]);
    assert_eq!(ids(&mut s, &[("id", "")]).await, [1, 9, 10, 100]);
    assert!(ids(&mut s, &[("name", "'; DROP TABLE people; --")]).await.is_empty());
    assert!(ids(&mut s, &[("name", "=x' OR '1'='1")]).await.is_empty());
    assert_eq!(ids(&mut s, &[]).await.len(), 4);
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn sort_multi_column_and_count() {
    let mut s = open_my().await;
    // (MariaDB's sequence engine returns nothing for ORDER BY + LIMIT in a derived table, so use a real table.)
    run(&mut s, "CREATE TEMPORARY TABLE nums (n INT)").await;
    run(&mut s, "INSERT INTO nums SELECT seq FROM seq_1_to_25").await;
    let sql = "SELECT n % 3 AS m, n FROM nums WHERE n <= 9";
    let o = run_page(&mut s, sql, &sorted(&[("m", false), ("n", true)])).await;
    let rows: Vec<(i64, i64)> = o.rows.iter().map(|r| (r[0].display().parse().unwrap(), r[1].display().parse().unwrap())).collect();
    assert_eq!(rows, [(0, 9), (0, 6), (0, 3), (1, 7), (1, 4), (1, 1), (2, 8), (2, 5), (2, 2)]);
    assert!(o.executed_sql.contains("ORDER BY q.`m`, q.`n` DESC"));
    let mut p = sorted(&[("n", true)]);
    p.filters = vec![("m".into(), "=1".into())];
    assert_eq!(ints(&run_page(&mut s, "SELECT n, n % 3 AS m FROM nums WHERE n BETWEEN 7 AND 12", &p).await), [10, 7]);
    let o = run_page(&mut s, "SELECT n FROM nums WHERE n <= 12", &Page { sort: vec![("n".into(), false)], filters: vec![("n".into(), ">=9".into())], ..Page::default() }).await;
    assert_eq!(ints(&o), [9, 10, 11, 12]);

    assert_eq!(s.count(sql, &[]).await.unwrap(), 9);
    assert_eq!(s.count("SELECT n FROM nums WHERE n <= 9;", &[("n".into(), ">=7".into())]).await.unwrap(), 3);
    assert_eq!(s.count("SELECT n FROM nums WHERE n <= 9", &[("n".into(), "null".into())]).await.unwrap(), 0);
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn dml_rows_affected_and_returning() {
    let mut s = open_my().await;
    let o = run(&mut s, "CREATE TEMPORARY TABLE d (id INT AUTO_INCREMENT PRIMARY KEY, v INT)").await;
    assert_eq!((o.rows_affected, o.rows.len()), (Some(0), 0));
    let o = run(&mut s, "INSERT INTO d (v) SELECT seq FROM seq_1_to_5").await;
    assert_eq!(o.rows_affected, Some(5));
    let o = run(&mut s, "UPDATE d SET v = v * 10 WHERE id <= 3").await;
    assert_eq!(o.rows_affected, Some(3));
    let o = run(&mut s, "DELETE FROM d WHERE id <= 2 RETURNING id, v").await; // MariaDB
    assert_eq!(o.rows_affected, Some(2));
    assert_eq!(o.rows, [[Value::Int(1), Value::Int(10)], [Value::Int(2), Value::Int(20)]]);
    let o = run(&mut s, "DELETE FROM d WHERE id > 100 RETURNING id").await;
    assert_eq!((o.rows_affected, o.rows.len()), (Some(0), 0));
    // (MariaDB's prepare reports no columns for DELETE … RETURNING, so none without rows.)
    assert_eq!(run(&mut s, "DELETE FROM d").await.rows_affected, Some(3));
    let o = s.run("INSERT INTO d (v) VALUES (1); INSERT INTO d (v) VALUES (2), (3)", Kind::Other, &Page::default()).await.unwrap();
    assert_eq!(o.rows_affected, Some(3));
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn show_explain_and_truncation() {
    let mut s = open_my().await;
    let o = run(&mut s, "SHOW VARIABLES LIKE 'version'").await;
    assert_eq!(o.rows.len(), 1);
    assert_eq!(o.columns[0].name, "Variable_name");
    assert_eq!(o.rows_affected, None);
    let o = run(&mut s, "EXPLAIN SELECT 1").await;
    assert_eq!(o.rows.len(), 1);
    assert!(o.executed_sql.starts_with("EXPLAIN"));

    s.max_rows = 5;
    let o = run(&mut s, "SHOW VARIABLES").await;
    assert_eq!(o.rows.len(), 5);
    assert!(o.truncated);
    assert_eq!(ints(&run(&mut s, "SELECT 7").await), [7]);
    s.max_rows = 10_000;
    let o = run(&mut s, "SHOW VARIABLES").await;
    assert!(!o.truncated && o.rows.len() > 5);
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn session_state_and_transactions() {
    let mut s = open_my().await;
    run(&mut s, "SET @siesta_state = 42").await;
    run(&mut s, "DROP TABLE IF EXISTS siesta_tx_test").await;
    run(&mut s, "CREATE TABLE siesta_tx_test (a INT) ENGINE=InnoDB").await;
    run(&mut s, "CREATE TEMPORARY TABLE keep (a INT)").await;
    run(&mut s, "INSERT INTO keep VALUES (1)").await;
    assert_eq!(ints(&run(&mut s, "SELECT @siesta_state").await), [42]);
    assert_eq!(ints(&run(&mut s, "SELECT count(*) FROM keep").await), [1]);

    assert!(!s.in_transaction());
    assert!(run(&mut s, "BEGIN").await.in_transaction);
    run(&mut s, "INSERT INTO siesta_tx_test VALUES (1)").await;
    let o = run(&mut s, "SELECT count(*) FROM siesta_tx_test").await;
    assert!(o.in_transaction);
    assert_eq!(ints(&o), [1]);
    assert!(!run(&mut s, "ROLLBACK").await.in_transaction);
    assert_eq!(ints(&run(&mut s, "SELECT count(*) FROM siesta_tx_test").await), [0]);

    run(&mut s, "START TRANSACTION").await;
    run(&mut s, "INSERT INTO siesta_tx_test VALUES (2)").await;
    assert!(!run(&mut s, "COMMIT").await.in_transaction);
    assert_eq!(ints(&run(&mut s, "SELECT count(*) FROM siesta_tx_test").await), [1]);
    run(&mut s, "DROP TABLE siesta_tx_test").await;
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn read_only_and_init_sql() {
    let mut s = open_my().await;
    run(&mut s, "DROP TABLE IF EXISTS siesta_ro_test").await;
    run(&mut s, "CREATE TABLE siesta_ro_test (a INT)").await;

    let mut c = connection(MY);
    c.read_only = true;
    c.init_sql = Some("SET @from_init = 7; SET @second = 8;".into());
    let mut ro = Session::open(&c).await.unwrap();
    assert_eq!(ints(&run(&mut ro, "SELECT @from_init + @second").await), [15]);
    let e = try_run(&mut ro, "INSERT INTO siesta_ro_test VALUES (1)").await.unwrap_err();
    assert_eq!(e.code.as_deref(), Some("25006"), "{e:?}");
    assert_eq!(ints(&run(&mut ro, "SELECT 1").await), [1]);
    run(&mut s, "DROP TABLE siesta_ro_test").await;
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn errors_and_warnings() {
    let mut s = open_my().await;
    let e = try_run(&mut s, "SELECT * FROM nope_missing;").await.unwrap_err();
    assert_eq!(e.code.as_deref(), Some("42S02"));
    assert!(e.message.contains("nope_missing"));
    assert_eq!(e.position, None);
    let e = try_run(&mut s, "SELEC 1").await.unwrap_err();
    assert_eq!(e.code.as_deref(), Some("42000"));
    assert!(!e.connection_lost);
    let o = run(&mut s, "DROP TABLE IF EXISTS siesta_no_such_table").await;
    assert!(o.notices.iter().any(|n| n.contains("1051") || n.contains("siesta_no_such_table")), "{:?}", o.notices);
    assert!(run(&mut s, "SELECT 1").await.notices.is_empty());
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn cancel_returns_quickly_and_session_survives() {
    let mut s = open_my().await;
    run(&mut s, "SET @survivor = 'yes'").await;
    let handle = s.cancel_handle();
    let started = Instant::now();
    let (res, cancelled) = tokio::join!(try_run(&mut s, "SELECT SLEEP(10)"), async {
        tokio::time::sleep(Duration::from_millis(400)).await;
        handle.cancel().await
    });
    cancelled.unwrap();
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    // KILL QUERY makes SLEEP return 1 (or the statement fail with 70100); either way it ended.
    if let Err(e) = res {
        assert_eq!(e.code.as_deref(), Some("70100"));
    }
    assert_eq!(texts(&run(&mut s, "SELECT @survivor").await), ["yes"]);
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn reconnects_after_the_connection_is_killed() {
    let mut s = open_my().await;
    run(&mut s, "SET @before_kill = 1").await;
    let old = s.backend_id();
    let mut other = open_my().await;
    run(&mut other, &format!("KILL CONNECTION {old}")).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let o = run(&mut s, "SELECT @before_kill").await;
    assert!(o.notices.iter().any(|n| n.contains("lost")), "{:?}", o.notices);
    assert_eq!(o.rows[0][0], Value::Null); // fresh connection: user variable is gone
    assert_ne!(s.backend_id(), old);
    assert!(run(&mut s, "SELECT 1").await.notices.is_empty());
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn lost_connection_does_not_rerun_dml() {
    let mut s = open_my().await;
    let old = s.backend_id();
    let mut other = open_my().await;
    run(&mut other, &format!("KILL CONNECTION {old}")).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let e = try_run(&mut s, "CREATE TABLE IF NOT EXISTS siesta_never_rerun (a INT)").await.unwrap_err();
    assert!(e.connection_lost && e.message.contains("may or may not have run"), "{e:?}");
    assert_ne!(s.backend_id(), old);
    assert_eq!(ints(&run(&mut s, "SELECT 1").await), [1]);
    let o = run(&mut s, "SELECT count(*) FROM information_schema.tables WHERE table_name = 'siesta_never_rerun'").await;
    assert_eq!(ints(&o), [0]);
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn bad_credentials_are_an_error_not_a_panic() {
    let mut c = connection(MY);
    c.password = Some("wrong".into());
    let e = Session::open(&c).await.err().expect("must fail");
    assert!(!e.message.is_empty());
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn repeated_column_names_run_unwrapped() {
    let mut s = open_my().await;
    let join = "SELECT * FROM (SELECT 1 AS id, 'a' AS x) a JOIN (SELECT 1 AS id, 'b' AS y) b ON a.id = b.id";
    // A plain page needs no wrapper: the LIMIT is appended.
    let o = run(&mut s, join).await;
    assert!(o.wrapped && o.notices.is_empty() && o.executed_sql.ends_with("LIMIT 101 OFFSET 0"), "{:?}", o.executed_sql);
    assert_eq!(o.rows.len(), 1);
    // Sort and filters would need `SELECT * FROM (…) q`, which MySQL refuses (42S21): unwrapped, with a note.
    for p in [sorted(&[("id", true)]), page(&[("x", "a")])] {
        let o = run_page(&mut s, join, &p).await;
        assert!(!o.wrapped && !o.has_more, "{:?}", o.executed_sql);
        assert_eq!(o.executed_sql, join);
        assert_eq!(o.rows.len(), 1);
        assert!(o.notices.iter().any(|n| n.contains("`id` repeats")), "{:?}", o.notices);
    }
    let e = s.count(join, &[]).await.unwrap_err();
    assert!(e.message.contains("`id` repeats"), "{e:?}");
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn order_by_survives_paging() {
    let mut s = open_my().await;
    run(&mut s, "CREATE TEMPORARY TABLE ord (a INT)").await;
    run(&mut s, "INSERT INTO ord VALUES (3), (1), (5), (2), (4)").await;
    let p = |offset, size| Page { offset, size, ..Page::default() };
    let sql = "SELECT a FROM ord ORDER BY a DESC";
    // No filters, no sort: MariaDB would drop the ORDER BY of a derived table.
    assert_eq!(ints(&run_page(&mut s, sql, &p(0, 3)).await), [5, 4, 3]);
    assert_eq!(ints(&run_page(&mut s, sql, &p(3, 3)).await), [2, 1]);
    // Filters wrap, and still keep the order.
    let filtered = Page { filters: vec![("a".into(), ">1".into())], ..p(0, 3) };
    assert_eq!(ints(&run_page(&mut s, sql, &filtered).await), [5, 4, 3]);
    // The user's sort wins over the statement's.
    let sorted = Page { sort: vec![("a".into(), false)], ..p(0, 3) };
    assert_eq!(ints(&run_page(&mut s, sql, &sorted).await), [1, 2, 3]);
    // A statement's own LIMIT is kept.
    assert_eq!(ints(&run_page(&mut s, "SELECT a FROM ord ORDER BY a DESC LIMIT 4", &p(0, 2)).await), [5, 4]);
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn statement_timeout_is_applied_on_both_servers_and_after_reconnect() {
    let mut s = open_my().await;
    let mariadb = texts(&run(&mut s, "SELECT VERSION()").await)[0].to_lowercase().contains("mariadb");
    let var = if mariadb { "@@max_statement_time" } else { "@@max_execution_time" };
    // MariaDB counts seconds (1.5), MySQL milliseconds (1500).
    let want = if mariadb { 1.5 } else { 1500.0 };
    let num = |o: &db_exec::Outcome| texts(o)[0].parse::<f64>().unwrap();
    s.set_statement_timeout(Some(1500)).await.unwrap();
    assert_eq!(num(&run(&mut s, &format!("SELECT {var}")).await), want);
    // A killed connection is reopened with the same timeout.
    let old = s.backend_id();
    let mut other = open_my().await;
    run(&mut other, &format!("KILL CONNECTION {old}")).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let o = run(&mut s, &format!("SELECT {var}")).await;
    assert!(o.notices.iter().any(|n| n.contains("lost")), "{:?}", o.notices);
    assert_eq!(num(&o), want);
    if mariadb {
        // MariaDB really stops the statement.
        let e = try_run(&mut s, "SELECT SLEEP(5)").await.unwrap_err();
        assert_eq!(e.code.as_deref(), Some("70100"), "{e:?}");
    }
    s.set_statement_timeout(None).await.unwrap();
    assert_eq!(num(&run(&mut s, &format!("SELECT {var}")).await), 0.0);
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn only_the_first_of_several_result_sets_is_shown() {
    let mut s = open_my().await;
    let create = "CREATE PROCEDURE siesta_multi() BEGIN SELECT 1 AS a; SELECT 'x' AS b, 2 AS c; SELECT 3 AS d; END";
    s.run("DROP PROCEDURE IF EXISTS siesta_multi", Kind::Other, &Page::default()).await.unwrap();
    s.run(create, Kind::Other, &Page::default()).await.unwrap();
    let o = run(&mut s, "CALL siesta_multi()").await;
    assert_eq!(o.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["a"]);
    assert_eq!(ints(&o), [1]);
    assert!(o.notices.contains(&"2 more result sets not shown".to_string()), "{:?}", o.notices);
    s.run("DROP PROCEDURE siesta_multi", Kind::Other, &Page::default()).await.unwrap();
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn read_only_refuses_switching_to_read_write() {
    let mut s = open_my().await;
    run(&mut s, "DROP TABLE IF EXISTS siesta_ro_guard").await;
    run(&mut s, "CREATE TABLE siesta_ro_guard (a INT)").await;
    let mut c = connection(MY);
    c.read_only = true;
    let mut ro = Session::open(&c).await.unwrap();
    for sql in ["SET SESSION TRANSACTION READ WRITE", "START TRANSACTION READ WRITE", "SET @@session.tx_read_only = 0", "SET SESSION transaction_read_only = OFF"] {
        let e = try_run(&mut ro, sql).await.unwrap_err();
        assert!(e.refused && e.code.is_none() && e.message.contains("not a security boundary"), "{sql}: {e:?}");
    }
    // Still read-only afterwards.
    let e = try_run(&mut ro, "INSERT INTO siesta_ro_guard VALUES (1)").await.unwrap_err();
    assert_eq!(e.code.as_deref(), Some("25006"), "{e:?}");
    run(&mut s, "DROP TABLE siesta_ro_guard").await;
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn autocommit_off_counts_as_a_transaction() {
    let mut s = open_my().await;
    assert!(run(&mut s, "SET autocommit = 0").await.in_transaction);
    assert!(s.in_transaction());
    assert!(run(&mut s, "SELECT 1").await.in_transaction);
    assert!(!run(&mut s, "SET autocommit = 1").await.in_transaction);
    assert!(!s.in_transaction());
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn cancel_handle_follows_the_reconnected_backend() {
    let mut s = open_my().await;
    let handle = s.cancel_handle(); // taken before the reconnect
    let old = s.backend_id();
    let mut other = open_my().await;
    run(&mut other, &format!("KILL CONNECTION {old}")).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    run(&mut s, "SELECT 1").await; // reconnects
    assert_ne!(s.backend_id(), old);
    let started = Instant::now();
    let (res, cancelled) = tokio::join!(try_run(&mut s, "SELECT SLEEP(10)"), async {
        tokio::time::sleep(Duration::from_millis(400)).await;
        handle.cancel().await
    });
    cancelled.unwrap();
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    if let Err(e) = res {
        assert_eq!(e.code.as_deref(), Some("70100"));
    }
}
