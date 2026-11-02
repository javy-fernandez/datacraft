//! Round trips on real databases: compare, run the script on the target, compare again.
//! The PostgreSQL ones are ignored like the other live tests; run them with
//! `cargo test -- --ignored` and DATACRAFT_PG_HOST (DATACRAFT_PG_PORT, _USER, _PASSWORD, _DB).

use super::read::{Progress, Side, run};
use super::*;
use crate::db::{ConnectionConfig, Session};
use std::sync::Arc;

/// Compares, runs the script on the target the way a console runs it (in the engine's batches),
/// and compares again: nothing may be left.
async fn round_trip(source: ConnectionConfig, target: ConnectionConfig, src: Side, tgt: Side) -> (Comparison, Script) {
    let (s, t) = run(source.clone(), target.clone(), src.clone(), tgt.clone(), Arc::new(Progress::default())).await.unwrap();
    let before = compare(&s, &t);
    assert!(before.has_differences());
    let script = script(&s, &t, &before, &vec![true; before.objects.len()]).unwrap();
    let text = script.text(&s, &t);
    let mut session = Session::connect(&target).await.unwrap();
    session.use_database(&tgt.db).await.unwrap();
    for batch in crate::sql::split_batches(&text, t.driver) {
        session.execute(&batch.text, 10).await.unwrap_or_else(|e| panic!("{}\n{e:#}\n\n{text}", batch.text));
    }
    let (s, t) = run(source, target, src, tgt, Arc::new(Progress::default())).await.unwrap();
    let after = compare(&s, &t);
    let left: Vec<_> = after.objects.iter().filter(|o| o.status != Status::Identical).map(|o| (&o.name, o.status, &o.changes)).collect();
    assert!(left.is_empty(), "differences after the script: {left:#?}\n\n{text}");
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
             create view customer_orders as select c.email, o.id from customers c join orders o on o.customer_id = c.id;
             create virtual table notes using fts5(body);",
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
    // The virtual table is reported as not compared, never part of the script.
    assert!(before.skipped_source.iter().any(|s| s.starts_with("notes (")), "{:?}", before.skipped_source);
    assert!(!script.statements.iter().any(|s| s.contains("notes")));
    assert!(script.statements.iter().any(|s| s == "alter table customers add column tier TEXT default 'basic'"), "{:#?}", script.statements);
    for w in ["Drops column “legacy” and all its data", "Changing the type of “total”", "Drops table “audit_log”"] {
        assert!(script.warnings.iter().any(|x| x.contains(w)), "{w}: {:?}", script.warnings);
    }
    // The session's settings (DataCraft's SQLite sessions check foreign keys) come back at the
    // end of the script.
    let n = script.statements.len();
    assert_eq!(&script.statements[n - 2..], ["pragma foreign_keys = on", "pragma legacy_alter_table = off"]);
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

/// A cancel stops the comparison while a connection is still being opened: here the server
/// accepts the connection and never answers.
#[tokio::test]
async fn cancel_stops_a_comparison_that_is_still_connecting() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let silent = tokio::spawn(async move {
        let mut held = vec![];
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    let mut cfg = ConnectionConfig::new(1, Driver::Postgres);
    cfg.host = "127.0.0.1".into();
    cfg.port = port;
    cfg.user = "nobody".into();
    cfg.database = "db".into();
    let progress = Arc::new(Progress::default());
    let side = || Side { db: "db".into(), schema: "public".into() };
    let started = std::time::Instant::now();
    let task = tokio::spawn(run(cfg.clone(), cfg, side(), side(), progress.clone()));
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!task.is_finished(), "the connection must still be waiting");
    progress.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), task).await.expect("the cancel must end the comparison").unwrap();
    assert_eq!(result.err().map(|e| e.to_string()).as_deref(), Some(crate::i18n::tr("Cancelled")));
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    silent.abort();
}

