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
        schema: None,
        init_sql: None,
        color: None,
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
