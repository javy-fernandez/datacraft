//! Full-window documentation captures, rendered from the real app UI with synthetic data.
use super::*;
use crate::db::object_info::ObjectInfo;
use crate::ui::filter_sort::{Condition, Connector, FilterRow, Group, Node, Operator, SortRule};
use crate::ui::object_browser::{ObjectLayout, ObjectScope};
use egui_kittest::{Harness, kittest::Queryable};

fn fixture(dark: bool) -> DataCraftApp {
    let mut app = super::tests::tabs_app();
    app.dark = dark;
    app.demo_explorer_w = Some(255.0);
    app.consoles.clear();
    app.active = 1;
    let columns: Vec<_> = [
        ("id", "int"),
        ("customer", "varchar(120)"),
        ("status", "varchar(20)"),
        ("total", "decimal(10,2)"),
        ("created_at", "datetime"),
        ("note", "text"),
    ]
    .into_iter()
    .map(|(name, data_type)| ColumnInfo {
        name: name.into(),
        data_type: data_type.into(),
        nullable: name == "note",
        primary_key: name == "id",
    })
    .collect();
    let names = [
        "addresses",
        "audit_events",
        "brands",
        "carts",
        "cart_items",
        "categories",
        "channels",
        "coupons",
        "currencies",
        "customers",
        "customer_groups",
        "customer_notes",
        "delivery_methods",
        "discounts",
        "exchange_rates",
        "inventory",
        "inventory_events",
        "invoices",
        "invoice_items",
        "labels",
        "locations",
        "notifications",
        "orders",
        "order_items",
        "order_notes",
        "order_statuses",
        "payments",
        "payment_methods",
        "price_lists",
        "products",
        "product_images",
        "product_options",
        "product_tags",
        "promotions",
        "purchase_orders",
        "receipts",
        "refunds",
        "regions",
        "returns",
        "reviews",
        "roles",
        "sessions",
        "shipments",
        "shipping_rates",
        "stock_levels",
        "suppliers",
        "tags",
        "tax_rates",
        "teams",
        "transactions",
        "users",
        "variants",
        "warehouses",
        "webhooks",
        "wishlists",
        "wishlist_items",
    ];
    let tables = names
        .iter()
        .map(|name| crate::db::TableInfo {
            name: (*name).into(),
            kind: crate::db::ObjectKind::Table,
            columns: columns.clone(),
        })
        .collect();
    let mut cfg = ConnectionConfig::new(7, Driver::MariaDb);
    cfg.name = "Development".into();
    cfg.database = "shop".into();
    let mut conn = ConnState::new(cfg);
    conn.status = ConnStatus::Connected;
    conn.databases = vec![
        DatabaseInfo {
            name: "shop".into(),
            current: true,
            schemas: Some(vec![SchemaInfo {
                name: "shop".into(),
                tables,
            }]),
        },
        DatabaseInfo {
            name: "analytics".into(),
            current: false,
            schemas: None,
        },
    ];
    app.conns.push(conn);
    app.explorer
        .expanded
        .extend(["c:7".into(), "d:7:shop".into()]);

    let info = ObjectInfo {
        rows: Some(48),
        engine: Some("InnoDB".into()),
        auto_increment: Some(49),
        row_format: Some("Dynamic".into()),
        created: Some("2026-10-01 09:30:00".into()),
        modified: Some("2026-10-23 12:00:00".into()),
        data_bytes: Some(32768),
        index_bytes: Some(16384),
        collation: Some("utf8mb4_unicode_ci".into()),
        comment: Some("Customer orders".into()),
        ..Default::default()
    };
    let rs = crate::db::ResultSet {
        columns: columns.iter().map(|column| column.name.clone()).collect(),
        rows: (1..=48)
            .map(|id| {
                vec![
                    Some(id.to_string()),
                    Some(format!("Customer {id:02}")),
                    Some(if id % 3 == 0 { "pending" } else { "paid" }.into()),
                    Some(format!("{}.00", id * 25)),
                    Some("2026-10-23 12:00:00".into()),
                    Some(
                        if id == 1 {
                            "Delivery instructions\nLeave at reception."
                        } else {
                            "Standard delivery"
                        }
                        .into(),
                    ),
                ]
            })
            .collect(),
        elapsed: std::time::Duration::from_millis(42),
        ..Default::default()
    };
    let query = "-- Paid orders ready for fulfillment\nSELECT id, customer, status, total, created_at, note\nFROM orders\nWHERE status = 'paid'\n  AND total >= 100\nORDER BY total DESC;";
    let mut console = Console::new(1, "Sales overview".into(), query.into(), Some(7));
    console.db = Some("shop".into());
    let mut query_result = rs.clone();
    query_result.rows.retain(|row| {
        row[2].as_deref() == Some("paid")
            && row[3].as_ref().unwrap().parse::<f64>().unwrap() >= 100.0
    });
    query_result.rows.reverse();
    console.results.push(ResultView::new(query_result, 92));
    console.result_tab = ResultTab::Result(0);
    console.last_statements.push(query.into());
    app.consoles.push(console);

    let mut table = Console::new_table(2, 7, Some("shop".into()), "shop".into(), "orders".into());
    let mut view = ResultView::new(rs, 93);
    view.set_edit_target(Ok(editing::EditTarget {
        schema: "shop".into(),
        table: "orders".into(),
        col_names: view.rs.columns.clone(),
        key_cols: vec![0],
        binary_cols: vec![false; 6],
        has_pk: true,
    }));
    view.begin_edit(0, 5);
    view.finish_inline_edit();
    table.results.push(view);
    table
        .last_statements
        .push("SELECT * FROM `shop`.`orders` ORDER BY `id` LIMIT 1000".into());
    let state = table.table.as_mut().unwrap();
    state.loaded = true;
    state.info_requested = true;
    state.info = Some(Ok(info.clone()));
    state.filter_sort.filter.children = vec![
        Node::Condition(Condition {
            enabled: true,
            column: "status".into(),
            operator: Operator::Equal,
            value: "paid".into(),
            second: String::new(),
        })
        .into(),
        Node::Condition(Condition {
            enabled: true,
            column: "total".into(),
            operator: Operator::GreaterEqual,
            value: "100".into(),
            second: String::new(),
        })
        .into(),
        FilterRow {
            connector: Connector::Or,
            node: Node::Condition(Condition {
                enabled: true,
                column: "note".into(),
                operator: Operator::NotNull,
                value: String::new(),
                second: String::new(),
            }),
        },
        Node::Condition(Condition {
            enabled: true,
            column: "status".into(),
            operator: Operator::Equal,
            value: "pending".into(),
            second: String::new(),
        })
        .into(),
        Node::Condition(Condition {
            enabled: true,
            column: "total".into(),
            operator: Operator::GreaterEqual,
            value: "200".into(),
            second: String::new(),
        })
        .into(),
    ];
    state.filter_sort.sort.push(SortRule {
        enabled: true,
        column: "total".into(),
        descending: true,
    });
    app.consoles.push(table);
    app.next_console_id = 3;
    app.object_browser.open(ObjectScope {
        conn: 7,
        db: "shop".into(),
        schema: None,
        kind: None,
    });
    for (index, name) in names.iter().enumerate() {
        let mut statistics = info.clone();
        statistics.rows = Some((index as u64 + 1) * 48);
        statistics.auto_increment = statistics.rows.map(|count| count + 1);
        statistics.comment = None;
        app.object_browser
            .catalog
            .insert(("shop".into(), (*name).into()), statistics);
    }
    let reference = TableRef {
        conn: 7,
        db: "shop".into(),
        schema: "shop".into(),
        table: "orders".into(),
    };
    let request = app.object_browser.start_request(reference);
    app.object_browser.receive(7, request, Ok(info));
    app.object_browser.organizations.push(
        serde_json::from_value(serde_json::json!({
            "conn": 7, "db": "shop", "groups": ["Billing", "Fulfillment"],
            "group_colors": {"Billing": [111, 158, 255], "Fulfillment": [97, 195, 147]},
            "objects": [
                {"schema": "shop", "table": "orders", "color": [111, 158, 255]},
                {"schema": "shop", "table": "customers", "color": [198, 150, 240]},
                {"schema": "shop", "table": "payments", "color": [97, 195, 147]},
                {"schema": "shop", "table": "invoices", "group": "Billing"},
                {"schema": "shop", "table": "invoice_items", "group": "Billing"},
                {"schema": "shop", "table": "shipments", "group": "Fulfillment"}
            ]
        }))
        .unwrap(),
    );
    app.object_browser.active = false;
    app
}