fn pg_config() -> Option<ConnectionConfig> {
    let Ok(host) = std::env::var("DATACRAFT_PG_HOST") else {
        eprintln!("DATACRAFT_PG_HOST not set: skipping the PostgreSQL test");
        return None;
    };
    let env = |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_string());
    let mut cfg = ConnectionConfig::new(1, Driver::Postgres);
    cfg.host = host;
    cfg.port = env("DATACRAFT_PG_PORT", "5432").parse().expect("port");
    cfg.user = env("DATACRAFT_PG_USER", "postgres");
    cfg.password = env("DATACRAFT_PG_PASSWORD", "");
    cfg.database = env("DATACRAFT_PG_DB", "postgres");
    Some(cfg)
}

#[tokio::test]
#[ignore]
async fn postgres_script_makes_the_target_match_the_source() {
    let Some(cfg) = pg_config() else { return };
    let tag = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let (a, b) = (format!("dc_cmp_src_{tag}"), format!("dc_cmp_tgt_{tag}"));
    let mut admin = Session::connect(&cfg).await.unwrap();
    // Besides the usual differences: a table named like the source schema, used by a view through
    // an alias of that name, with a default calling a function of the source schema and a literal
    // naming it (both shared: they must stay as they are); a generated column; a primary key
    // change under a view; a view changed in place; a partitioned table (not compared).
    let setup = format!(
        "create schema {a}; create schema {b};
         create function {a}.f() returns int language sql as 'select 1';
         create table {a}.customers (id serial primary key, email varchar(255) not null unique, name text, created_at timestamptz not null default now());
         create table {a}.products (id integer generated by default as identity primary key, sku varchar(40) not null, price numeric(10,2) not null default 0);
         create index products_sku_idx on {a}.products (sku);
         create table {a}.orders (id bigserial primary key, customer_id integer not null references {a}.customers (id) on delete cascade,
             status varchar(20) not null default 'new', total numeric(12,2) not null, note text, doubled numeric generated always as (total * 2) stored);
         create index orders_customer_idx on {a}.orders (customer_id);
         create table {a}.order_items (order_id bigint not null references {a}.orders (id), product_id integer not null references {a}.products (id),
             qty integer not null default 1, primary key (order_id, product_id));
         create table {a}.{a} (id int primary key, v text, x int default {a}.f(), s text default '{a}.home');
         create table {a}.other (id int, w text);
         create table {a}.tags (id int, code text, primary key (id, code));
         create table {a}.events (id int, at date) partition by range (at);
         create table {a}.events_2026 partition of {a}.events for values from ('2026-01-01') to ('2027-01-01');
         create view {a}.big_orders as select id, total from {a}.orders where total > 100;
         create view {a}.emails as select email from {a}.customers;
         create view {a}.customer_totals as select c.email, sum(o.total) as total from {a}.customers c join {a}.orders o on o.customer_id = c.id group by c.email;
         create view {a}.v2 as select {a}.id, other.w from {a}.{a} join {a}.other on other.id = {a}.id;
         create view {a}.tag_codes as select id, code from {a}.tags;
         create table {b}.customers (id serial primary key, email varchar(200) not null, name text, legacy_code char(3));
         create table {b}.products (id integer primary key, sku varchar(40), price numeric(10,2) not null);
         create table {b}.orders (id bigserial primary key, customer_id integer not null, status varchar(10) not null, total numeric(12,2) not null);
         create table {b}.coupons (id serial primary key, code text not null);
         create table {b}.{a} (id int primary key, v text, x int default {a}.f(), s text default '{a}.home');
         create table {b}.other (id int, w text);
         create table {b}.tags (id int, code text, primary key (id));
         create view {b}.big_orders as select id, total from {b}.orders where total > 1000;
         create view {b}.emails as select email from {b}.customers;
         create view {b}.statuses as select distinct status from {b}.orders;
         create view {b}.v2 as select {a}.id, other.w from {b}.{a} join {b}.other on other.id = {a}.id;
         create view {b}.tag_codes as select id, code from {b}.tags;
         grant select on {b}.tag_codes to public;
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
    let grants = admin.execute(&format!("select count(*) from information_schema.role_table_grants where table_schema = '{b}' and table_name = 'tag_codes' and grantee = 'PUBLIC'"), 10).await;
    admin.execute(&format!("drop schema {a} cascade; drop schema {b} cascade"), 10).await.unwrap();
    let (before, script) = result.unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()));
    assert_eq!(rows.unwrap()[0].rows[0][0].as_deref(), Some("1"));
    assert_eq!(before.count(Status::OnlyInTarget), 2, "{before:#?}");
    let status = |name: &str| before.objects.iter().find(|o| o.name == name).map(|o| o.status);
    assert_eq!((status(&a), status("v2"), status("other")), (Some(Status::Identical), Some(Status::Identical), Some(Status::Identical)));
    assert_eq!(before.skipped_source.len(), 2, "{:?}", before.skipped_source);
    let text = script.statements.join(";\n");
    // The view on the retyped column is dropped first and created again, with a warning.
    assert!(text.contains(&format!("drop view {b}.emails")) && text.contains(&format!("create view {b}.emails")), "{text}");
    assert!(script.warnings.iter().any(|w| w.contains("lose their grants") && w.contains("emails")), "{:?}", script.warnings);
    // A primary key change keeps its view (and its grant); a compatible view is replaced.
    assert!(!text.contains(&format!("view {b}.tag_codes")), "{text}");
    assert_eq!(grants.unwrap()[0].rows[0][0].as_deref(), Some("1"), "the grant on tag_codes is kept");
    assert!(text.contains(&format!("create or replace view {b}.big_orders")), "{text}");
    assert!(text.contains("doubled numeric generated always as ((total * (2)::numeric)) stored"), "{text}");
    for moved in ["customers", "orders", "products"] {
        assert!(!text.contains(&format!("{a}.{moved}")), "{a}.{moved} left in {text}");
    }
}

