use super::*;

fn col(name: &str, ty: &str) -> ColumnDef {
    ColumnDef { name: name.into(), data_type: ty.into(), nullable: true, ..Default::default() }
}

fn required(mut c: ColumnDef) -> ColumnDef {
    c.nullable = false;
    c
}

fn with_default(mut c: ColumnDef, d: &str) -> ColumnDef {
    c.default = Some(d.into());
    c
}

fn index(name: &str, columns: &[&str], unique: bool) -> IndexDef {
    IndexDef { name: name.into(), columns: columns.iter().map(|c| c.to_string()).collect(), unique, constraint: false, plain: true }
}

fn fk(name: &str, columns: &[&str], table: &str, ref_columns: &[&str]) -> ForeignKeyDef {
    ForeignKeyDef {
        name: name.into(),
        columns: columns.iter().map(|c| c.to_string()).collect(),
        ref_table: table.into(),
        ref_columns: ref_columns.iter().map(|c| c.to_string()).collect(),
        ..Default::default()
    }
}

fn table(name: &str, columns: Vec<ColumnDef>, pk: &[&str]) -> TableModel {
    TableModel { name: name.into(), details: TableDetails { columns, primary_key: pk.iter().map(|c| c.to_string()).collect(), ..Default::default() }, foreign_keys: vec![] }
}

fn view(name: &str, body: &str) -> ViewModel {
    ViewModel { name: name.into(), definition: ViewDefinition { body: body.into(), ..Default::default() } }
}

fn model(driver: Driver, schema: &str, tables: Vec<TableModel>, views: Vec<ViewModel>) -> SchemaModel {
    SchemaModel { driver, db: "db".into(), schema: schema.into(), tables, views }
}

fn status(c: &Comparison, name: &str) -> Status {
    c.objects.iter().find(|o| o.name == name).unwrap_or_else(|| panic!("{name} not compared")).status
}

fn all(c: &Comparison) -> Vec<bool> {
    vec![true; c.objects.len()]
}

/// Position of the first statement containing `needle`.
fn at(script: &Script, needle: &str) -> usize {
    script.statements.iter().position(|s| s.contains(needle)).unwrap_or_else(|| panic!("no statement with {needle:?} in {:#?}", script.statements))
}

#[test]
fn requalify_moves_bare_quoted_and_literal_qualifiers() {
    let d = Driver::Postgres;
    assert_eq!(requalify(d, "select * from shop.orders o join \"shop\".items i", "shop", "dev"), "select * from dev.orders o join dev.items i");
    assert_eq!(requalify(d, "nextval('shop.orders_id_seq'::regclass)", "shop", "shop_staging"), "nextval('shop_staging.orders_id_seq'::regclass)");
    assert_eq!(requalify(d, "shop.orders", "shop", ""), "orders");
    // Only a whole qualifier: not part of another name nor a column of an alias.
    assert_eq!(requalify(d, "myshop.x, o.shop.y, shop_x.z", "shop", "dev"), "myshop.x, o.shop.y, shop_x.z");
    assert_eq!(requalify(d, "Shop.T", "shop", "Dev"), "\"Dev\".T");
    assert_eq!(requalify(Driver::MsSql, "[dbo].[t] join dbo.u", "dbo", "stage"), "stage.[t] join stage.u");
}

