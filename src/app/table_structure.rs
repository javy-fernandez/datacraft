//! Explorer-to-grid column navigation reuses table tabs and never reloads loaded data.
use super::*;

impl DataCraftApp {
    pub(super) fn navigate_table_column(
        &mut self,
        ctx: &egui::Context,
        table: TableRef,
        columns: Vec<String>,
        open: bool,
    ) {
        if columns.is_empty() {
            return;
        }
        let default_db = self
            .conn(table.conn)
            .and_then(|c| c.default_db())
            .map(str::to_string);
        let matches = |c: &Console| {
            c.conn == Some(table.conn)
                && c.db.as_ref().or(default_db.as_ref()) == Some(&table.db)
                && c.table
                    .as_ref()
                    .is_some_and(|t| t.schema == table.schema && t.table == table.table)
        };
        let index = self
            .consoles
            .get(self.active)
            .filter(|c| matches(c))
            .map(|_| self.active)
            .or_else(|| self.consoles.iter().position(matches));
        let index = match index {
            Some(i) => i,
            None if open => {
                self.open_table(&table);
                self.active
            }
            None => return,
        };
        self.active = index;
        self.object_browser.active = false;
        if self.consoles[index]
            .table
            .as_ref()
            .is_some_and(|t| !t.loaded)
            && self.consoles[index].results.is_empty()
            && !self.consoles[index].is_busy()
        {
            self.reload_table(index);
        }
        let console = &mut self.consoles[index];
        if console.running || console.pending.is_some() || console.results.is_empty() {
            if let Some(t) = console.table.as_mut() {
                t.pending_column = Some(columns);
            }
        } else {
            console.focus_table_column(&columns);
        }
        ctx.memory_mut(|m| {
            if let Some(id) = m.focused() {
                m.surrender_focus(id);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ResultSet;

    fn rows() -> ResultSet {
        ResultSet {
            columns: vec!["id".into(), "customer_id".into()],
            rows: vec![vec![Some("1".into()), Some("42".into())]],
            ..Default::default()
        }
    }

    fn editor(table: &TableRef) -> Console {
        let mut console = Console::new_table(
            800,
            table.conn,
            Some(table.db.clone()),
            table.schema.clone(),
            table.table.clone(),
        );
        console.table.as_mut().unwrap().loaded = true;
        console.results.push(ResultView::new(rows(), 880));
        console
    }

    #[test]
    fn explorer_navigation_reuses_the_exact_table_without_reloading_or_discarding_edits() {
        let (mut app, table) = crate::app::tests::table_menu_app(Driver::Sqlite, true);
        let mut console = editor(&table);
        console.table.as_mut().unwrap().where_clause = "id > 0".into();
        console.table.as_mut().unwrap().offset = 100;
        let view = &mut console.results[0];
        view.set_edit_target(Ok(editing::EditTarget {
            schema: table.schema.clone(),
            table: table.table.clone(),
            col_names: rows().columns,
            key_cols: vec![0],
            binary_cols: vec![false; 2],
            has_pk: true,
            insert_only: false,
        }));
        view.edit.as_mut().unwrap().state.set_value(
            &view.rs,
            editing::RowRef::Existing(0),
            0,
            Some("2".into()),
        );
        let index = app.consoles.len();
        app.consoles.push(console);
        // Another database can contain the same schema and table names.
        let other = TableRef {
            db: "other".into(),
            ..table.clone()
        };
        app.consoles.push(editor(&other));
        app.active = app.consoles.len() - 1;
        app.object_browser.active = true;
        let count = app.consoles.len();
        app.handle_explorer(
            &egui::Context::default(),
            vec![ExplorerAction::NavigateColumn {
                table: table.clone(),
                columns: vec!["expression".into(), "customer_id".into()],
                open: false,
            }],
        );
        assert_eq!(app.active, index);
        assert_eq!(app.consoles.len(), count);
        let console = &app.consoles[index];
        assert!(!console.running);
        assert!(console.has_pending_edits());
        assert_eq!(
            console.results[0].selected_cell(),
            Some((0, 1, Some("42".into())))
        );
        assert_eq!(console.result_tab, ResultTab::Result(0));
        assert_eq!(console.table.as_ref().unwrap().offset, 100);
        assert_eq!(console.table.as_ref().unwrap().where_clause, "id > 0");
        assert!(!app.object_browser.active);
    }

    #[test]
    fn explorer_navigation_waits_for_loading_results_and_does_not_open_tables_on_single_click() {
        let (mut app, table) = crate::app::tests::table_menu_app(Driver::Sqlite, true);
        let ctx = egui::Context::default();
        let count = app.consoles.len();
        app.handle_explorer(
            &ctx,
            vec![ExplorerAction::NavigateColumn {
                table: table.clone(),
                columns: vec!["customer_id".into()],
                open: false,
            }],
        );
        assert_eq!(app.consoles.len(), count);
        app.handle_explorer(
            &ctx,
            vec![ExplorerAction::NavigateColumn {
                table: table.clone(),
                columns: vec!["customer_id".into()],
                open: true,
            }],
        );
        assert_eq!(app.consoles.len(), count + 1);
        let console = &mut app.consoles[app.active];
        assert_eq!(
            console.table.as_ref().unwrap().pending_column.as_deref(),
            Some(&["customer_id".to_string()][..])
        );
        apply_execution(console, vec![rows()], None, std::time::Duration::ZERO, 900);
        assert_eq!(
            console.results[0].selected_cell(),
            Some((0, 1, Some("42".into())))
        );
        assert!(console.table.as_ref().unwrap().pending_column.is_none());
        // A later click during a reload applies to the replacement result.
        console.running = true;
        app.handle_explorer(
            &ctx,
            vec![ExplorerAction::NavigateColumn {
                table,
                columns: vec!["id".into()],
                open: false,
            }],
        );
        let console = &mut app.consoles[app.active];
        apply_execution(console, vec![rows()], None, std::time::Duration::ZERO, 901);
        assert_eq!(
            console.results[0].selected_cell(),
            Some((0, 0, Some("1".into())))
        );
    }
}
