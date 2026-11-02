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
    TableModel { name: name.into(), details: TableDetails { columns, primary_key: pk.iter().map(|c| c.to_string()).collect(), ..Default::default() }, ..Default::default() }
}

fn view(name: &str, body: &str) -> ViewModel {
    ViewModel { name: name.into(), definition: ViewDefinition { body: body.into(), ..Default::default() }, ..Default::default() }
}

/// A PostgreSQL view with its columns and the (table, column) pairs it uses.
fn pg_view(name: &str, body: &str, columns: &[(&str, &str)], uses: &[(&str, &str)]) -> ViewModel {
    ViewModel {
        columns: columns.iter().map(|(n, t)| (n.to_string(), t.to_string())).collect(),
        uses: uses.iter().map(|(t, c)| (t.to_string(), c.to_string())).collect(),
        ..view(name, body)
    }
}

fn model(driver: Driver, schema: &str, tables: Vec<TableModel>, views: Vec<ViewModel>) -> SchemaModel {
    SchemaModel { tables, views, ..SchemaModel::new(driver, "db", schema) }
}

/// Names of a schema with these tables (and nothing else).
fn names(driver: Driver, tables: &[&str]) -> Names {
    Names::of(&model(driver, "s", tables.iter().map(|t| table(t, vec![], &[])).collect(), vec![]))
}

fn status(c: &Comparison, name: &str) -> Status {
    c.objects.iter().find(|o| o.name == name).unwrap_or_else(|| panic!("{name} not compared")).status
}

fn all(c: &Comparison) -> Vec<bool> {
    vec![true; c.objects.len()]
}

/// The statements without the transaction around them.
fn body(script: &Script) -> Vec<String> {
    let control = ["begin", "commit", "set xact_abort on", "begin transaction", "commit transaction", "set noexec off", MSSQL_GUARD];
    script.statements.iter().filter(|x| !control.contains(&x.as_str())).cloned().collect()
}

/// Position of the first statement containing `needle`.
fn at(script: &Script, needle: &str) -> usize {
    script.statements.iter().position(|s| s.contains(needle)).unwrap_or_else(|| panic!("no statement with {needle:?} in {:#?}", script.statements))
}

#[test]
fn requalify_moves_only_references_to_the_compared_objects() {
    let d = Driver::Postgres;
    let n = {
        let mut m = model(d, "shop", ["orders", "items", "t", "shop"].iter().map(|t| table(t, vec![], &[])).collect(), vec![]);
        m.sequences.push("orders_id_seq".into());
        Names::of(&m)
    };
    assert_eq!(requalify(d, "select * from shop.orders o join \"shop\".items i", "shop", "dev", &n), "select * from dev.orders o join dev.items i");
    assert_eq!(requalify(d, "nextval('shop.orders_id_seq'::regclass)", "shop", "shop_staging", &n), "nextval('shop_staging.orders_id_seq'::regclass)");
    assert_eq!(requalify(d, "shop.orders", "shop", "", &n), "orders");
    // Only a whole qualifier: not part of another name nor a column of an alias.
    assert_eq!(requalify(d, "myshop.x, o.shop.y, shop_x.z", "shop", "dev", &n), "myshop.x, o.shop.y, shop_x.z");
    assert_eq!(requalify(d, "Shop.T", "shop", "Dev", &n), "\"Dev\".T");
    // A table named like its schema: `shop.id` is that table's column, `shop.shop` the table.
    assert_eq!(requalify(d, "select shop.id from shop.shop join shop.items on items.id = shop.id", "shop", "dev", &n), "select shop.id from dev.shop join dev.items on items.id = shop.id");
    // Literals, comments, functions, types and sequences of no compared object stay.
    for text in ["'shop.orders'", "default 'shop.home'", "-- shop.orders", "/* shop.orders */", "shop.f()", "shop.mood", "nextval('shop.other_seq'::regclass)", "x::shop.mood", "$$shop.orders$$"] {
        assert_eq!(requalify(d, text, "shop", "dev", &n), text, "{text}");
    }
    assert_eq!(requalify(Driver::MsSql, "[dbo].[t] join dbo.items, dbo.fn_total(1), 'dbo.t'", "dbo", "stage", &names(Driver::MsSql, &["t", "items"])), "stage.[t] join stage.items, dbo.fn_total(1), 'dbo.t'");
    // MySQL: backslash escapes inside literals don't end them early.
    let text = "concat('a\\'shop.items', `shop`.`items`.x)";
    assert_eq!(requalify(Driver::MySql, text, "shop", "dev", &names(Driver::MySql, &["items"])), "concat('a\\'shop.items', dev.`items`.x)");
}