#[test]
fn equivalent_spellings_compare_equal() {
    let pg = Driver::Postgres;
    assert_eq!(norm_type(pg, "s", "int4"), norm_type(pg, "s", "integer"));
    assert_eq!(norm_type(pg, "s", "varchar(20)"), norm_type(pg, "s", "character varying(20)"));
    assert_eq!(norm_type(pg, "s", "timestamptz"), norm_type(pg, "s", "timestamp with time zone"));
    assert_eq!(norm_type(pg, "s", "s.mood"), norm_type(pg, "x", "x.mood"));
    assert_ne!(norm_type(pg, "s", "numeric(10,2)"), norm_type(pg, "s", "numeric(12,2)"));
    let my = Driver::MySql;
    assert_eq!(norm_type(my, "s", "int(11)"), norm_type(my, "s", "int"));
    assert_eq!(norm_type(my, "s", "bigint(20) unsigned"), "bigint unsigned");
    assert_ne!(norm_type(my, "s", "tinyint(1)"), norm_type(my, "s", "tinyint(4)"));
    assert_eq!(norm_type(Driver::Sqlite, "main", "VARCHAR (20)"), norm_type(Driver::Sqlite, "main", "varchar(20)"));
    assert_eq!(norm_default(Driver::MsSql, "dbo", Some("((0))")), norm_default(Driver::MsSql, "dbo", Some("0")));
    assert_eq!(norm_default(Driver::MsSql, "dbo", Some("(getdate())")), Some("getdate()".into()));
    assert_eq!(norm_default(Driver::Postgres, "s", Some("NULL::character varying")), None);
    assert_eq!(norm_default(Driver::Sqlite, "main", Some("'A b'")), Some("'A b'".into()));
    assert_ne!(norm_default(Driver::Sqlite, "main", Some("'A b'")), norm_default(Driver::Sqlite, "main", Some("'a b'")));
    assert_eq!(strip_parens("(a) + (b)"), "(a) + (b)");
    assert_eq!(norm_rule(Driver::MySql, "RESTRICT"), "NO ACTION");
    assert_eq!(norm_rule(Driver::MsSql, "SET_NULL"), "SET NULL");
}

#[test]
fn equal_schemas_show_no_differences_despite_case_blanks_and_qualifiers() {
    let source = model(
        Driver::Sqlite,
        "main",
        vec![table("Orders", vec![required(col("ID", "INTEGER")), with_default(col("Note", "TEXT"), "'x'")], &["ID"])],
        vec![view("v", "SELECT id\n  FROM   Orders\nWHERE note = 'Hi';")],
    );
    let mut target = model(
        Driver::Sqlite,
        "main",
        vec![table("orders", vec![required(col("id", "integer")), with_default(col("note", "text"), "'x'")], &["id"])],
        vec![view("V", "select id from orders where note = 'Hi'")],
    );
    let c = compare(&source, &target);
    assert!(!c.has_differences(), "{c:#?}");
    assert_eq!(c.count(Status::Identical), 2);
    // String literals keep their case.
    target.views[0].definition.body = "select id from orders where note = 'hi'".into();
    assert_eq!(status(&compare(&source, &target), "v"), Status::Different);
    // PostgreSQL: names are case-sensitive, each side's own schema qualifier is ignored.
    let (src, tgt) = demo_models();
    let c = compare(&src, &src.clone());
    assert!(!c.has_differences());
    let same = SchemaModel { schema: "elsewhere".into(), ..src.clone() };
    let moved: Vec<ViewModel> = same.views.iter().map(|v| ViewModel { definition: ViewDefinition { body: requalify(Driver::Postgres, &v.definition.body, "shop", "elsewhere"), ..v.definition.clone() }, ..v.clone() }).collect();
    let mut moved_model = SchemaModel { views: moved, ..same };
    for c in moved_model.tables.iter_mut().flat_map(|t| t.details.columns.iter_mut()) {
        c.default = c.default.as_deref().map(|d| requalify(Driver::Postgres, d, "shop", "elsewhere"));
    }
    for f in moved_model.tables.iter_mut().flat_map(|t| t.foreign_keys.iter_mut()) {
        f.ref_schema = "elsewhere".into();
    }
    assert!(!compare(&src, &moved_model).has_differences(), "{:#?}", compare(&src, &moved_model));
    assert!(compare(&src, &tgt).has_differences());
}

