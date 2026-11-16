#![allow(dead_code)]

use db_conn::{Connection, Driver, Ssl};
use db_exec::{Outcome, Page, Session, Value};
use sql_split::{Dialect, split};

pub fn connection(driver: Driver) -> Connection {
    let var = match driver {
        Driver::Postgres => "SIESTA_TEST_PG_URL",
        Driver::MySql => "SIESTA_TEST_MYSQL_URL",
    };
    Connection {
        name: "test".into(),
        driver,
        url: Some(std::env::var(var).unwrap_or_else(|_| panic!("{var} is not set; run scripts/test-db.sh"))),
        host: None,
        port: None,
        database: None,
        user: None,
        password: None,
        ssl: Ssl::default(),
        read_only: false,
        prod: false,
        schema: None,
        init_sql: None,
        color: None,
    }
}

/// A rejected login without any password says where to set one (the keychain is the in-memory mock).
pub async fn rejected_login_names_the_fix(driver: Driver) {
    db_conn::secrets::use_memory_store();
    let mut c = connection(driver);
    c.name = "test-no-password".into();
    c.url = c.url.map(|u| u.replacen(":siesta@", "@", 1));
    let e = Session::open(&c).await.err().expect("login without a password is rejected");
    assert!(e.message.contains("No password is stored for `test-no-password`") && e.message.contains("set-password"), "{e}");
}

/// The server lists the accessible databases, and a session opened with another `database` runs in it.
pub async fn databases_and_override(driver: Driver) {
    let mut s = open(driver).await;
    let dbs = s.databases().await.expect("databases");
    for want in ["siesta", "siesta_b"] {
        assert!(dbs.iter().any(|d| d == want), "{want} missing in {dbs:?}");
    }
    assert!(!dbs.iter().any(|d| d == "template0" || d == "template1"), "{dbs:?}");
    if driver == Driver::MySql {
        // System databases come last.
        let first_system = dbs.iter().position(|d| db_exec::is_system_database(d)).expect("system databases are listed");
        assert!(dbs[first_system..].iter().all(|d| db_exec::is_system_database(d)), "{dbs:?}");
    }
    s.close().await;
    let current = if driver == Driver::Postgres { "SELECT current_database()" } else { "SELECT DATABASE()" };
    let mut s = Session::open(&connection(driver).with_database(Some("siesta_b"))).await.expect("open siesta_b");
    assert_eq!(texts(&run(&mut s, current).await), ["siesta_b"]);
    s.close().await;
    // No override: the connection's own database.
    let mut s = open(driver).await;
    assert_eq!(texts(&run(&mut s, current).await), ["siesta"]);
}

/// `driver` pointed at the port of the other database. It must fail within the connect timeout.
/// `hangs`: the client waits for a server greeting that never comes (MySQL on Postgres), so the
/// timeout itself ends it with the "No response" message; the Postgres client instead rejects
/// MariaDB's greeting at once ("unexpected response from SSLRequest").
pub async fn wrong_driver_fails_fast(driver: Driver, other_url_var: &str, hangs: bool) {
    let mut c = connection(driver);
    c.url = Some(std::env::var(other_url_var).unwrap_or_else(|_| panic!("{other_url_var} is not set; run scripts/test-db.sh")));
    let t = std::time::Instant::now();
    let e = Session::open_with(&c, std::time::Duration::from_millis(1500)).await.err().expect("must fail");
    assert!(t.elapsed() < std::time::Duration::from_secs(5), "took {:?}", t.elapsed());
    if hangs {
        assert!(e.connection_lost && e.message.starts_with("No response from 127.0.0.1:") && e.message.contains("within 1.5 s"), "{e:?}");
    }
}

pub async fn open(driver: Driver) -> Session {
    Session::open(&connection(driver)).await.expect("open session")
}

fn dialect(s: &Session) -> Dialect {
    s.dialect()
}

/// Runs `sql` as the splitter classifies it, with the first page of 100.
pub async fn run(s: &mut Session, sql: &str) -> Outcome {
    run_page(s, sql, &Page::default()).await
}

pub async fn run_page(s: &mut Session, sql: &str, page: &Page) -> Outcome {
    try_run_page(s, sql, page).await.unwrap_or_else(|e| panic!("`{sql}` failed: {e:?}"))
}

pub async fn try_run_page(s: &mut Session, sql: &str, page: &Page) -> Result<Outcome, db_exec::DbError> {
    let file = split(sql, dialect(s));
    let st = &file.statements[0];
    s.run(st.text(sql), st.kind, page).await
}

pub async fn try_run(s: &mut Session, sql: &str) -> Result<Outcome, db_exec::DbError> {
    try_run_page(s, sql, &Page::default()).await
}

pub fn page(filters: &[(&str, &str)]) -> Page {
    Page { filters: filters.iter().map(|(c, t)| (c.to_string(), t.to_string())).collect(), ..Page::default() }
}

pub fn sorted(sort: &[(&str, bool)]) -> Page {
    Page { sort: sort.iter().map(|(c, d)| (c.to_string(), *d)).collect(), ..Page::default() }
}

/// First column of every row, as ints.
pub fn ints(o: &Outcome) -> Vec<i64> {
    o.rows
        .iter()
        .map(|r| match &r[0] {
            Value::Int(i) => *i,
            v => panic!("not an int: {v:?}"),
        })
        .collect()
}

pub fn texts(o: &Outcome) -> Vec<String> {
    o.rows.iter().map(|r| r[0].display()).collect()
}

pub fn col<'a>(o: &'a Outcome, name: &str) -> (&'a str, &'a Value) {
    let i = o.columns.iter().position(|c| c.name == name).unwrap_or_else(|| panic!("no column {name}: {:?}", o.columns));
    (&o.columns[i].type_name, &o.rows[0][i])
}
