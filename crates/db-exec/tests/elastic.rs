//! Needs a running Elasticsearch: `scripts/test-db.sh`.
mod common;

use common::*;
use db_conn::Driver;
use db_exec::{Page, Session, Value};

const ES: Driver = Driver::Elastic;

/// An index with 5 documents, refreshed so they are searchable at once.
async fn fixture(index: &str) -> Session {
    let mut s = open(ES).await;
    let _ = try_run(&mut s, &format!("DELETE /{index}")).await;
    run(&mut s, &format!("PUT /{index}\n{{\"mappings\": {{\"properties\": {{\"name\": {{\"type\": \"keyword\"}}, \"age\": {{\"type\": \"long\"}}}}}}}}")).await;
    let mut bulk = String::new();
    for (i, (name, age)) in [("ann", 30), ("bob", 17), ("cy", 45), ("dan", 22), ("eve", 61)].iter().enumerate() {
        bulk += &format!("{{\"index\": {{\"_id\": \"{}\"}}}}\n{{\"name\": \"{name}\", \"age\": {age}}}\n", i + 1);
    }
    let o = run(&mut s, &format!("POST /{index}/_bulk?refresh=true\n{bulk}")).await;
    assert_eq!(o.rows_affected, Some(5));
    s
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn search_pages_sorts_filters_and_counts() {
    let mut s = fixture("siesta_es_search").await;
    let q = "GET /siesta_es_search/_search";
    let page = |offset, size, sort: &[(&str, bool)], filters: &[(&str, &str)]| Page {
        offset,
        size,
        sort: sort.iter().map(|(c, d)| (c.to_string(), *d)).collect(),
        filters: filters.iter().map(|(c, t)| (c.to_string(), t.to_string())).collect(),
    };
    let o = run_page(&mut s, q, &page(0, 2, &[("age", false)], &[])).await;
    assert!(o.wrapped && o.has_more);
    assert_eq!(o.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["_id", "name", "age"]);
    assert_eq!(o.rows.iter().map(|r| r[1].clone()).collect::<Vec<_>>(), [Value::Text("bob".into()), Value::Text("dan".into())]);
    let last = run_page(&mut s, q, &page(4, 2, &[("age", false)], &[])).await;
    assert!(!last.has_more && last.rows.len() == 1);
    let adults = run_page(&mut s, q, &page(0, 10, &[("age", true)], &[("age", ">=30")])).await;
    assert_eq!(adults.rows.iter().map(|r| r[1].display()).collect::<Vec<_>>(), ["eve", "cy", "ann"]);
    assert_eq!(s.count(q, &[("name".into(), "a".into())]).await.unwrap(), 2); // ann, dan
    // A body with its own size runs as written.
    let own = run(&mut s, "GET /siesta_es_search/_search\n{\"size\": 1}").await;
    assert!(!own.wrapped && own.rows.len() == 1);
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn other_endpoints_and_errors() {
    let mut s = fixture("siesta_es_other").await;
    let sql = run(&mut s, "POST /_sql?format=json\n{\"query\": \"SELECT name FROM siesta_es_other WHERE age > 40 ORDER BY name\"}").await;
    assert_eq!(texts(&sql), ["cy", "eve"]);
    let cat = run(&mut s, "GET /_cat/indices/siesta_es_other").await;
    assert!(cat.columns.iter().any(|c| c.name == "docs.count"), "{:?}", cat.columns);
    let doc = run(&mut s, "POST /siesta_es_other/_update/1?refresh=true\n{\"doc\": {\"age\": 31}}").await;
    assert_eq!(doc.rows_affected, Some(1));
    let e = try_run(&mut s, "GET /siesta_es_missing/_search").await.unwrap_err();
    assert_eq!(e.code.as_deref(), Some("index_not_found_exception"));
    let schema = s.schema().await.unwrap();
    let (_, t) = schema.find("siesta_es_other").unwrap();
    assert_eq!(t.columns.iter().map(|c| (c.name.as_str(), c.data_type.as_str())).collect::<Vec<_>>(), [("age", "long"), ("name", "keyword")]);
    assert!(s.table_ddl("indices", "siesta_es_other").await.unwrap().starts_with("PUT /siesta_es_other\n{"));
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn read_only_refuses_writes_and_auth_errors_name_the_fix() {
    let mut c = connection(ES);
    c.read_only = true;
    let mut s = Session::open(&c).await.unwrap();
    let e = try_run(&mut s, "DELETE /anything").await.unwrap_err();
    assert!(e.refused && e.message.contains("DELETE /anything"), "{e:?}");
    run(&mut s, "GET /_cluster/health").await;
    let mut anon = connection(ES);
    anon.name = "es-no-password".into();
    anon.url = anon.url.map(|u| u.replacen("elastic:siesta@", "", 1));
    let e = Session::open(&anon).await.err().expect("needs credentials");
    assert!(e.message.contains("No password or API key is stored for `es-no-password`"), "{e}");
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn cancel_drops_the_request() {
    let mut s = open(ES).await;
    let cancel = s.cancel_handle();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        cancel.cancel().await.unwrap();
    });
    // A single-node cluster never has 5 nodes: the request waits its 10 s timeout.
    let t = std::time::Instant::now();
    let slow = "GET /_cluster/health?wait_for_nodes=5&timeout=10s";
    let e = try_run(&mut s, slow).await.unwrap_err();
    assert!(e.message.starts_with("Cancelled") && t.elapsed() < std::time::Duration::from_secs(3), "{e:?} after {:?}", t.elapsed());
}