#[test]
fn statuses_and_changes_are_reported() {
    let (src, tgt) = demo_models();
    let c = compare(&src, &tgt);
    assert_eq!(status(&c, "payments"), Status::OnlyInSource);
    assert_eq!(status(&c, "coupons"), Status::OnlyInTarget);
    assert_eq!(status(&c, "orders"), Status::Different);
    assert_eq!(status(&c, "customers"), Status::Identical);
    assert_eq!(status(&c, "order_totals"), Status::Different);
    assert_eq!(status(&c, "active_products"), Status::Identical);
    // Sorted by status: only in source first, identical last.
    let order: Vec<Status> = c.objects.iter().map(|o| o.status).collect();
    let mut sorted = order.clone();
    sorted.sort();
    assert_eq!(order, sorted);
    let orders = c.objects.iter().find(|o| o.name == "orders").unwrap();
    let changes = orders.changes.join("\n");
    for expected in [
        "Column “shipped_at” added",
        "Column “status”: type character varying(10) → character varying(20)",
        "Column “status”: default (none) → 'pending'::character varying",
        "Column “total”: type numeric(10,2) → numeric(12,2)",
        "Index “orders_status_idx” added",
    ] {
        assert!(changes.contains(expected), "{expected:?} not in {changes}");
    }
    // Drops and identical objects are left out of the script at first.
    let selection = c.default_selection();
    for (o, on) in c.objects.iter().zip(&selection) {
        assert_eq!(*on, matches!(o.status, Status::OnlyInSource | Status::Different), "{}", o.name);
    }
    // The definitions differ only on the changed lines.
    let (a, b) = changed_lines(&orders.source_ddl, &orders.target_ddl);
    let changed_src: Vec<&str> = orders.source_ddl.lines().zip(&a).filter(|(_, c)| **c).map(|(l, _)| l).collect();
    let changed_tgt: Vec<&str> = orders.target_ddl.lines().zip(&b).filter(|(_, c)| **c).map(|(l, _)| l).collect();
    assert!(changed_src.iter().any(|l| l.contains("shipped_at")) && changed_src.iter().any(|l| l.contains("orders_status_idx")), "{changed_src:?}");
    assert!(changed_tgt.iter().all(|l| l.contains("status") || l.contains("total")), "{changed_tgt:?}");
    assert!(!changed_src.iter().any(|l| l.contains("customer_id integer")));
}

#[test]
fn changed_lines_marks_only_lines_outside_the_common_sequence() {
    let (a, b) = changed_lines("a\nb\nc\nd", "a\nc\nX\nd");
    assert_eq!(a, [false, true, false, false]);
    assert_eq!(b, [false, false, true, false]);
    let (a, b) = changed_lines("", "x");
    assert!(a.is_empty());
    assert_eq!(b, [true]);
}

