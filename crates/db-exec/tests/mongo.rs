//! Needs a running MongoDB: `scripts/test-db.sh`.
mod common;

use common::*;
use db_conn::Driver;
use db_exec::{Page, Session, Value};

const MG: Driver = Driver::Mongo;

/// A collection with 5 documents.
async fn fixture(coll: &str) -> Session {
    let mut s = open(MG).await;
    let _ = try_run(&mut s, &format!("db.{coll}.drop()")).await;
    let docs: Vec<String> = [("ann", 30), ("bob", 17), ("cy", 45), ("dan", 22), ("eve", 61)].iter().enumerate().map(|(i, (n, a))| format!("{{_id: {}, name: '{n}', age: {a}, tags: ['t{i}'], at: ISODate('2024-01-0{}')}}", i + 1, i + 1)).collect();
    let o = run(&mut s, &format!("db.{coll}.insertMany([{}])", docs.join(", "))).await;
    assert_eq!(o.rows_affected, Some(5));
    s
}

fn page(offset: u64, size: u64, sort: &[(&str, bool)], filters: &[(&str, &str)]) -> Page {
    Page { offset, size, sort: sort.iter().map(|(c, d)| (c.to_string(), *d)).collect(), filters: filters.iter().map(|(c, t)| (c.to_string(), t.to_string())).collect() }
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn find_and_aggregate_page_sort_filter_and_count() {
    let mut s = fixture("siesta_mg_find").await;
    let q = "db.siesta_mg_find.find({})";
    let o = run_page(&mut s, q, &page(0, 2, &[("age", false)], &[])).await;
    assert!(o.wrapped && o.has_more);
    assert_eq!(o.columns.iter().map(|c| (c.name.as_str(), c.type_name.as_str())).collect::<Vec<_>>(), [("_id", "int"), ("name", "string"), ("age", "int"), ("tags", "array"), ("at", "date")]);
    assert_eq!(o.rows.iter().map(|r| r[1].display()).collect::<Vec<_>>(), ["bob", "dan"]);
    assert_eq!(o.rows[0][4], Value::DateTime("2024-01-02T00:00:00Z".into()));
    // Raw: the page's documents as extended JSON.
    assert_eq!(o.raw.as_ref().unwrap()[0]["at"], serde_json::json!({"$date": "2024-01-02T00:00:00Z"}));
    let last = run_page(&mut s, q, &page(4, 2, &[("age", false)], &[])).await;
    assert!(!last.has_more && last.rows.len() == 1);
    let adults = run_page(&mut s, q, &page(0, 10, &[("age", true)], &[("age", ">=30")])).await;
    assert_eq!(texts(&adults.clone()).len(), 3);
    assert_eq!(adults.rows.iter().map(|r| r[1].display()).collect::<Vec<_>>(), ["eve", "cy", "ann"]);
    assert_eq!(s.count(q, &[("name".into(), "A".into())]).await.unwrap(), 2); // ann, dan
    assert_eq!(s.count(q, &[("_id".into(), "=3".into())]).await.unwrap(), 1);
    // `.limit()` runs as written.
    let own = run(&mut s, "db.siesta_mg_find.find().sort({age: -1}).limit(1)").await;
    assert!(!own.wrapped && own.rows.len() == 1 && own.rows[0][1].display() == "eve");
    let agg = "db.siesta_mg_find.aggregate([{$group: {_id: {$gt: ['$age', 25]}, n: {$sum: 1}}}])";
    let g = run_page(&mut s, agg, &page(0, 10, &[("n", true)], &[])).await;
    assert_eq!(g.rows.iter().map(|r| r[1].display()).collect::<Vec<_>>(), ["3", "2"]);
    assert_eq!(s.count(agg, &[]).await.unwrap(), 2);
    let one = run(&mut s, "db.siesta_mg_find.findOne({name: /^C/i}, {name: 1, _id: 0})").await;
    assert_eq!((texts(&one), one.columns.len()), (vec!["cy".to_string()], 1));
    let n = run(&mut s, "db.siesta_mg_find.countDocuments({age: {$lt: 30}})").await;
    assert_eq!(n.rows, vec![vec![Value::Int(2)]]);
    let d = run(&mut s, "db.siesta_mg_find.distinct('name', {age: {$gt: 40}})").await;
    assert_eq!(texts(&d), ["cy", "eve"]);
    let e = run(&mut s, "db.siesta_mg_find.find({age: 30}).explain()").await;
    assert!(e.columns.iter().any(|c| c.name == "queryPlanner"), "{:?}", e.columns);
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn writes_report_what_they_changed() {
    let mut s = fixture("siesta_mg_write").await;
    let u = run(&mut s, "db.siesta_mg_write.updateMany({age: {$gt: 40}}, {$inc: {age: 1}})").await;
    assert_eq!(u.rows_affected, Some(2));
    let f = run(&mut s, "db.siesta_mg_write.findOneAndUpdate({_id: 1}, {$set: {name: 'anna'}}, {returnDocument: 'after'})").await;
    assert_eq!((f.rows_affected, f.rows[0][1].display()), (Some(1), "anna".to_string()));
    let i = run(&mut s, "db.siesta_mg_write.insertOne({name: 'new'})").await;
    assert_eq!(i.rows_affected, Some(1));
    assert!(i.raw.as_ref().unwrap()["insertedIds"][0]["$oid"].is_string(), "{:?}", i.raw);
    let dup = try_run(&mut s, "db.siesta_mg_write.insertOne({_id: 1})").await.unwrap_err();
    // Write errors carry the number only.
    assert_eq!(dup.code.as_deref(), Some("11000"), "{dup:?}");
    let del = run(&mut s, "db.siesta_mg_write.deleteMany({age: {$lt: 25}})").await;
    assert_eq!(del.rows_affected, Some(2));
    run(&mut s, "db.siesta_mg_write.createIndex({name: 1}, {unique: true})").await;
    let schema = s.schema().await.unwrap();
    let (db, t) = schema.find("siesta_mg_write").unwrap();
    assert_eq!(db, "siesta");
    assert!(t.columns.iter().any(|c| c.name == "name" && c.data_type == "string"), "{:?}", t.columns);
    assert!(t.indexes.iter().any(|i| i.name == "name_1" && i.unique), "{:?}", t.indexes);
    let ddl = s.table_ddl("siesta", "siesta_mg_write").await.unwrap();
    assert!(ddl.starts_with("db.createCollection(\"siesta_mg_write\")\ndb.siesta_mg_write.createIndex({\"name\":1}, {\"name\":\"name_1\",\"unique\":true})"), "{ddl}");
    assert!(s.databases().await.unwrap().contains(&"siesta".to_string()));
    let e = try_run(&mut s, "db.siesta_mg_write.aggregate([{$frob: 1}])").await.unwrap_err();
    assert!(e.code.is_some(), "{e:?}");
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn read_only_refuses_writes_and_auth_errors_name_the_fix() {
    let mut c = connection(MG);
    c.read_only = true;
    let mut s = Session::open(&c).await.unwrap();
    let e = try_run(&mut s, "db.anything.drop()").await.unwrap_err();
    assert!(e.refused && e.message.contains("db.anything.drop(…)"), "{e:?}");
    let e = try_run(&mut s, "db.a.aggregate([{$out: 'b'}])").await.unwrap_err();
    assert!(e.refused, "{e:?}");
    run(&mut s, "show collections").await;
    run(&mut s, "db.runCommand({ping: 1})").await;
    let mut anon = connection(MG);
    anon.name = "mongo-no-password".into();
    anon.url = Some("mongodb://siesta@127.0.0.1:27018/siesta?authSource=admin".into());
    let e = Session::open(&anon).await.err().expect("needs a password");
    assert!(e.message.contains("No password is stored for `mongo-no-password`"), "{e}");
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn cancel_drops_the_command() {
    let mut s = fixture("siesta_mg_cancel").await;
    let cancel = s.cancel_handle();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        cancel.cancel().await.unwrap();
    });
    let t = std::time::Instant::now();
    let slow = "db.siesta_mg_cancel.find({$where: 'sleep(1000) || true'})";
    let e = try_run(&mut s, slow).await.unwrap_err();
    assert!(e.message.starts_with("Cancelled") && t.elapsed() < std::time::Duration::from_secs(3), "{e:?} after {:?}", t.elapsed());
    // The session still works.
    run(&mut s, "db.siesta_mg_cancel.findOne()").await;
}
