//! Round trips on real databases: compare, run the script on the target, compare again.
//! The PostgreSQL one is ignored like the other live tests; run it with
//! `cargo test -- --ignored` and DATACRAFT_PG_HOST (DATACRAFT_PG_PORT, _USER, _PASSWORD, _DB).

use super::read::{Progress, Side, run};
use super::*;
use crate::db::{ConnectionConfig, Session};
use std::sync::Arc;

async fn round_trip(source: ConnectionConfig, target: ConnectionConfig, src: Side, tgt: Side) -> (Comparison, Script) {
    let (s, t) = run(source.clone(), target.clone(), src.clone(), tgt.clone(), Arc::new(Progress::default())).await.unwrap();
    let before = compare(&s, &t);
    assert!(before.has_differences());
    let script = script(&s, &t, &before, &vec![true; before.objects.len()]).unwrap();
    let mut session = Session::connect(&target).await.unwrap();
    session.use_database(&tgt.db).await.unwrap();
    for statement in &script.statements {
        session.execute(statement, 10).await.unwrap_or_else(|e| panic!("{statement}\n{e:#}\n\n{}", script.text(&s, &t)));
    }
    let (s, t) = run(source, target, src, tgt, Arc::new(Progress::default())).await.unwrap();
    let after = compare(&s, &t);
    let left: Vec<_> = after.objects.iter().filter(|o| o.status != Status::Identical).map(|o| (&o.name, o.status, &o.changes)).collect();
    assert!(left.is_empty(), "differences after the script: {left:#?}\n\n{}", script.text(&s, &t));
    (before, script)
}

fn sqlite(id: u64, path: &std::path::Path) -> ConnectionConfig {
    let mut cfg = ConnectionConfig::new(id, Driver::Sqlite);
    cfg.file_path = path.display().to_string();
    cfg
}