/// From `public` to another schema of a database of its own: an extension type and a function
/// of `public` are shared, and must keep their `public.` qualifier.
#[tokio::test]
#[ignore]
async fn postgres_public_to_another_schema_keeps_shared_objects() {
    let Some(cfg) = pg_config() else { return };
    let tag = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let db = format!("dc_cmp_db_{tag}");
    let mut admin = Session::connect(&cfg).await.unwrap();
    admin.execute(&format!("create database {db}"), 10).await.unwrap();
    let mut own = cfg.clone();
    own.database.clone_from(&db);
    let mut session = Session::connect(&own).await.unwrap();
    session
        .execute(
            "create extension citext; create schema stage;
             create function public.f() returns int language sql as 'select 7';
             create table public.people (id serial primary key, email citext not null, n int default public.f(), u uuid default gen_random_uuid());
             create table public.contacts (id int primary key, person int references public.people (id), email citext, n int default public.f());
             create view public.emails as select email from public.people;
             create table stage.people (id serial primary key, email public.citext not null, n int default public.f());
             create view stage.emails as select email from stage.people;",
            10,
        )
        .await
        .unwrap();
    drop(session);
    let side = |schema: &str| Side { db: db.clone(), schema: schema.into() };
    let result = tokio::spawn({
        let (own, src, tgt) = (own.clone(), side("public"), side("stage"));
        async move { round_trip(own.clone(), own, src, tgt).await }
    })
    .await;
    admin.execute(&format!("drop database {db} with (force)"), 10).await.unwrap();
    let (before, script) = result.unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()));
    assert_eq!(before.objects.iter().find(|o| o.name == "emails").map(|o| o.status), Some(Status::Identical));
    let text = script.statements.join(";\n");
    assert!(text.contains("email public.citext") && text.contains("default public.f()"), "{text}");
    assert!(text.contains("references stage.people (id)"), "{text}");
    assert!(!text.contains("stage.citext") && !text.contains("stage.f()"), "{text}");
}
