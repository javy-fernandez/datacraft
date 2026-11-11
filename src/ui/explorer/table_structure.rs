//! Lazy table folders with separate caches for keys, indexes and foreign keys.
use super::*;
use crate::db::table_structure::{Group, Reference, TableObject};
use std::sync::Arc;

pub(super) struct MetadataNode {
    request_id: u64,
    result: Option<Result<Arc<Vec<TableObject>>, String>>,
}

impl ExplorerState {
    pub fn request_table_objects(
        &mut self,
        table: &TableRef,
        group: Group,
        refresh: bool,
    ) -> Option<u64> {
        let key = (table.clone(), group);
        if !refresh && self.table_objects.contains_key(&key) {
            return None;
        }
        self.next_metadata_request += 1;
        let request_id = self.next_metadata_request;
        self.table_objects.insert(
            key,
            MetadataNode {
                request_id,
                result: None,
            },
        );
        Some(request_id)
    }

    pub fn receive_table_objects(
        &mut self,
        table: &TableRef,
        group: Group,
        request_id: u64,
        result: Result<Vec<TableObject>, String>,
    ) {
        if let Some(node) = self
            .table_objects
            .get_mut(&(table.clone(), group))
            .filter(|n| n.request_id == request_id)
        {
            node.result = Some(result.map(Arc::new));
        }
    }

    pub fn forget_table_objects(&mut self, conn: ConnId, db: Option<&str>) {
        self.table_objects
            .retain(|(t, _), _| t.conn != conn || db.is_some_and(|db| t.db != db));
    }
}

fn message(ui: &mut Ui, depth: usize, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(12.0 + depth as f32 * INDENT);
        ui.label(RichText::new(text).small().color(pal(ui).muted));
    });
}

#[allow(clippy::too_many_arguments)]
fn folder(
    ui: &mut Ui,
    state: &mut ExplorerState,
    table: &TableRef,
    key: &str,
    label: &str,
    detail: &str,
    depth: usize,
    open: bool,
) -> (egui::Response, bool) {
    let row = tree_row(
        ui,
        state,
        RowSpec {
            key,
            depth,
            expandable: true,
            expanded: open,
            icon: Ico::Folder,
            label,
            badge: None,
            detail: Some((detail, pal(ui).muted)),
            status: None,
            label_color: None,
        },
    );
    if row.resp.clicked() || row.resp.secondary_clicked() {
        state.selected_table = Some(table.clone());
    }
    (row.resp, row.toggled)
}

pub(super) fn columns(
    ui: &mut Ui,
    state: &mut ExplorerState,
    actions: &mut Vec<ExplorerAction>,
    table: &TableRef,
    info: &TableInfo,
    depth: usize,
) {
    // Columns start expanded, as before the folders existed; only a collapse is remembered.
    let key = columns_key(table);
    let open = !state.collapsed.contains(&key);
    let (_, toggled) = folder(
        ui,
        state,
        table,
        &key,
        tr("Columns"),
        &info.columns.len().to_string(),
        depth,
        open,
    );
    if toggled && open {
        state.collapsed.insert(key.clone());
    } else if toggled {
        state.collapsed.remove(&key);
    }
    if !state.collapsed.contains(&key) {
        column_rows(ui, state, actions, table, &key, &info.columns, depth + 1);
    }
}