#[tokio::test]
async fn sqlite_script_makes_the_target_match_the_source() {
    let dir = std::env::temp_dir().join(format!("datacraft-compare-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let (src_path, tgt_path) = (dir.join("source.db"), dir.join("target.db"));
    rusqlite::Connection::open(&src_path)
        .unwrap()
        .execute_batch(
            "create table customers (id integer primary key autoincrement, email text not null unique, name text, tier text default 'basic');
             create table products (id integer primary key, sku text not null, price real not null default 0);
             create index products_sku on products (sku);
             create table orders (id integer primary key, customer_id integer not null references customers (id) on delete cascade,
                 total real not null, status text not null default 'new', created_at text default current_timestamp);
             create index orders_customer on orders (customer_id);
             create table order_items (order_id integer not null references orders (id), product_id integer not null references products,
                 qty integer not null default 1, primary key (order_id, product_id));
             create view big_orders as select id, total from orders where total > 100;
             create view customer_orders as select c.email, o.id from customers c join orders o on o.customer_id = c.id;",
        )
        .unwrap();
    rusqlite::Connection::open(&tgt_path)
        .unwrap()
        .execute_batch(
            "create table customers (id integer primary key autoincrement, email text not null unique, name text);
             create table products (id integer primary key, sku text, price real not null default 0);
             create table orders (id integer primary key, customer_id integer not null, total text, legacy text);
             create index orders_legacy on orders (legacy);
             create table audit_log (id integer primary key, msg text);
             create view big_orders as select id, total from orders where total > 1000;
             create view legacy_view as select legacy from orders;
             insert into customers (email, name) values ('a@example.com', 'Ann'), ('b@example.com', 'Bob');
             insert into products values (1, 'SKU-1', 9.5);
             insert into orders values (1, 1, '120.5', 'x'), (2, 2, '80', null);",
        )
        .unwrap();
    let side = || Side { db: "main".into(), schema: "main".into() };
    let (before, script) = round_trip(sqlite(1, &src_path), sqlite(2, &tgt_path), side(), side()).await;
    for (name, status) in [
        ("order_items", Status::OnlyInSource),
        ("customer_orders", Status::OnlyInSource),
        ("audit_log", Status::OnlyInTarget),
        ("legacy_view", Status::OnlyInTarget),
        ("customers", Status::Different),
        ("products", Status::Different),
        ("orders", Status::Different),
        ("big_orders", Status::Different),
    ] {
        assert_eq!(before.objects.iter().find(|o| o.name == name).map(|o| o.status), Some(status), "{name}");
    }
    assert!(script.statements.iter().any(|s| s == "alter table customers add column tier TEXT default 'basic'"), "{:#?}", script.statements);
    // Rows survive the rebuilds, converted to the new types.
    let db = rusqlite::Connection::open(&tgt_path).unwrap();
    let rows: Vec<(i64, f64, String)> =
        db.prepare("select id, total, status from orders order by id").unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(Result::unwrap).collect();
    assert_eq!(rows, [(1, 120.5, "new".to_string()), (2, 80.0, "new".to_string())]);
    let tiers: i64 = db.query_row("select count(*) from customers where tier = 'basic'", [], |r| r.get(0)).unwrap();
    assert_eq!(tiers, 2);
    assert_eq!(db.query_row("select count(*) from pragma_foreign_key_check", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    drop(db);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
#[ignore]
async fn postgres_script_makes_the_target_match_the_source() {
    let Ok(host) = std::env::var("DATACRAFT_PG_HOST") else {
        eprintln!("DATACRAFT_PG_HOST not set: skipping the PostgreSQL test");
        return;
    };
    let env = |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_string());
    let mut cfg = ConnectionConfig::new(1, Driver::Postgres);
    cfg.host = host;
    cfg.port = env("DATACRAFT_PG_PORT", "5432").parse().expect("port");
    cfg.user = env("DATACRAFT_PG_USER", "postgres");
    cfg.password = env("DATACRAFT_PG_PASSWORD", "");
    cfg.database = env("DATACRAFT_PG_DB", "postgres");
    let tag = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let (a, b) = (format!("dc_cmp_src_{tag}"), format!("dc_cmp_tgt_{tag}"));
    let mut admin = Session::connect(&cfg).await.unwrap();
    let setup = format!(
        "create schema {a}; create schema {b};
         create table {a}.customers (id serial primary key, email varchar(255) not null unique, name text, created_at timestamptz not null default now());
         create table {a}.products (id integer generated by default as identity primary key, sku varchar(40) not null, price numeric(10,2) not null default 0);
         create index products_sku_idx on {a}.products (sku);
         create table {a}.orders (id bigserial primary key, customer_id integer not null references {a}.customers (id) on delete cascade,
             status varchar(20) not null default 'new', total numeric(12,2) not null, note text);
         create index orders_customer_idx on {a}.orders (customer_id);
         create table {a}.order_items (order_id bigint not null references {a}.orders (id), product_id integer not null references {a}.products (id),
             qty integer not null default 1, primary key (order_id, product_id));
         create view {a}.big_orders as select id, total from {a}.orders where total > 100;
         create view {a}.emails as select email from {a}.customers;
         create view {a}.customer_totals as select c.email, sum(o.total) as total from {a}.customers c join {a}.orders o on o.customer_id = c.id group by c.email;
         create table {b}.customers (id serial primary key, email varchar(200) not null, name text, legacy_code char(3));
         create table {b}.products (id integer primary key, sku varchar(40), price numeric(10,2) not null);
         create table {b}.orders (id bigserial primary key, customer_id integer not null, status varchar(10) not null, total numeric(12,2) not null);
         create table {b}.coupons (id serial primary key, code text not null);
         create view {b}.big_orders as select id, total from {b}.orders where total > 1000;
         create view {b}.emails as select email from {b}.customers;
         create view {b}.statuses as select distinct status from {b}.orders;
         insert into {b}.customers (email, name) values ('a@example.com', 'Ann');
         insert into {b}.products values (1, 'SKU-1', 9.5);
         insert into {b}.orders (customer_id, status, total) values (1, 'paid', 120.5);"
    );
    admin.execute(&setup, 10).await.unwrap();
    let result = tokio::spawn({
        let (cfg, db, a, b) = (cfg.clone(), cfg.database.clone(), a.clone(), b.clone());
        async move { round_trip(cfg.clone(), cfg, Side { db: db.clone(), schema: a }, Side { db, schema: b }).await }
    })
    .await;
    let rows = admin.execute(&format!("select count(*) from {b}.orders where status = 'paid'"), 10).await;
    admin.execute(&format!("drop schema {a} cascade; drop schema {b} cascade"), 10).await.unwrap();
    let (before, script) = result.unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()));
    assert_eq!(rows.unwrap()[0].rows[0][0].as_deref(), Some("1"));
    assert_eq!(before.count(Status::OnlyInTarget), 2, "{before:#?}");
    // The view on the retyped column is dropped first and created again.
    let text = script.statements.join(";\n");
    assert!(text.contains(&format!("drop view {b}.emails")) && text.contains(&format!("create view {b}.emails")), "{text}");
    assert!(!text.contains(&format!("{a}.")), "{text}");
}
