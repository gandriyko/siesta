//! Needs running Postgres and MariaDB: `scripts/test-db.sh`.
mod common;

use common::*;
use db_conn::Driver;
use db_exec::{Schema, TableKind};

async fn fixture(driver: Driver) -> (db_exec::Session, String) {
    let mut s = open(driver).await;
    for sql in [
        "DROP VIEW IF EXISTS siesta_sc_v",
        "DROP TABLE IF EXISTS siesta_sc_child",
        "DROP TABLE IF EXISTS siesta_sc_parent",
        "CREATE TABLE siesta_sc_parent (id INT NOT NULL, email VARCHAR(100) NOT NULL, note VARCHAR(20) DEFAULT 'x', PRIMARY KEY (id), UNIQUE (email))",
        "CREATE TABLE siesta_sc_child (a INT NOT NULL, b INT NOT NULL, parent_id INT, PRIMARY KEY (a, b), FOREIGN KEY (parent_id) REFERENCES siesta_sc_parent (id))",
        "CREATE INDEX siesta_sc_idx ON siesta_sc_child (parent_id, b)",
        "CREATE VIEW siesta_sc_v AS SELECT id FROM siesta_sc_parent",
    ] {
        run(&mut s, sql).await;
    }
    let db = match driver {
        Driver::Postgres => "public",
        Driver::MySql => "siesta",
        Driver::Elastic => unreachable!("SQL fixture"),
    };
    (s, db.to_string())
}

async fn check(driver: Driver) {
    let (mut s, db) = fixture(driver).await;
    let schema = s.schema().await.expect("schema");
    // No system schemas, and it round-trips through serde (the on-disk cache).
    assert!(schema.schemas.iter().all(|n| !["pg_catalog", "information_schema", "mysql", "sys"].contains(&n.name.as_str())), "{:?}", schema.schemas.iter().map(|n| &n.name).collect::<Vec<_>>());
    assert_eq!(serde_json::from_str::<Schema>(&serde_json::to_string(&schema).unwrap()).unwrap(), schema);

    let (sch, parent) = schema.find(&format!("{db}.siesta_sc_parent")).expect("parent table");
    assert_eq!(sch, db);
    assert_eq!(parent.kind, TableKind::Table);
    let cols: Vec<_> = parent.columns.iter().map(|c| (c.name.as_str(), c.nullable, c.primary_key, c.ordinal)).collect();
    assert_eq!(cols, [("id", false, true, 1), ("email", false, false, 2), ("note", true, false, 3)]);
    assert!(parent.columns[1].data_type.contains("varchar") || parent.columns[1].data_type.contains("character varying"), "{}", parent.columns[1].data_type);
    let pk = parent.indexes.iter().find(|i| i.primary).expect("primary index");
    assert_eq!(pk.columns, ["id"]);
    assert!(parent.indexes.iter().any(|i| i.unique && !i.primary && i.columns == ["email"]));

    let (_, child) = schema.find("siesta_sc_child").unwrap();
    assert_eq!(child.columns.iter().filter(|c| c.primary_key).map(|c| c.name.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    let idx = child.indexes.iter().find(|i| i.name == "siesta_sc_idx").expect("secondary index");
    assert_eq!((idx.columns.as_slice(), idx.unique, idx.primary), (["parent_id".to_string(), "b".to_string()].as_slice(), false, false));

    let (_, view) = schema.find("siesta_sc_v").unwrap();
    assert_eq!(view.kind, TableKind::View);
    assert_eq!(view.columns.len(), 1);

    let ddl = s.table_ddl(&db, "siesta_sc_child").await.expect("ddl");
    assert!(ddl.to_uppercase().contains("CREATE TABLE") && ddl.contains("siesta_sc_child"), "{ddl}");
    assert!(ddl.to_uppercase().contains("PRIMARY KEY") && ddl.to_uppercase().contains("FOREIGN KEY"), "{ddl}");
    let ddl = s.table_ddl(&db, "siesta_sc_parent").await.expect("ddl");
    assert!(ddl.contains("NOT NULL") && ddl.to_uppercase().contains("UNIQUE") && ddl.contains("DEFAULT"), "{ddl}");
    let view_ddl = s.table_ddl(&db, "siesta_sc_v").await.expect("view ddl");
    assert!(view_ddl.to_uppercase().contains("CREATE") && view_ddl.contains("siesta_sc_v"), "{view_ddl}");
    assert!(s.table_ddl(&db, "siesta_sc_nope").await.is_err());
    // Quotes in names cannot break out of the catalog query.
    assert!(s.table_ddl(&db, "x' OR '1'='1").await.is_err());

    // Where result columns come from (editing): aliases resolve to the real names, expressions
    // and views have no key, a join names both tables.
    let src = s.source("SELECT p.note AS n, p.id, upper(p.email) AS e FROM siesta_sc_parent p;").await.expect("source");
    assert_eq!(src.tables, [db_exec::SourceTable { schema: db.clone(), name: "siesta_sc_parent".into(), primary_key: vec!["id".into()] }]);
    assert_eq!(src.columns, [Some((0, "note".to_string())), Some((0, "id".to_string())), None]);
    let src = s.source("SELECT c.b, c.a, p.email FROM siesta_sc_child c JOIN siesta_sc_parent p ON p.id = c.parent_id").await.expect("join source");
    let child = src.tables.iter().find(|t| t.name == "siesta_sc_child").expect("child");
    assert_eq!(child.primary_key, ["a", "b"]);
    assert_eq!(src.tables.len(), 2);
    let src = s.source("SELECT id FROM siesta_sc_v").await.expect("view source");
    assert!(src.tables.iter().all(|t| t.primary_key.is_empty()), "{src:?}");
    let src = s.source("SELECT 1 AS x").await.expect("expression source");
    assert_eq!((src.columns, src.tables.len()), (vec![None], 0));
    assert!(s.source("SELECT nope FROM siesta_sc_parent").await.is_err());
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn postgres_schema_and_ddl() {
    check(Driver::Postgres).await;
}

#[tokio::test]
#[ignore = "needs docker: scripts/test-db.sh"]
async fn mysql_schema_and_ddl() {
    check(Driver::MySql).await;
}