#[test]
fn equivalent_spellings_compare_equal() {
    let pg = Driver::Postgres;
    let n = names(pg, &[]);
    assert_eq!(norm_type(pg, "s", &n, "int4"), norm_type(pg, "s", &n, "integer"));
    assert_eq!(norm_type(pg, "s", &n, "varchar(20)"), norm_type(pg, "s", &n, "character varying(20)"));
    assert_eq!(norm_type(pg, "s", &n, "timestamptz"), norm_type(pg, "s", &n, "timestamp with time zone"));
    // Each schema's own type compares equal; another schema's type does not.
    let mood = |schema: &str| Names::of(&SchemaModel { types: vec!["mood".into()], ..SchemaModel::new(pg, "db", schema) });
    assert_eq!(norm_type(pg, "s", &mood("s"), "s.mood"), norm_type(pg, "x", &mood("x"), "x.mood"));
    assert_ne!(norm_type(pg, "s", &n, "s.mood"), norm_type(pg, "x", &n, "x.mood"));
    assert_ne!(norm_type(pg, "s", &n, "numeric(10,2)"), norm_type(pg, "s", &n, "numeric(12,2)"));
    let my = Driver::MySql;
    assert_eq!(norm_type(my, "s", &n, "int(11)"), norm_type(my, "s", &n, "int"));
    assert_eq!(norm_type(my, "s", &n, "bigint(20) unsigned"), "bigint unsigned");
    assert_ne!(norm_type(my, "s", &n, "tinyint(1)"), norm_type(my, "s", &n, "tinyint(4)"));
    assert_eq!(norm_type(Driver::Sqlite, "main", &n, "VARCHAR (20)"), norm_type(Driver::Sqlite, "main", &n, "varchar(20)"));
    assert_eq!(norm_default(Driver::MsSql, "dbo", &n, Some("((0))")), norm_default(Driver::MsSql, "dbo", &n, Some("0")));
    assert_eq!(norm_default(Driver::MsSql, "dbo", &n, Some("(getdate())")), Some("getdate()".into()));
    assert_eq!(norm_default(Driver::Postgres, "s", &n, Some("NULL::character varying")), None);
    assert_eq!(norm_default(Driver::Sqlite, "main", &n, Some("'A b'")), Some("'A b'".into()));
    assert_ne!(norm_default(Driver::Sqlite, "main", &n, Some("'A b'")), norm_default(Driver::Sqlite, "main", &n, Some("'a b'")));
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
    let n = Names::of(&src);
    let moved: Vec<ViewModel> =
        same.views.iter().map(|v| ViewModel { definition: ViewDefinition { body: requalify(Driver::Postgres, &v.definition.body, "shop", "elsewhere", &n), ..v.definition.clone() }, ..v.clone() }).collect();
    let mut moved_model = SchemaModel { views: moved, ..same };
    for c in moved_model.tables.iter_mut().flat_map(|t| t.details.columns.iter_mut()) {
        c.default = c.default.as_deref().map(|d| requalify(Driver::Postgres, d, "shop", "elsewhere", &n));
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
    // Drops, changes that drop columns or change their type, and identical objects are left
    // out of the script at first.
    let selection = c.default_selection();
    assert!(orders.destructive && !c.objects.iter().find(|o| o.name == "order_totals").unwrap().destructive);
    for (o, on) in c.objects.iter().zip(&selection) {
        assert_eq!(*on, matches!(o.status, Status::OnlyInSource | Status::Different) && !o.destructive, "{}", o.name);
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
    assert!(!s.warnings.iter().any(|w| w.contains("CHECK constraints")), "rebuilds are faithful now");
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
    assert!(script.warnings.iter().any(|w| w.contains("t.g is a computed column")), "{:?}", script.warnings);
    assert!(script.warnings.iter().any(|w| w.contains("“t_expr” is an expression")), "{:?}", script.warnings);
    assert!(script.text(&src, &tgt).starts_with("-- Makes db.b match db.a\n-- "));
}

#[test]
fn a_table_named_like_its_schema_and_shared_objects_compare_equal_and_stay_shared() {
    // Reviewer repro: schema rvwa has a table rvwa; defaults use a function of rvwa and a literal
    // mentioning it. The staging schema rvwb references the same shared function.
    let d = Driver::Postgres;
    let t = |schema: &str| {
        let mut x = table("rvwa", vec![required(col("id", "integer")), col("v", "text"), with_default(col("x", "integer"), "rvwa.f()"), with_default(col("s", "text"), "'rvwa.home'::text")], &["id"]);
        x.details.pk_name = Some(format!("{schema}_pkey"));
        x
    };
    let body = |schema: &str| format!(" SELECT rvwa.id,\n    other.w\n   FROM {schema}.rvwa\n     JOIN {schema}.other ON other.id = rvwa.id;");
    let side = |schema: &str| {
        let v = pg_view("v2", &body(schema), &[("id", "integer"), ("w", "text")], &[("rvwa", "id"), ("other", "id"), ("other", "w")]);
        model(d, schema, vec![t(schema), table("other", vec![col("id", "integer"), col("w", "text")], &[])], vec![v])
    };
    let (src, tgt) = (side("rvwa"), side("rvwb"));
    let c = compare(&src, &tgt);
    assert!(!c.has_differences(), "{:#?}", c.objects.iter().filter(|o| o.status != Status::Identical).collect::<Vec<_>>());
    // A new table and view keep the shared function and the literal, and move the own tables.
    let empty = model(d, "rvwb", vec![], vec![]);
    let c = compare(&src, &empty);
    let script = script(&src, &empty, &c, &c.default_selection()).unwrap();
    let text = script.statements.join(";\n");
    assert!(text.contains("x integer default rvwa.f()") && text.contains("default 'rvwa.home'::text"), "{text}");
    assert!(text.contains("SELECT rvwa.id,") && text.contains("FROM rvwb.rvwa") && text.contains("JOIN rvwb.other ON other.id = rvwa.id"), "{text}");
}

#[test]
fn postgres_views_are_replaced_when_they_can_be_and_dropped_only_when_a_column_blocks_them() {
    let d = Driver::Postgres;
    let orders = |status_type: &str, pk: &[&str]| table("orders", vec![required(col("id", "integer")), required(col("code", "integer")), col("status", status_type), col("note", "text")], pk);
    let report = |extra: bool| {
        let mut cols = vec![("id", "integer"), ("status", "text")];
        if extra {
            cols.push(("note", "text"));
        }
        let body = if extra { " SELECT id, status, note FROM s.orders;" } else { " SELECT id, status FROM s.orders;" };
        pg_view("report", body, &cols, &[("orders", "id"), ("orders", "status")])
    };
    let notes = pg_view("notes", " SELECT note FROM s.orders;", &[("note", "text")], &[("orders", "note")]);
    let on_notes = pg_view("on_notes", " SELECT note FROM s.notes;", &[("note", "text")], &[("notes", "note")]);
    // The primary key changes and `report` gains a column at the end: nothing is dropped, the
    // view is replaced in place.
    let src = model(d, "s", vec![orders("text", &["id", "code"])], vec![report(true), notes.clone(), on_notes.clone()]);
    let tgt = model(d, "s", vec![orders("text", &["id"])], vec![report(false), notes.clone(), on_notes.clone()]);
    let c = compare(&src, &tgt);
    let s = script(&src, &tgt, &c, &all(&c)).unwrap();
    assert!(!s.statements.iter().any(|x| x.starts_with("drop view")), "{:#?}", s.statements);
    assert!(s.statements.iter().any(|x| x.starts_with("create or replace view s.report")), "{:#?}", s.statements);
    assert!(!s.warnings.iter().any(|w| w.contains("Views dropped")));
    // Dropping a column the view shows can't be done by OR REPLACE.
    let c = compare(&tgt, &src);
    let s = script(&tgt, &src, &c, &all(&c)).unwrap();
    assert!(at(&s, "drop view s.report") < at(&s, "create view s.report"), "{:#?}", s.statements);
    // Retyping `note` drops the views on it (and the views on those), and creates them again
    // with their definition, with a warning naming them.
    let src = model(d, "s", vec![orders("character varying(20)", &["id"])], vec![report(false), notes.clone(), on_notes.clone()]);
    let tgt = model(d, "s", vec![table("orders", vec![required(col("id", "integer")), required(col("code", "integer")), col("status", "character varying(20)"), col("note", "integer")], &["id"])], vec![report(false), notes.clone(), on_notes.clone()]);
    let c = compare(&src, &tgt);
    let s = script(&src, &tgt, &c, &all(&c)).unwrap();
    assert!(at(&s, "drop view s.on_notes") < at(&s, "drop view s.notes"));
    assert!(at(&s, "drop view s.notes") < at(&s, "alter column note type"));
    assert!(at(&s, "alter column note type") < at(&s, "create view s.notes") && at(&s, "create view s.notes") < at(&s, "create view s.on_notes"));
    assert!(!s.statements.iter().any(|x| x.contains("view s.report")), "report doesn't use note: {:#?}", s.statements);
    assert!(s.warnings.iter().any(|w| w.contains("lose their grants") && w.contains("notes, on_notes")), "{:?}", s.warnings);
    // An unselected view on a dropped table is dropped with it, not created again.
    let src = model(d, "s", vec![], vec![]);
    let tgt = model(d, "s", vec![orders("text", &["id"])], vec![notes]);
    let c = compare(&src, &tgt);
    let only_table: Vec<bool> = c.objects.iter().map(|o| o.kind == Kind::Table).collect();
    let s = script(&src, &tgt, &c, &only_table).unwrap();
    assert!(at(&s, "drop view s.notes") < at(&s, "drop table s.orders"));
    assert!(!s.statements.iter().any(|x| x.starts_with("create")), "{:#?}", s.statements);
    assert!(s.warnings.iter().any(|w| w.contains("“notes” uses a dropped table")), "{:?}", s.warnings);
    assert!(s.warnings.iter().any(|w| w.contains("Drops table “orders”")), "{:?}", s.warnings);
}

#[test]
fn sqlite_rebuilds_warn_about_lost_data_and_restore_the_session() {
    let src = model(Driver::Sqlite, "main", vec![table("a", vec![required(col("id", "INTEGER")), col("n", "TEXT")], &["id"])], vec![]);
    let mut tgt = model(Driver::Sqlite, "main", vec![table("a", vec![required(col("id", "INTEGER")), col("n", "INTEGER"), col("legacy", "TEXT")], &["id"])], vec![]);
    tgt.pragmas = vec![("foreign_keys".into(), "on".into()), ("legacy_alter_table".into(), "off".into())];
    let c = compare(&src, &tgt);
    let s = script(&src, &tgt, &c, &all(&c)).unwrap();
    for w in ["Drops column “legacy” and all its data", "Changing the type of “n” can truncate", "Run the script as a whole"] {
        assert!(s.warnings.iter().any(|x| x.contains(w)), "{w}: {:?}", s.warnings);
    }
    assert_eq!(&s.statements[..2], ["pragma foreign_keys = off", "pragma legacy_alter_table = on"]);
    let n = s.statements.len();
    assert_eq!(&s.statements[n - 2..], ["pragma foreign_keys = on", "pragma legacy_alter_table = off"]);
    // Run as SQLite runs a console script (one batch): the settings come back.
    let db = rusqlite::Connection::open_in_memory().unwrap();
    db.execute_batch("pragma foreign_keys = on; create table a (id INTEGER not null, n INTEGER, legacy TEXT, primary key (id)); insert into a values (1, 5, 'x');").unwrap();
    db.execute_batch(&s.text(&src, &tgt)).unwrap();
    assert_eq!(db.query_row("pragma foreign_keys", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    assert_eq!(db.query_row("pragma legacy_alter_table", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    assert_eq!(db.query_row("select n from a", [], |r| r.get::<_, String>(0)).unwrap(), "5");
}

#[test]
fn sql_server_scripts_are_split_into_go_batches() {
    let src = model(Driver::MsSql, "dbo", vec![table("t", vec![required(col("id", "int"))], &["id"])], vec![view("v", "select id from dbo.t")]);
    let tgt = model(Driver::MsSql, "stage", vec![], vec![]);
    let c = compare(&src, &tgt);
    let s = script(&src, &tgt, &c, &all(&c)).unwrap();
    let text = s.text(&src, &tgt);
    let batches = crate::sql::split_batches(&text, Driver::MsSql);
    assert_eq!(batches.len(), s.statements.len(), "{text}");
    let view = batches.iter().find(|b| b.text.contains("view")).unwrap();
    assert!(view.text.trim_start().starts_with("create or alter view stage.v as"), "{}", view.text);
    assert!(!text.contains(';'), "{text}");
}

#[test]
fn mysql_generated_columns_are_written_with_their_expression_or_left_out() {
    let d = Driver::MySql;
    let generated = |name: &str, extra: &str| ColumnDef { generated: true, extra: extra.into(), ..col(name, "int") };
    let src = model(d, "a", vec![table("g", vec![required(col("id", "int")), generated("dbl", "generated always as ((`id` * 2)) virtual"), generated("raw", "VIRTUAL GENERATED")], &["id"])], vec![]);
    let tgt = model(d, "b", vec![], vec![]);
    let c = compare(&src, &tgt);
    let s = script(&src, &tgt, &c, &all(&c)).unwrap();
    let create = &s.statements[at(&s, "create table b.g")];
    assert!(create.contains("dbl int generated always as ((`id` * 2)) virtual"), "{create}");
    assert!(!create.contains("raw") && !create.to_lowercase().contains("virtual generated"), "{create}");
    assert!(s.warnings.iter().any(|w| w.contains("g.raw is a computed column whose expression isn't available")), "{:?}", s.warnings);
    // auto_increment added on its own: MODIFY keeps the comment and the collation.
    let col_with = |extra: &str| ColumnDef { extra: extra.into(), comment: "Order's id".into(), collation: Some("utf8mb4_bin".into()), ..required(col("code", "varchar(20)")) };
    let src = model(d, "a", vec![table("t", vec![col_with("on update current_timestamp")], &["code"])], vec![]);
    let tgt = model(d, "b", vec![table("t", vec![col_with("")], &["code"])], vec![]);
    let c = compare(&src, &tgt);
    let s = script(&src, &tgt, &c, &all(&c)).unwrap();
    assert_eq!(s.statements, ["alter table b.t modify column code varchar(20) collate utf8mb4_bin not null on update current_timestamp comment 'Order''s id'"]);
}

#[test]
fn skipped_objects_are_reported_and_never_count_as_no_differences() {
    let mut src = model(Driver::Postgres, "s", vec![], vec![]);
    src.skipped.push("events (partitioned table)".into());
    let tgt = model(Driver::Postgres, "s", vec![], vec![]);
    let c = compare(&src, &tgt);
    assert!(!c.has_differences() && !c.complete());
    assert_eq!(c.skipped_source, ["events (partitioned table)"]);
    assert!(compare(&tgt, &tgt).complete());
}

#[test]
fn long_warnings_wrap_into_comment_lines() {
    let m = model(Driver::Postgres, "s", vec![], vec![]);
    let script = Script { statements: vec!["select 1".into()], warnings: vec!["word ".repeat(60)] };
    let text = script.text(&m, &m);
    let lines: Vec<&str> = text.lines().skip(1).take_while(|l| l.starts_with("--")).collect();
    assert!(lines.len() >= 3 && lines.iter().all(|l| l.len() <= 101), "{text}");
    assert!(lines[1..].iter().all(|l| l.starts_with("--   word")), "{text}");
    assert_eq!(lines.iter().map(|l| l.matches("word").count()).sum::<usize>(), 60);
}

#[test]
fn sqlite_definitions_are_parsed_for_constraints_options_and_generated_columns() {
    let sql = "CREATE TABLE \"Orders\" (id INTEGER PRIMARY KEY, code TEXT COLLATE NOCASE NOT NULL ON CONFLICT REPLACE, qty INT CHECK (qty > 0),
        price REAL, total REAL GENERATED ALWAYS AS (qty * price) STORED, label TEXT AS (upper(code)),
        CONSTRAINT positive CHECK (price >= 0), UNIQUE (code) ON CONFLICT IGNORE) WITHOUT ROWID, STRICT";
    let (features, generated) = sqlite_features(sql);
    assert_eq!(
        features,
        [
            "check(price>=0)",
            "code collate nocase",
            "code on conflict replace",
            "qty check(qty>0)",
            "strict",
            "unique on conflict ignore",
            "without rowid"
        ],
        "{features:?}"
    );
    assert_eq!(generated["total"], "generated always as (qty * price) stored");
    assert_eq!(generated["label"], "generated always as (upper(code)) virtual");
    assert_eq!(sqlite_retarget(sql, "_dc_new_Orders").unwrap()[..29].to_string(), "CREATE TABLE _dc_new_Orders (");
    assert_eq!(sqlite_retarget("create table if not exists main.t (a)", "x").unwrap(), "create table if not exists x (a)");
    assert_eq!(sqlite_retarget("create view v as select 1", "x"), None);
}

#[test]
fn sqlite_tables_differing_in_generated_columns_or_options_are_different_and_rebuilt_faithfully() {
    let sqlite = |sql: &str, columns: Vec<ColumnDef>| {
        let (features, generated) = sqlite_features(sql);
        let mut t = table("t", columns, &["id"]);
        for c in t.details.columns.iter_mut().filter(|c| c.generated) {
            c.extra = generated[&c.name.to_lowercase()].clone();
        }
        TableModel { sql: sql.into(), features, ..t }
    };
    let plain = || vec![required(col("id", "INTEGER")), col("qty", "INT")];
    let with_generated = || {
        let mut c = plain();
        c.push(ColumnDef { generated: true, ..col("twice", "INT") });
        c
    };
    // A generated column only in the source: different, and the rebuild keeps it.
    let src = model(Driver::Sqlite, "main", vec![sqlite("create table t (id INTEGER primary key, qty INT, twice INT as (qty * 2) stored)", with_generated())], vec![]);
    let tgt = model(Driver::Sqlite, "main", vec![sqlite("create table t (id INTEGER primary key, qty INT)", plain())], vec![]);
    let c = compare(&src, &tgt);
    assert_eq!(status(&c, "t"), Status::Different);
    let s = script(&src, &tgt, &c, &all(&c)).unwrap();
    assert!(s.statements.iter().any(|x| x == "create table _dc_new_t (id INTEGER primary key, qty INT, twice INT as (qty * 2) stored)"), "{:#?}", s.statements);
    assert!(s.statements.iter().any(|x| x == "insert into _dc_new_t (id, qty) select id, qty from t"), "generated columns are not copied: {:#?}", s.statements);
    // WITHOUT ROWID / STRICT only on one side: different too.
    let src = model(Driver::Sqlite, "main", vec![sqlite("create table t (id INTEGER primary key, qty INT) without rowid, strict", plain())], vec![]);
    let c = compare(&src, &tgt);
    assert_eq!(c.objects[0].changes, ["Constraint or option “strict” added", "Constraint or option “without rowid” added"]);
    let s = script(&src, &tgt, &c, &all(&c)).unwrap();
    assert!(s.statements.iter().any(|x| x.ends_with("without rowid, strict")), "{:#?}", s.statements);
    // A target with triggers can't be rebuilt without losing them: a manual migration, never
    // selected nor scripted.
    let mut tgt = tgt;
    tgt.tables[0].triggers = vec!["t_audit".into()];
    let c = compare(&src, &tgt);
    assert_eq!(c.objects[0].manual.as_deref(), Some("rebuilding the table would drop its triggers (t_audit)"));
    assert_eq!(c.default_selection(), [false]);
    let s = script(&src, &tgt, &c, &all(&c)).unwrap();
    assert!(s.statements.is_empty(), "{:#?}", s.statements);
    assert!(s.warnings.iter().any(|w| w == "Table “t” needs a manual migration: rebuilding the table would drop its triggers (t_audit)"), "{:?}", s.warnings);
    // Changes ALTER TABLE makes in place keep the triggers: not manual.
    let mut wider = sqlite("create table t (id INTEGER primary key, qty INT, note TEXT)", plain());
    wider.details.columns.push(col("note", "TEXT"));
    let c = compare(&model(Driver::Sqlite, "main", vec![wider], vec![]), &tgt);
    assert_eq!(c.objects[0].manual, None);
}

#[test]
fn sequences_are_created_when_both_schemas_have_the_same_name() {
    let d = Driver::Postgres;
    let seq_default = |seq: &str| format!("nextval('public.{seq}'::regclass)");
    let mut src = model(d, "public", vec![table("t", vec![required(col("id", "integer")), with_default(required(col("y", "integer")), &seq_default("t_y_seq"))], &["id"]), table("n", vec![with_default(required(col("id", "integer")), &seq_default("n_keys"))], &["id"])], vec![]);
    src.sequences = vec!["t_y_seq".into(), "n_keys".into()];
    // A shared sequence of another schema is left alone.
    src.tables[1].details.columns.push(with_default(col("z", "integer"), "nextval('billing.numbers'::regclass)"));
    let tgt = model(d, "public", vec![table("t", vec![required(col("id", "integer"))], &["id"])], vec![]);
    let c = compare(&src, &tgt);
    let s = script(&src, &tgt, &c, &all(&c)).unwrap();
    assert!(at(&s, "create sequence if not exists public.t_y_seq") < at(&s, "add column y integer"), "{:#?}", s.statements);
    assert!(at(&s, "create sequence if not exists public.n_keys") < at(&s, "create table public.n"));
    assert!(!s.statements.iter().any(|x| x.contains("sequence if not exists billing")));
}

#[test]
fn unique_constraints_named_by_the_server_match_by_their_columns() {
    let unique = |name: &str| IndexDef { constraint: true, ..index(name, &["code"], true) };
    let t = |name: &str| {
        let mut t = table("orders", vec![required(col("id", "int")), col("code", "varchar(20)")], &["id"]);
        if !name.is_empty() {
            t.details.indexes.push(unique(name));
        }
        t
    };
    for (driver, a, b) in [(Driver::MsSql, "UQ__orders__A9D105347C5E6F11", "UQ__orders__B2C3D4E5F6A7B8C9"), (Driver::Oracle, "SYS_C0012345", "SYS_C0099999")] {
        let (src, tgt) = (model(driver, "a", vec![t(a)], vec![]), model(driver, "b", vec![t(b)], vec![]));
        assert!(!compare(&src, &tgt).has_differences(), "{driver:?}");
        // Only in the source: added unnamed again (the server names it); only in the target:
        // dropped by its own name.
        let bare = model(driver, "b", vec![t("")], vec![]);
        let c = compare(&src, &bare);
        let s = script(&src, &bare, &c, &all(&c)).unwrap();
        let unquoted = |s: &Script| body(s).iter().map(|x| x.replace('"', "")).collect::<Vec<_>>();
        assert_eq!(unquoted(&s), ["alter table b.orders add unique (code)"], "{driver:?}");
        let c = compare(&bare, &tgt);
        let s = script(&bare, &tgt, &c, &all(&c)).unwrap();
        assert_eq!(unquoted(&s), [format!("alter table b.orders drop constraint {b}")], "{driver:?}");
    }
    // A name chosen by someone is still compared by name.
    let named = |name: &str| model(Driver::MsSql, "dbo", vec![t(name)], vec![]);
    assert!(compare(&named("uq_orders_code"), &named("uq_code")).has_differences());
}

#[test]
fn primary_key_column_order_is_compared_and_scripted() {
    let t = |pk: &[&str]| {
        let mut t = table("t", vec![required(col("a", "integer")), required(col("b", "integer"))], pk);
        t.details.pk_name = Some("t_pkey".into());
        t
    };
    let c = compare(&model(Driver::Postgres, "s", vec![t(&["b", "a"])], vec![]), &model(Driver::Postgres, "s", vec![t(&["a", "b"])], vec![]));
    assert_eq!(c.objects[0].changes, ["Primary key changed"]);
    for (driver, expected) in [
        (Driver::Postgres, vec!["alter table s.t drop constraint t_pkey", "alter table s.t add primary key (b, a)"]),
        (Driver::MySql, vec!["alter table s.t drop primary key, add primary key (b, a)"]),
        (Driver::Oracle, vec!["alter table s.t drop primary key", "alter table s.t add primary key (b, a)"]),
    ] {
        let (src, tgt) = (model(driver, "s", vec![t(&["b", "a"])], vec![]), model(driver, "s", vec![t(&["a", "b"])], vec![]));
        let c = compare(&src, &tgt);
        let s = script(&src, &tgt, &c, &all(&c)).unwrap();
        // Oracle quotes the lowercase names of these models.
        let body: Vec<String> = body(&s).iter().map(|x| x.replace('"', "")).collect();
        assert_eq!(body, expected, "{driver:?}");
    }
}

#[test]
fn mysql_table_names_fold_only_when_the_server_ignores_their_case() {
    let both = || vec![table("Users", vec![col("id", "int")], &[]), table("users", vec![col("id", "int")], &[])];
    let sensitive = |schema: &str| SchemaModel { names_ignore_case: false, ..model(Driver::MySql, schema, both(), vec![]) };
    let c = compare(&sensitive("a"), &sensitive("b"));
    assert_eq!(c.objects.len(), 2, "Users and users are two tables");
    assert!(!c.has_differences());
    // Folding servers see one table under either spelling.
    let folding = |schema: &str, name: &str| model(Driver::MySql, schema, vec![table(name, vec![col("id", "int")], &[])], vec![]);
    assert!(!compare(&folding("a", "Users"), &folding("b", "users")).has_differences());
}

#[test]
fn scripts_run_in_one_transaction_where_ddl_is_transactional() {
    let t = |ty: &str| table("t", vec![required(col("id", "int")), col("n", ty)], &["id"]);
    let pair = |d: Driver| (model(d, "a", vec![t("bigint")], vec![]), model(d, "b", vec![t("int")], vec![]));
    let (src, tgt) = pair(Driver::Postgres);
    let c = compare(&src, &tgt);
    let s = script(&src, &tgt, &c, &all(&c)).unwrap();
    assert_eq!((s.statements.first().unwrap().as_str(), s.statements.last().unwrap().as_str()), ("begin", "commit"));
    // SQL Server: XACT_ABORT, and after each statement a guard that stops the following batches
    // once an error has rolled the transaction back.
    let (src, tgt) = pair(Driver::MsSql);
    let c = compare(&src, &tgt);
    let s = script(&src, &tgt, &c, &all(&c)).unwrap();
    assert_eq!(s.statements, ["set xact_abort on", "begin transaction", "alter table b.t alter column n bigint null", MSSQL_GUARD, "commit transaction", "set noexec off"]);
    // MySQL commits each DDL statement: no transaction, and the script says so.
    let (src, tgt) = pair(Driver::MySql);
    let c = compare(&src, &tgt);
    let s = script(&src, &tgt, &c, &all(&c)).unwrap();
    assert!(!s.statements.iter().any(|x| x.contains("begin") || x.contains("commit")));
    assert!(s.warnings.iter().any(|w| w.contains("commits each DDL statement")));
}

#[test]
fn a_failing_sqlite_rebuild_leaves_nothing_after_rollback() {
    // The new NOT NULL column has no default: copying the rows fails half way through.
    let src = model(Driver::Sqlite, "main", vec![table("a", vec![required(col("id", "INTEGER")), col("n", "TEXT"), required(col("must", "TEXT"))], &["id"])], vec![]);
    let mut tgt = model(Driver::Sqlite, "main", vec![table("a", vec![required(col("id", "INTEGER")), col("n", "TEXT")], &["id"])], vec![]);
    tgt.pragmas = vec![("foreign_keys".into(), "on".into()), ("legacy_alter_table".into(), "off".into())];
    let c = compare(&src, &tgt);
    let s = script(&src, &tgt, &c, &all(&c)).unwrap();
    let db = rusqlite::Connection::open_in_memory().unwrap();
    db.execute_batch("pragma foreign_keys = on; create table a (id INTEGER not null, n TEXT, primary key (id)); insert into a values (1, 'x');").unwrap();
    assert!(db.execute_batch(&s.text(&src, &tgt)).is_err());
    db.execute_batch("rollback").unwrap();
    let tables: Vec<String> = db.prepare("select name from sqlite_master where type = 'table' order by name").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(tables, ["a"], "the temporary table is gone");
    assert_eq!(db.query_row("select n from a", [], |r| r.get::<_, String>(0)).unwrap(), "x");
    // The settings at the end of the script restore the session.
    let n = s.statements.len();
    db.execute_batch(&s.statements[n - 2..].join(";")).unwrap();
    assert_eq!(db.query_row("pragma foreign_keys", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
}

#[test]
fn requalifying_a_big_view_with_many_literals_stays_linear() {
    let n = names(Driver::Postgres, &["t"]);
    let body: String = (0..20_000).map(|i| format!("select 'v{i}'::text, s.t.x from s.t union all ")).collect();
    let started = std::time::Instant::now();
    let moved = requalify(Driver::Postgres, &body, "s", "d", &n);
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
    assert_eq!(moved.matches("from d.t").count(), 20_000);
}