fn harness(dark: bool) -> Harness<'static, DataCraftApp> {
    let mut first = true;
    let mut frame = eframe::Frame::_new_kittest();
    let mut harness = Harness::builder()
        .with_size(vec2(1600.0, 1000.0))
        .build_ui_state(
            move |ui, app| {
                if std::mem::take(&mut first) {
                    theme::install_fonts(ui.ctx());
                    theme::apply(ui.ctx(), dark);
                    return;
                }
                // The app owns its full root layout; remove the harness's outer padding.
                let bounds = ui.ctx().content_rect();
                let mut root = ui.new_child(egui::UiBuilder::new().max_rect(bounds));
                root.set_clip_rect(bounds);
                eframe::App::ui(app, &mut root, &mut frame);
            },
            fixture(dark),
        );
    harness.run_steps(4);
    harness
}

#[test]
#[ignore = "Requires a GPU; regenerates full-window README screenshots without a database"]
fn render_readme_screenshots() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/readme-snapshots");
    std::fs::create_dir_all(&dir).unwrap();
    let mut light = harness(false);
    light
        .render()
        .unwrap()
        .save(dir.join("workspace-light.png"))
        .unwrap();
    let mut h = harness(true);
    assert!(h.query_by_label("Cell Editor").is_some());
    assert!(h.query_by_label("Close window").is_some());
    h.render()
        .unwrap()
        .save(dir.join("workspace-dark.png"))
        .unwrap();
    h.get_by_label("Filter & Sort").click();
    h.run_steps(3);
    h.render()
        .unwrap()
        .save(dir.join("filter-sort.png"))
        .unwrap();
    let children = &mut h.state_mut().consoles[1]
        .table
        .as_mut()
        .unwrap()
        .filter_sort
        .filter
        .children;
    let rows = std::mem::take(children);
    let mut paid = rows[0].clone();
    paid.connector = Connector::Or;
    *children = vec![
        Node::Group(Group {
            children: vec![paid, rows[3].clone()],
        })
        .into(),
        rows[1].clone(),
    ];
    h.run_steps(3);
    h.render()
        .unwrap()
        .save(dir.join("filter-groups.png"))
        .unwrap();
    h.get_by_label("Filter & Sort").click();
    h.get_by_label("Cell Editor").click();
    h.get_by_label("Record").click();
    h.run_steps(3);
    h.render()
        .unwrap()
        .save(dir.join("cell-editor.png"))
        .unwrap();
    h.state_mut().active = 0;
    h.run_steps(3);
    h.render()
        .unwrap()
        .save(dir.join("sql-console.png"))
        .unwrap();
    for (layout, filename) in [
        (ObjectLayout::List, "objects-list.png"),
        (ObjectLayout::Grid, "objects-grid.png"),
    ] {
        h.state_mut().object_browser.active = true;
        h.state_mut().object_browser.layout = layout;
        h.run_steps(3);
        h.render().unwrap().save(dir.join(filename)).unwrap();
    }
    let mut backup =
        crate::ui::backup_dialog::BackupDialog::new(7, Driver::MariaDb, "shop".into(), false);
    backup.request.destination = "D:/Backups/shop_20261023.sql.gz".into();
    backup.request.utility = "mariadb-dump".into();
    backup.request.compress = true;
    h.state_mut().backup = Some(backup);
    h.run_steps(3);
    h.render()
        .unwrap()
        .save(dir.join("database-backup.png"))
        .unwrap();
}