fn referenced_table(table: &TableRef, reference: &Reference) -> TableRef {
    TableRef {
        conn: table.conn,
        db: reference
            .database
            .clone()
            .unwrap_or_else(|| table.db.clone()),
        schema: reference.schema.clone(),
        table: reference.table.clone(),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn show(
    ui: &mut Ui,
    state: &mut ExplorerState,
    actions: &mut Vec<ExplorerAction>,
    table: &TableRef,
    group: Group,
    depth: usize,
    connected: bool,
) {
    let p = pal(ui);
    let key = table_group_key(table, group);
    let cache_key = (table.clone(), group);
    let detail = match state
        .table_objects
        .get(&cache_key)
        .and_then(|n| n.result.as_ref())
    {
        Some(Ok(objects)) => objects.len().to_string(),
        Some(Err(_)) => tr("error").to_string(),
        None if state.table_objects.contains_key(&cache_key) => tr("loading…").to_string(),
        None => String::new(),
    };
    let open = state.expanded.contains(&key);
    let (resp, toggled) = folder(ui, state, table, &key, group.label(), &detail, depth, open);
    if toggled {
        toggle(state, &key, open);
    }
    resp.context_menu(|ui| {
        if ui
            .add_enabled(connected, egui::Button::new(tr("Refresh")))
            .clicked()
        {
            actions.push(ExplorerAction::LoadTableObjects(table.clone(), group, true));
            ui.close();
        }
    });
    if !state.expanded.contains(&key) {
        return;
    }
    if !state.table_objects.contains_key(&cache_key) && connected {
        actions.push(ExplorerAction::LoadTableObjects(
            table.clone(),
            group,
            false,
        ));
    }
    let result = state
        .table_objects
        .get(&cache_key)
        .and_then(|n| n.result.clone());
    match result {
        Some(Ok(objects)) => {
            if objects.is_empty() {
                message(ui, depth + 1, group.empty_label());
            }
            for object in objects.iter() {
                let key = format!("{key}:object:{}", object.name);
                let open = state.expanded.contains(&key);
                let mut detail = object.columns.join(", ");
                if object.primary_key {
                    detail.push_str(&format!(" · {}", tr("Primary key")));
                } else if object.unique {
                    detail.push_str(&format!(" · {}", tr("Unique")));
                }
                if let Some(reference) = &object.reference {
                    let database = reference
                        .database
                        .as_ref()
                        .map(|db| format!("{db}."))
                        .unwrap_or_default();
                    detail.push_str(&format!(
                        " → {database}{}.{} ({})",
                        reference.schema,
                        reference.table,
                        reference.columns.join(", ")
                    ));
                }
                let row = tree_row(
                    ui,
                    state,
                    RowSpec {
                        key: &key,
                        depth: depth + 1,
                        expandable: !object.columns.is_empty(),
                        expanded: open,
                        icon: match group {
                            Group::Indexes => Ico::Index,
                            Group::Keys => Ico::Key,
                            Group::ForeignKeys => Ico::FollowKey,
                        },
                        label: &object.name,
                        badge: None,
                        detail: Some((&detail, p.muted)),
                        status: None,
                        label_color: None,
                    },
                );
                if row.resp.clicked() || row.resp.secondary_clicked() {
                    state.selected_table = Some(table.clone());
                }
                if row.toggled {
                    toggle(state, &key, open);
                }
                if row.resp.clicked() && (!row.toggled || row.resp.double_clicked()) {
                    let columns: Vec<_> = object.column_names.iter().flatten().cloned().collect();
                    if !columns.is_empty() {
                        actions.push(ExplorerAction::NavigateColumn {
                            table: table.clone(),
                            columns,
                            open: row.resp.double_clicked(),
                        });
                    }
                }
                row.resp
                    .on_hover_text(format!(
                        "{}\n{}\n{}",
                        object.kind, detail, object.definition
                    ))
                    .context_menu(|ui| {
                        if ui.button(tr("Copy Name")).clicked() {
                            ui.ctx().copy_text(object.name.clone());
                            ui.close();
                        }
                        if let Some(reference) = &object.reference
                            && ui
                                .add_enabled(
                                    connected,
                                    egui::Button::new(tr("Open Referenced Table")),
                                )
                                .clicked()
                            {
                                actions.push(ExplorerAction::NavigateColumn {
                                    table: referenced_table(table, reference),
                                    columns: reference.columns.clone(),
                                    open: true,
                                });
                                ui.close();
                            }
                    });
                if state.expanded.contains(&key) {
                    for (i, column) in object.columns.iter().enumerate() {
                        let target = object
                            .reference
                            .as_ref()
                            .and_then(|r| r.columns.get(i))
                            .map(|c| format!("→ {c}"));
                        let row = tree_row(
                            ui,
                            state,
                            RowSpec {
                                key: &format!("{key}:column:{i}"),
                                depth: depth + 2,
                                expandable: false,
                                expanded: false,
                                icon: Ico::Column,
                                label: column,
                                badge: None,
                                detail: target.as_deref().map(|s| (s, p.muted)),
                                status: None,
                                label_color: None,
                            },
                        );
                        if row.resp.clicked() {
                            state.selected_table = Some(table.clone());
                            if let Some(Some(column)) = object.column_names.get(i) {
                                actions.push(ExplorerAction::NavigateColumn {
                                    table: table.clone(),
                                    columns: vec![column.clone()],
                                    open: row.resp.double_clicked(),
                                });
                            }
                        }
                    }
                }
            }
        }
        Some(Err(error)) => error_line(ui, depth + 1, &error),
        None => message(
            ui,
            depth + 1,
            if connected {
                tr("loading…")
            } else {
                tr("The connection is not open")
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;

    fn table(conn: ConnId, db: &str) -> TableRef {
        TableRef {
            conn,
            db: db.into(),
            schema: "main".into(),
            table: "orders".into(),
        }
    }

    #[test]
    fn index_cache_ignores_replies_from_before_refresh_or_disconnect() {
        let mut state = ExplorerState::default();
        let t = table(1, "shop");
        let old = state
            .request_table_objects(&t, Group::Indexes, false)
            .unwrap();
        assert!(
            state
                .request_table_objects(&t, Group::Indexes, false)
                .is_none()
        );
        let current = state
            .request_table_objects(&t, Group::Indexes, true)
            .unwrap();
        state.receive_table_objects(&t, Group::Indexes, old, Err("stale".into()));
        assert!(
            state.table_objects[&(t.clone(), Group::Indexes)]
                .result
                .is_none()
        );
        state.receive_table_objects(&t, Group::Indexes, current, Ok(Vec::new()));
        assert!(matches!(
            state.table_objects[&(t.clone(), Group::Indexes)].result,
            Some(Ok(_))
        ));
        let other_db = table(1, "other");
        let other_conn = table(2, "shop");
        state.request_table_objects(&other_db, Group::Indexes, false);
        state.request_table_objects(&other_conn, Group::Indexes, false);
        state.forget_table_objects(1, Some("shop"));
        state.receive_table_objects(&t, Group::Indexes, current, Err("late reply".into()));
        assert!(
            !state
                .table_objects
                .contains_key(&(t.clone(), Group::Indexes))
        );
        assert!(
            state
                .table_objects
                .contains_key(&(other_db.clone(), Group::Indexes))
        );
        state.forget_table_objects(1, None);
        assert_eq!(state.table_objects.len(), 1);
        assert!(
            state
                .table_objects
                .contains_key(&(other_conn.clone(), Group::Indexes))
        );
    }

    #[test]
    fn table_indexes_load_only_when_expanded_and_can_refresh_after_an_error() {
        let t = table(1, "shop");
        let mut state = ExplorerState::default();
        state.expanded.insert(table_key(&t));
        let table_info = TableInfo {
            name: "orders".into(),
            kind: ObjectKind::Table,
            columns: Vec::new(),
        };
        let mut first = true;
        let mut h = egui_kittest::Harness::builder()
            .with_size(vec2(600.0, 500.0))
            .build_ui_state(
                move |ui, state: &mut (ExplorerState, usize)| {
                    if std::mem::take(&mut first) {
                        theme::install_fonts(ui.ctx());
                        theme::apply(ui.ctx(), true);
                        return;
                    }
                    let mut actions = Vec::new();
                    table_rows(
                        ui,
                        &mut state.0,
                        &mut actions,
                        t.clone(),
                        &table_info,
                        0,
                        crate::ui::table_menu::TableContext {
                            driver: Driver::Sqlite,
                            connected: true,
                            read_only: true,
                        },
                    );
                    for action in actions {
                        if let ExplorerAction::LoadTableObjects(t, group, refresh) = action
                            && state.0.request_table_objects(&t, group, refresh).is_some() {
                                state.1 += 1;
                            }
                    }
                },
                (state, 0),
            );
        h.run_steps(3);
        assert!(h.query_by_label("Indexes").is_some());
        assert_eq!(h.state().1, 0);
        let t = table(1, "shop");
        h.state_mut()
            .0
            .expanded
            .insert(table_group_key(&t, Group::Indexes));
        h.run_steps(3);
        assert_eq!(h.state().1, 1);
        let id = h.state().0.table_objects[&(t.clone(), Group::Indexes)].request_id;
        h.state_mut().0.receive_table_objects(
            &t,
            Group::Indexes,
            id,
            Ok(vec![TableObject {
                name: "orders_email_idx".into(),
                columns: vec!["email".into()],
                unique: true,
                ..Default::default()
            }]),
        );
        h.run_steps(2);
        assert!(h.query_by_label("orders_email_idx").is_some());
        h.state_mut().0.receive_table_objects(
            &t,
            Group::Indexes,
            id,
            Err("catalog unavailable".into()),
        );
        h.run_steps(2);
        h.get_by_label("Indexes").click_secondary();
        h.run_steps(2);
        h.get_by_label("Refresh").click();
        h.run_steps(2);
        assert_eq!(h.state().1, 2);
        let id = h.state().0.table_objects[&(t.clone(), Group::Indexes)].request_id;
        h.state_mut()
            .0
            .receive_table_objects(&t, Group::Indexes, id, Ok(Vec::new()));
        h.run_steps(2);
        assert!(h.query_by_label("No indexes").is_some());
    }

    #[test]
    fn table_structure_lists_keys_indexes_and_foreign_key_targets_in_separate_folders() {
        let t = table(7, "shop");
        let mut state = ExplorerState::default();
        state.expanded.insert(table_key(&t));
        let info = TableInfo {
            name: "orders".into(),
            kind: ObjectKind::Table,
            columns: vec![crate::db::ColumnInfo {
                name: "customer_id".into(),
                data_type: "INTEGER".into(),
                nullable: false,
                primary_key: false,
            }],
        };
        for group in Group::ALL {
            let id = state.request_table_objects(&t, group, false).unwrap();
            let object = match group {
                Group::Keys => TableObject {
                    name: "orders_pkey".into(),
                    primary_key: true,
                    unique: true,
                    columns: vec!["id".into()],
                    column_names: vec![Some("id".into())],
                    ..Default::default()
                },
                Group::Indexes => TableObject {
                    name: "orders_customer_idx".into(),
                    columns: vec!["customer_id".into()],
                    column_names: vec![Some("customer_id".into())],
                    ..Default::default()
                },
                Group::ForeignKeys => TableObject {
                    name: "orders_customer_fkey".into(),
                    columns: vec!["customer_id".into()],
                    column_names: vec![Some("customer_id".into())],
                    reference: Some(Reference {
                        database: Some("customers_db".into()),
                        schema: "public".into(),
                        table: "customers".into(),
                        columns: vec!["id".into()],
                    }),
                    ..Default::default()
                },
            };
            state.receive_table_objects(&t, group, id, Ok(vec![object]));
        }
        let mut first = true;
        let mut h = egui_kittest::Harness::builder()
            .with_size(vec2(800.0, 700.0))
            .build_ui_state(
                move |ui, state: &mut (ExplorerState, Vec<ExplorerAction>)| {
                    if std::mem::take(&mut first) {
                        theme::install_fonts(ui.ctx());
                        theme::apply(ui.ctx(), true);
                        return;
                    }
                    table_rows(
                        ui,
                        &mut state.0,
                        &mut state.1,
                        t.clone(),
                        &info,
                        0,
                        crate::ui::table_menu::TableContext {
                            driver: Driver::Postgres,
                            connected: true,
                            read_only: true,
                        },
                    );
                },
                (state, Vec::new()),
            );
        h.run_steps(3);
        for label in ["Columns", "Keys", "Indexes", "Foreign Keys"] {
            assert!(h.query_by_label(label).is_some());
        }
        assert!(
            h.query_by_label("customer_id").is_some(),
            "columns start expanded inside their folder"
        );
        assert!(h.query_by_label("orders_pkey").is_none());
        let t = table(7, "shop");
        for group in Group::ALL {
            h.state_mut().0.expanded.insert(table_group_key(&t, group));
        }
        h.run_steps(3);
        for label in [
            "customer_id",
            "orders_pkey",
            "orders_customer_idx",
            "orders_customer_fkey",
        ] {
            assert!(h.query_by_label(label).is_some());
        }
        for (label, column) in [
            ("customer_id", "customer_id"),
            ("orders_pkey", "id"),
            ("orders_customer_idx", "customer_id"),
            ("orders_customer_fkey", "customer_id"),
        ] {
            h.state_mut().1.clear();
            h.get_by_label(label).click();
            h.run_steps(2);
            assert!(h.state().1.iter().any(|a| matches!(a, ExplorerAction::NavigateColumn { table, columns, open: false } if table == &t && columns == &[column.to_string()])));
        }
        h.get_by_label("orders_customer_fkey").click_secondary();
        h.run_steps(2);
        h.get_by_label("Open Referenced Table").click();
        h.run_steps(2);
        assert!(h.state().1.iter().any(|a| matches!(a, ExplorerAction::NavigateColumn { table: t, columns, open: true } if t.conn == 7 && t.db == "customers_db" && t.schema == "public" && t.table == "customers" && columns == &["id".to_string()])));
        h.state_mut().0.expanded.insert(format!(
            "{}:object:orders_customer_fkey",
            table_group_key(&t, Group::ForeignKeys)
        ));
        h.state_mut().0.collapsed.insert(columns_key(&t));
        h.run_steps(2);
        assert!(
            h.query_by_label("customer_id").is_some(),
            "expanding a foreign key lists its local columns"
        );
        h.state_mut().1.clear();
        h.get_by_label("customer_id").click();
        h.run_steps(2);
        assert!(h.state().1.iter().any(|a| matches!(a, ExplorerAction::NavigateColumn { table, columns, open: false } if table == &t && columns == &["customer_id".to_string()])));
    }
}