#[test]
fn postgres_script_orders_statements_safely() {
    let (src, tgt) = demo_models();
    let c = compare(&src, &tgt);
    let script = script(&src, &tgt, &c, &all(&c)).unwrap();
    let text = script.text(&src, &tgt);
    // The created table's serial key and the qualifiers move to the target schema.
    assert!(text.contains("create table shop_staging.payments"), "{text}");
    assert!(text.contains("id serial not null"), "{text}");
    assert!(!script.statements.join("
").contains(" shop."), "{text}");
    // Drops come before creates, tables before their foreign keys, views last.
    let drop_coupons = at(&script, "drop table shop_staging.coupons");
    let create_payments = at(&script, "create table shop_staging.payments");
    let payments_fk = at(&script, "alter table shop_staging.payments add constraint payments_order_id_fkey");
    assert!(drop_coupons < create_payments && create_payments < payments_fk);
    assert!(at(&script, "create index payments_order_idx") > create_payments);
    // `status` is retyped: the view on it goes first and comes back after the ALTER.
    let drop_view = at(&script, "drop view shop_staging.order_totals");
    let retype = at(&script, "alter column status type");
    let create_view = at(&script, "create view shop_staging.order_totals");
    assert!(drop_view < retype && retype < create_view, "{text}");
    assert!(script.statements[create_view].contains("'cancelled'"));
    assert!(text.contains("add column shipped_at timestamp with time zone"));
    assert!(text.contains("alter column status set default 'pending'::character varying"));
    assert!(text.contains("create index orders_status_idx on shop_staging.orders (status, created_at)"));
    // Unselected drops stay out.
    let selection = c.default_selection();
    let script = super::script(&src, &tgt, &c, &selection).unwrap();
    assert!(!script.statements.iter().any(|s| s.contains("coupons")));
}

#[test]
fn foreign_keys_go_before_the_columns_and_tables_they_need() {
    let parent = |id_type: &str| {
        let mut t = table("parent", vec![required(col("id", id_type)), col("code", "text")], &["id"]);
        t.details.pk_name = Some("parent_pkey".into());
        t
    };
    let child = || {
        let mut t = table("child", vec![required(col("id", "integer")), col("parent_id", "integer")], &["id"]);
        t.foreign_keys.push(fk("child_parent_fk", &["parent_id"], "parent", &["id"]));
        t
    };
    // Retyping the referenced key drops the key first and adds it back afterwards.
    let src = model(Driver::Postgres, "a", vec![parent("bigint"), child()], vec![]);
    let tgt = model(Driver::Postgres, "b", vec![parent("integer"), child()], vec![]);
    let c = compare(&src, &tgt);
    assert_eq!(status(&c, "child"), Status::Identical);
    let s = script(&src, &tgt, &c, &all(&c)).unwrap();
    let drop = at(&s, "alter table b.child drop constraint child_parent_fk");
    let retype = at(&s, "alter column id type bigint");
    let add = at(&s, "alter table b.child add constraint child_parent_fk foreign key (parent_id) references b.parent (id)");
    assert!(drop < retype && retype < add, "{:#?}", s.statements);
    // Dropping a referenced table drops the keys on it first; MySQL spells it DROP FOREIGN KEY.
    for (driver, statement) in [(Driver::Postgres, "alter table b.child drop constraint child_parent_fk"), (Driver::MySql, "alter table b.child drop foreign key child_parent_fk")] {
        let src = model(driver, "a", vec![child()], vec![]);
        let mut child_without = child();
        child_without.foreign_keys.clear();
        let src = SchemaModel { tables: vec![child_without], ..src };
        let tgt = model(driver, "b", vec![parent("integer"), child()], vec![]);
        let c = compare(&src, &tgt);
        let s = script(&src, &tgt, &c, &all(&c)).unwrap();
        assert!(at(&s, statement) < at(&s, "drop table b.parent"), "{:#?}", s.statements);
        assert_eq!(s.statements.iter().filter(|x| x.contains("child_parent_fk")).count(), 1, "{:#?}", s.statements);
    }
    // A key added to an existing table, and one on a new table, come after every table exists.
    let src = model(Driver::MsSql, "dbo", vec![parent("int"), child()], vec![]);
    let tgt = model(Driver::MsSql, "stage", vec![table("child", vec![required(col("id", "int")), col("parent_id", "int")], &["id"])], vec![]);
    let c = compare(&src, &tgt);
    let s = script(&src, &tgt, &c, &all(&c)).unwrap();
    assert!(at(&s, "create table stage.parent") < at(&s, "alter table stage.child add constraint child_parent_fk"));
    assert!(s.statements.iter().any(|x| x == "alter table stage.child add constraint child_parent_fk foreign key (parent_id) references stage.parent (id)"));
}

#[test]
fn sqlite_rebuilds_tables_it_cannot_alter_and_keeps_simple_changes_in_place() {
    let mut orders = table("orders", vec![required(col("id", "INTEGER")), col("customer_id", "INTEGER"), required(col("total", "REAL"))], &["id"]);
    orders.foreign_keys.push(ForeignKeyDef { on_delete: "CASCADE".into(), ..fk("", &["customer_id"], "customers", &["id"]) });
    orders.details.indexes.push(index("orders_customer", &["customer_id"], false));
    orders.details.autoincrement = true;
    let mut old = table("orders", vec![required(col("id", "INTEGER")), col("total", "TEXT"), col("legacy", "TEXT")], &["id"]);
    old.details.autoincrement = true;
    let customers = table("customers", vec![required(col("id", "INTEGER")), col("email", "TEXT"), with_default(col("tier", "TEXT"), "'basic'")], &["id"]);
    let old_customers = table("customers", vec![required(col("id", "INTEGER")), col("email", "TEXT")], &["id"]);
    let src = model(Driver::Sqlite, "main", vec![customers, orders], vec![view("big", "select * from orders where total > 100")]);
    let tgt = model(Driver::Sqlite, "main", vec![old_customers, old], vec![view("big", "select * from orders where total > 1000")]);
    let c = compare(&src, &tgt);
    let s = script(&src, &tgt, &c, &all(&c)).unwrap();
    let text = s.text(&src, &tgt);
    assert_eq!(&s.statements[..2], ["pragma foreign_keys = off", "pragma legacy_alter_table = on"]);
    // customers only gains a column with a constant default: ALTER TABLE.
    assert!(s.statements.iter().any(|x| x == "alter table customers add column tier TEXT default 'basic'"), "{text}");
    let create = at(&s, "create table _dc_new_orders");
    assert!(s.statements[create].contains("id INTEGER not null primary key autoincrement"), "{text}");
    assert!(s.statements[create].contains("foreign key (customer_id) references customers (id) on delete cascade"), "{text}");
    let copy = at(&s, "insert into _dc_new_orders (id, total) select id, total from orders");
    let drop = at(&s, "drop table orders");
    let rename = at(&s, "alter table _dc_new_orders rename to orders");
    let index = at(&s, "create index orders_customer on orders (customer_id)");
    assert!(create < copy && copy < drop && drop < rename && rename < index, "{text}");
    // SQLite has no OR REPLACE: the changed view is dropped and created.
    assert!(at(&s, "drop view big") < at(&s, "create view big as"));
    assert!(s.warnings.iter().any(|w| w.contains("CHECK constraints")));
}

#[test]
fn different_engines_compare_but_get_no_script() {
    let a = model(Driver::Postgres, "public", vec![table("t", vec![col("id", "integer")], &[])], vec![]);
    let b = model(Driver::MySql, "shop", vec![table("T", vec![col("ID", "int")], &[])], vec![]);
    let c = compare(&a, &b);
    assert_eq!(c.objects.len(), 1, "names match ignoring case across engines");
    assert!(script(&a, &b, &c, &all(&c)).is_err());
    assert!(same_dialect(Driver::MySql, Driver::MariaDb));
    let maria = SchemaModel { driver: Driver::MariaDb, ..b.clone() };
    assert!(script(&maria, &b, &compare(&maria, &b), &[]).is_ok());
}

#[test]
fn identity_changes_and_new_identity_columns_are_scripted() {
    let ident = |extra: &str| ColumnDef { extra: extra.into(), ..required(col("id", "integer")) };
    let src = model(Driver::Postgres, "a", vec![table("t", vec![ident("generated always as identity"), col("x", "text")], &["id"]), table("n", vec![ident("generated by default as identity")], &["id"])], vec![]);
    let tgt = model(Driver::Postgres, "b", vec![table("t", vec![ident(""), col("x", "text")], &["id"])], vec![]);
    let c = compare(&src, &tgt);
    let s = script(&src, &tgt, &c, &all(&c)).unwrap();
    assert!(s.statements.iter().any(|x| x == "alter table b.t alter column id add generated always as identity"), "{:#?}", s.statements);
    assert!(s.statements.iter().any(|x| x.contains("id integer generated by default as identity not null")), "{:#?}", s.statements);
    let my_src = model(Driver::MySql, "a", vec![table("t", vec![ColumnDef { extra: "auto_increment".into(), ..required(col("id", "int")) }], &["id"])], vec![]);
    let my_tgt = model(Driver::MySql, "b", vec![table("t", vec![required(col("id", "int(11)"))], &["id"])], vec![]);
    let c = compare(&my_src, &my_tgt);
    let s = script(&my_src, &my_tgt, &c, &all(&c)).unwrap();
    assert_eq!(s.statements, ["alter table b.t modify column id int not null auto_increment"]);
}

#[test]
fn expression_indexes_and_generated_columns_are_reported_not_scripted() {
    let mut s = table("t", vec![ColumnDef { generated: true, ..col("g", "integer") }], &[]);
    s.details.indexes.push(IndexDef { plain: false, ..index("t_expr", &["(expression)"], false) });
    let src = model(Driver::Postgres, "a", vec![s], vec![]);
    let tgt = model(Driver::Postgres, "b", vec![], vec![]);
    let c = compare(&src, &tgt);
    let script = script(&src, &tgt, &c, &all(&c)).unwrap();
    assert!(!script.statements.iter().any(|x| x.contains("t_expr")));
    assert_eq!(script.warnings.len(), 2, "{:?}", script.warnings);
    assert!(script.text(&src, &tgt).starts_with("-- Makes db.b match db.a\n-- "));
}
