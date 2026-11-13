//! Stable-id recent tab switching.
use super::*;
use console::navigation::Command;
fn switcher_key(input: &mut egui::InputState, key: Key) -> bool {
    let mut found = false;
    input.events.retain(|e| if matches!(e,egui::Event::Key{key:k,pressed:true,modifiers:m,..} if *k==key && !m.alt && !m.mac_cmd){found=true;false}else{true});
    found
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Tab(u64),
    Objects,
    Explorer,
    Results,
}
#[derive(Clone, Copy)]
struct Location {
    target: Target,
    position: usize,
}
struct Switcher {
    items: Vec<Target>,
    selected: usize,
    release_ctrl: bool,
    reveal_selection: bool,
}
#[derive(Default)]
pub(super) struct State {
    pub(super) reveal_tab: Option<u64>,
    recent: Vec<Target>,
    switcher: Option<Switcher>,
}
impl State {
    pub fn is_open(&self) -> bool {
        self.switcher.is_some()
    }
}
impl DataCraftApp {
    fn navigation_location(&self) -> Option<Location> {
        if self.object_browser.active {
            Some(Location {
                target: Target::Objects,
                position: 0,
            })
        } else {
            self.consoles.get(self.active).map(|c| Location {
                target: Target::Tab(c.id),
                position: c.cursor.map_or(0, |(_, b)| b),
            })
        }
    }
    pub(super) fn observe_navigation(&mut self) {
        let ids: HashSet<_> = self.consoles.iter().map(|c| c.id).collect();
        self.navigation
            .recent
            .retain(|t| !matches!(t,Target::Tab(id) if !ids.contains(id)));
        if self
            .navigation
            .reveal_tab
            .is_some_and(|id| !ids.contains(&id))
        {
            self.navigation.reveal_tab = None;
        }
        if let Some(loc) = self.navigation_location()
            && self.navigation.recent.first() != Some(&loc.target)
        {
            if let Target::Tab(id) = loc.target {
                self.navigation.reveal_tab = Some(id);
            }
            self.navigation.recent.retain(|t| *t != loc.target);
            self.navigation.recent.insert(0, loc.target);
        }
    }
    fn activate_navigation(
        &mut self,
        ctx: &egui::Context,
        target: Target,
        position: Option<usize>,
    ) {
        self.observe_navigation();
        // A late popup choice may target an editor absent from this frame's accessibility
        // tree. That editor requests focus when it is next drawn.
        ctx.memory_mut(|m| {
            if let Some(id) = m.focused() {
                m.surrender_focus(id);
            }
        });
        ctx.request_repaint();
        match target {
            Target::Tab(id) => {
                if let Some(i) = self.consoles.iter().position(|c| c.id == id) {
                    self.active = i;
                    self.object_browser.active = false;
                    if self.consoles[i].table.is_none() {
                        if let Some(editor) = &mut self.consoles[i].routine {
                            editor.page = crate::ui::routine_dialog::RoutinePage::Definition;
                        }
                        let p = position
                            .unwrap_or_else(|| self.consoles[i].cursor.map_or(0, |(_, b)| b));
                        let c = &mut self.consoles[i];
                        let p = p.min(c.text.chars().count());
                        c.cursor = Some((p, p));
                        c.navigation.caret = Some(p);
                        c.navigation.anchor = Some(p);
                        c.navigation.pending_location = Some(p);
                    } else {
                        ctx.memory_mut(|m| {
                            if let Some(id) = m.focused() {
                                m.surrender_focus(id);
                            }
                        });
                    }
                }
            }
            Target::Objects => {
                self.object_browser.active = true;
                ctx.memory_mut(|m| {
                    if let Some(id) = m.focused() {
                        m.surrender_focus(id);
                    }
                });
            }
            Target::Explorer => {
                self.show_explorer = true;
                self.explorer.focused = true;
                ctx.memory_mut(|m| {
                    if let Some(id) = m.focused() {
                        m.surrender_focus(id);
                    }
                });
            }
            Target::Results => {
                self.show_results = true;
                ctx.memory_mut(|m| {
                    if let Some(id) = m.focused() {
                        m.surrender_focus(id);
                    }
                });
            }
        }
    }
    fn open_switcher(&mut self, reverse: bool, release_ctrl: bool) {
        self.observe_navigation();
        let mut items = self.navigation.recent.clone();
        for t in self.consoles.iter().map(|c| Target::Tab(c.id)).chain([
            Target::Objects,
            Target::Explorer,
            Target::Results,
        ]) {
            if !items.contains(&t) {
                items.push(t);
            }
        }
        let selected = if reverse {
            items.len().saturating_sub(1)
        } else {
            1.min(items.len().saturating_sub(1))
        };
        self.navigation.switcher = Some(Switcher {
            items,
            selected,
            release_ctrl,
            reveal_selection: true,
        });
    }
    fn finish_switcher(&mut self, ctx: &egui::Context, choose: bool) {
        let Some(switcher) = self.navigation.switcher.take() else {
            return;
        };
        if choose {
            if let Some(&target) = switcher.items.get(switcher.selected) {
                self.activate_navigation(ctx, target, None);
            }
        } else if let Some(loc) = self.navigation_location() {
            self.activate_navigation(ctx, loc.target, Some(loc.position));
        }
    }
    pub(super) fn navigation_shortcuts(&mut self, ctx: &egui::Context) -> bool {
        // A native focus change can swallow the modifier's key-up event. Cancel rather
        // than leaving the editor disabled or choosing a tab while the app is inactive.
        if self.navigation.switcher.is_some() && !ctx.input(|i| i.focused) {
            self.finish_switcher(ctx, false);
            return true;
        }
        // Ctrl+Tab is Control on every platform, including macOS. Ignore extra Alt/Command.
        let cycle = ctx.input_mut(|i| {
            let mut reverse = None;
            i.events.retain(|e| {
                if let egui::Event::Key {
                    key: Key::Tab,
                    pressed: true,
                    modifiers: m,
                    ..
                } = e
                    && m.ctrl
                    && !m.alt
                    && !m.mac_cmd
                {
                    reverse = Some(m.shift);
                    return false;
                }
                true
            });
            reverse
        });
        if let Some(s) = &mut self.navigation.switcher {
            let (up, down, enter, esc, held) = ctx.input_mut(|i| {
                (
                    switcher_key(i, Key::ArrowUp) || switcher_key(i, Key::ArrowLeft),
                    switcher_key(i, Key::ArrowDown) || switcher_key(i, Key::ArrowRight),
                    switcher_key(i, Key::Enter),
                    switcher_key(i, Key::Escape),
                    i.modifiers.ctrl,
                )
            });
            if cycle.is_some() || up || down {
                let reverse = cycle.unwrap_or(up);
                s.selected = if reverse {
                    (s.selected + s.items.len() - 1) % s.items.len()
                } else {
                    (s.selected + 1) % s.items.len()
                };
                s.reveal_selection = true;
            }
            if esc {
                self.finish_switcher(ctx, false);
            } else if (s.release_ctrl && !held) || enter {
                self.finish_switcher(ctx, true);
            }
            return true;
        }
        if self.modal_open() {
            return cycle.is_some();
        }
        if let Some(reverse) = cycle {
            self.open_switcher(reverse, true);
            // Both events may arrive before the next frame during a quick Ctrl+Tab.
            if !ctx.input(|i| i.modifiers.ctrl) {
                self.finish_switcher(ctx, true);
            }
            return true;
        }
        false
    }
    pub(super) fn navigation_command(&mut self, ctx: &egui::Context, cmd: Command) {
        match cmd {
            Command::Switcher => self.open_switcher(false, false),
            Command::Structure => {
                if let Some(c) = self
                    .consoles
                    .get_mut(self.active)
                    .filter(|c| c.table.is_none() && !self.object_browser.active)
                {
                    c.navigation.pending.push(cmd);
                }
            }
        }
        ctx.request_repaint();
    }
    pub(super) fn navigation_ui(&mut self, ctx: &egui::Context) {
        if let Some(s) = &mut self.navigation.switcher {
            let rows: Vec<_> = s
                .items
                .iter()
                .map(|t| {
                    (
                        *t,
                        match t {
                            Target::Tab(id) => self
                                .consoles
                                .iter()
                                .find(|c| c.id == *id)
                                .map(|c| c.title.clone())
                                .unwrap_or_default(),
                            Target::Objects => tr("Objects").into(),
                            Target::Explorer => tr("Database Explorer").into(),
                            Target::Results => tr("Results panel").into(),
                        },
                    )
                })
                .collect();
            let selected = s.selected;
            let reveal_selection = std::mem::take(&mut s.reveal_selection);
            let mut pick = None;
            let mut open = true;
            egui::Window::new(tr("Recent tabs / Switcher")).id(egui::Id::new("tab_switcher")).open(&mut open).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER,vec2(0.0,0.0)).show(ctx,|ui|{
                ui.label(tr("Hold Ctrl, choose with Tab or arrows, release Ctrl to switch. Esc cancels."));
                egui::ScrollArea::vertical().max_height(360.0).show(ui,|ui|{
                    for (i,(target,label)) in rows.iter().enumerate(){let r=ui.selectable_label(i==selected,label);if i==selected && reveal_selection{r.scroll_to_me_animation(None,egui::style::ScrollAnimation::none());}if r.clicked(){pick=Some(*target);}}
                });
            });
            if let Some(target) = pick {
                self.navigation.switcher = None;
                self.activate_navigation(ctx, target, None);
            } else if !open {
                self.finish_switcher(ctx, false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;
    fn metadata_app(text: &str) -> DataCraftApp {
        let mut app = crate::app::tests::tabs_app();
        let mut cfg = ConnectionConfig::new(7, Driver::Postgres);
        cfg.database = "shop".into();
        let mut conn = ConnState::new(cfg);
        conn.databases = vec![DatabaseInfo {
            name: "shop".into(),
            current: true,
            schemas: Some(vec![SchemaInfo {
                name: "public".into(),
                tables: vec![TableInfo {
                    name: "users".into(),
                    kind: crate::db::ObjectKind::Table,
                    columns: vec![ColumnInfo {
                        name: "id".into(),
                        data_type: "integer".into(),
                        nullable: false,
                        primary_key: true,
                    }],
                }],
            }]),
        }];
        app.conns.push(conn);
        app.active = 0;
        app.object_browser.active = false;
        app.consoles[0].conn = Some(7);
        app.consoles[0].db = Some("shop".into());
        app.consoles[0].text = text.into();
        let text = app.consoles[0].text.clone();
        app.consoles[0].navigation.sync_text(&text);
        app
    }
    fn dirty_scope_app() -> DataCraftApp {
        let mut app = metadata_app("SELECT 1;\nSELECT 2;\nSELECT 3;");
        app.conns[0].status = ConnStatus::Connected;
        app.consoles[0].cursor = Some((0, 0));
        let mut view = ResultView::new(
            crate::db::ResultSet {
                columns: vec!["id".into()],
                rows: vec![vec![Some("1".into())]],
                ..Default::default()
            },
            100,
        );
        view.set_edit_target(Ok(crate::ui::editing::EditTarget {
            schema: "public".into(),
            table: "users".into(),
            col_names: vec!["id".into()],
            key_cols: vec![0],
            binary_cols: vec![false],
            has_pk: true,
            insert_only: false,
        }));
        view.edit.as_mut().unwrap().state.set_value(
            &view.rs,
            crate::ui::editing::RowRef::Existing(0),
            0,
            Some("2".into()),
        );
        app.consoles[0].results.push(view);
        app
    }
    fn harness(app: DataCraftApp) -> egui_kittest::Harness<'static, DataCraftApp> {
        let dark = app.dark;
        let mut first = true;
        let mut frame = eframe::Frame::_new_kittest();
        let mut h = egui_kittest::Harness::builder()
            .with_size(vec2(1280.0, 860.0))
            .build_ui_state(
                move |ui, app: &mut DataCraftApp| {
                    if std::mem::take(&mut first) {
                        theme::install_fonts(ui.ctx());
                        theme::apply(ui.ctx(), dark);
                        return;
                    }
                    app.handle_responses();
                    let bounds = ui.ctx().content_rect();
                    let mut root = ui.new_child(egui::UiBuilder::new().max_rect(bounds));
                    root.set_clip_rect(bounds);
                    eframe::App::ui(app, &mut root, &mut frame);
                },
                app,
            );
        h.run_steps(3);
        h
    }
    #[test]
    fn discard_confirmation_preserves_gutter_and_structure_execution_scopes() {
        for structure in [false, true] {
            let mut h = harness(dirty_scope_app());
            let expected = if structure {
                h.state_mut().consoles[0].open_structure(Driver::Postgres);
                h.run_steps(2);
                h.get_by_label("2:  SELECT 2").click();
                h.run_steps(2);
                h.get_by_label("3:  SELECT 3")
                    .click_modifiers(Modifiers::COMMAND);
                h.run_steps(2);
                h.get_by_label("Execute selected statements").click();
                vec!["SELECT 2".to_string(), "SELECT 3".into()]
            } else {
                h.get_by_label("Execute statement 2").click();
                vec!["SELECT 2".to_string()]
            };
            h.run_steps(2);
            assert!(h.state().confirm_discard.is_some());
            assert!(h.state().consoles[0].last_statements.is_empty());
            h.get_by_label("Discard").click();
            h.run_steps(2);
            assert_eq!(h.state().consoles[0].last_statements, expected);
            assert!(!h.state().consoles[0].has_pending_edits());
        }
    }
    #[test]
    fn confirmed_scope_execution_rechecks_source_and_remaining_guards() {
        for mode in 0..4 {
            let mut app = dirty_scope_app();
            app.consoles[0].text = "SELECT 1; DELETE FROM users; VACUUM;".into();
            app.conns[0].cfg.confirm_dangerous = true;
            let statements = vec![
                if mode == 2 {
                    "VACUUM"
                } else {
                    "DELETE FROM users"
                }
                .to_string(),
            ];
            app.run_selected_statements(0, statements.clone());
            let action = app.confirm_discard.take().unwrap();
            match mode {
                0 => app.conns[0].cfg.read_only = true,
                1 => app.consoles[0].text.push_str(" -- changed"),
                2 => app.consoles[0].manual_tx = true,
                _ => {}
            }
            app.discard_and_do(action);
            assert!(app.consoles[0].last_statements.is_empty());
            if mode == 1 {
                assert!(
                    app.consoles[0].has_pending_edits(),
                    "a changed source cancels the frozen request"
                );
            } else if mode == 3 {
                assert_eq!(
                    app.safety.confirm.as_ref().unwrap().what,
                    crate::app::safety::Guarded::Run(statements)
                );
            } else {
                assert!(app.consoles[0].log.last().unwrap().is_error);
            }
        }
    }
    #[test]
    fn control_tab_activates_a_large_varied_script_without_executing_it() {
        let mut app = metadata_app("SELECT 1;");
        app.consoles[1].text = (0..6000).map(|i| format!(
            "SELECT column_{i}, 'row {i}', {i} + 1 FROM schema_{i}.table_{i} WHERE id_{i} >= {i};\n"
        )).collect();
        let mut h = harness(app);
        h.input_mut().events.extend([
            egui::Event::ModifiersChanged(Modifiers::CTRL),
            egui::Event::Key {
                key: Key::Tab,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::CTRL,
            },
        ]);
        h.run_steps(2);
        h.input_mut()
            .events
            .push(egui::Event::ModifiersChanged(Modifiers::NONE));
        h.run_steps(1);
        assert_eq!(h.state().active, 1);
    }
    #[test]
    fn control_tab_preserves_cached_table_data_without_loading_or_query_execution() {
        let mut app = metadata_app("SELECT 1;");
        let mut table =
            Console::new_table(2, 7, Some("shop".into()), "public".into(), "users".into());
        let state = table.table.as_mut().unwrap();
        state.loaded = true;
        state.info_requested = true;
        table.results.push(ResultView::new(
            crate::db::ResultSet {
                statement: "SELECT id FROM public.users".into(),
                columns: vec!["id".into()],
                rows: (0..1000).map(|i| vec![Some(i.to_string())]).collect(),
                ..Default::default()
            },
            901,
        ));
        table.result_tab = console::ResultTab::Result(0);
        app.consoles[1] = table;
        let mut h = harness(app);
        for expected in [1, 0, 1, 0] {
            h.input_mut().events.extend([
                egui::Event::ModifiersChanged(Modifiers::CTRL),
                egui::Event::Key {
                    key: Key::Tab,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::CTRL,
                },
            ]);
            h.run_steps(2);
            h.input_mut()
                .events
                .push(egui::Event::ModifiersChanged(Modifiers::NONE));
            h.run_steps(3);
            assert_eq!(h.state().active, expected);
            let table = &h.state().consoles[1];
            assert!(
                !table.is_busy(),
                "switching to a cached table must not start loading it again"
            );
            assert!(
                table.last_statements.is_empty(),
                "Ctrl+Tab must never submit SQL"
            );
            assert_eq!(table.results[0].rs.rows.len(), 1000);
            assert!(h.query_by_label("Loading…").is_none());
        }
    }
    #[test]
    fn switcher_with_many_tabs_stops_repainting_when_idle() {
        let mut app = metadata_app("SELECT 1;");
        for id in 10..90 {
            app.consoles.push(Console::new(
                id,
                format!("Query {id}"),
                "SELECT 1;".into(),
                None,
            ));
        }
        let mut h = harness(app);
        h.input_mut()
            .events
            .push(egui::Event::ModifiersChanged(Modifiers::CTRL));
        h.input_mut().events.push(egui::Event::Key {
            key: Key::Tab,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL,
        });
        h.run_steps(2);
        assert!(h.state().navigation.switcher.is_some());
        h.run_steps(40); // Ten seconds of simulated idle time, beyond any scroll animation.
        let idle = h.try_run();
        assert!(
            idle.is_ok(),
            "an idle switcher must stop requesting immediate repaint: {idle:?}"
        );
        let before: Vec<_> = h.state().consoles.iter().map(|c| c.text.clone()).collect();
        let count = h.state().navigation.switcher.as_ref().unwrap().items.len();
        for _ in 0..count + 2 {
            h.input_mut().events.push(egui::Event::Key {
                key: Key::Tab,
                physical_key: None,
                pressed: true,
                repeat: true,
                modifiers: Modifiers::CTRL,
            });
            h.run_steps(1);
        }
        assert_eq!(h.state().navigation.switcher.as_ref().unwrap().selected, 3);
        assert!(
            h.try_run().is_ok(),
            "cycling past the bottom and wrapping must also settle"
        );
        h.input_mut()
            .events
            .push(egui::Event::ModifiersChanged(Modifiers::NONE));
        h.run_steps(2);
        assert!(h.state().navigation.switcher.is_none());
        assert_eq!(h.state().active, 3);
        assert_eq!(
            h.state()
                .consoles
                .iter()
                .map(|c| c.text.clone())
                .collect::<Vec<_>>(),
            before,
            "switcher keys must not reach the SQL editors"
        );
    }
    #[test]
    fn switcher_dismisses_on_focus_loss_even_without_control_release() {
        let mut h = harness(metadata_app("SELECT 1;"));
        h.input_mut()
            .events
            .push(egui::Event::ModifiersChanged(Modifiers::CTRL));
        h.input_mut().events.push(egui::Event::Key {
            key: Key::Tab,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL,
        });
        h.run_steps(2);
        assert!(h.state().navigation.switcher.is_some());
        h.input_mut().focused = false;
        h.run_steps(2);
        assert!(
            h.state().navigation.switcher.is_none(),
            "losing focus must dismiss the switcher even if Control release was missed"
        );
        assert_eq!(h.state().active, 0, "focus loss must not choose a tab");
    }
    #[test]
    fn fast_control_tab_release_does_not_leave_an_open_switcher() {
        let mut h = harness(metadata_app("SELECT 1;"));
        h.input_mut().events.extend([
            egui::Event::ModifiersChanged(Modifiers::CTRL),
            egui::Event::Key {
                key: Key::Tab,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::CTRL,
            },
            egui::Event::Key {
                key: Key::Tab,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: Modifiers::CTRL,
            },
            egui::Event::ModifiersChanged(Modifiers::NONE),
        ]);
        h.run_steps(1);
        assert!(
            h.state().navigation.switcher.is_none(),
            "Control was already released in the opening frame"
        );
        assert_eq!(h.state().active, 1);
    }
    #[test]
    fn menu_switcher_close_button_restores_typing_in_the_original_editor() {
        let mut h = harness(metadata_app("SELECT 1;"));
        let ctx = h.ctx.clone();
        h.state_mut().navigation_command(&ctx, Command::Switcher);
        h.run_steps(3);
        assert!(h.state().navigation.switcher.is_some());
        h.get_by_label("Close window").click();
        h.run_steps(3);
        assert!(h.state().navigation.switcher.is_none());
        assert_eq!(h.state().active, 0);
        h.input_mut()
            .events
            .push(egui::Event::Text("-- recovered\n".into()));
        h.run_steps(2);
        assert!(h.state().consoles[0].text.starts_with("-- recovered\n"));
    }
    #[test]
    fn switcher_uses_recent_stable_ids_and_commits_only_on_control_release() {
        let mut app = metadata_app("SELECT 1;");
        app.active = 2;
        app.observe_navigation();
        app.active = 0;
        app.observe_navigation();
        let mut h = harness(app);
        h.input_mut()
            .events
            .push(egui::Event::ModifiersChanged(Modifiers::CTRL));
        h.input_mut().events.push(egui::Event::Key {
            key: Key::Tab,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL,
        });
        h.run_steps(1);
        assert!(h.state().navigation.switcher.is_some());
        assert_eq!(h.state().active, 0);
        assert_eq!(
            h.state().navigation.switcher.as_ref().unwrap().items[1],
            Target::Tab(3)
        );
        h.input_mut().events.push(egui::Event::Key {
            key: Key::ArrowDown,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL,
        });
        h.run_steps(1);
        assert_eq!(h.state().navigation.switcher.as_ref().unwrap().selected, 2);
        h.input_mut().events.push(egui::Event::Key {
            key: Key::ArrowUp,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL,
        });
        h.run_steps(1);
        h.input_mut()
            .events
            .push(egui::Event::ModifiersChanged(Modifiers::NONE));
        h.run_steps(2);
        assert!(h.state().navigation.switcher.is_none());
        assert_eq!(h.state().consoles[h.state().active].id, 3);
        let ctx = h.ctx.clone();
        h.state_mut().navigation_command(&ctx, Command::Switcher);
        h.run_steps(2);
        assert!(
            h.state().navigation.switcher.is_some(),
            "menu-opened switcher waits for a choice"
        );
        h.key_press(Key::Escape);
        h.run_steps(2);
        assert!(h.state().navigation.switcher.is_none());
    }
    #[test]
    fn run_immediately_executes_only_the_outlined_query_through_the_real_sqlite_worker() {
        let path =
            std::env::temp_dir().join(format!("datacraft-navigation-{}.db", uuid::Uuid::new_v4()));
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE notes(id INTEGER PRIMARY KEY);INSERT INTO notes VALUES(1);")
            .unwrap();
        drop(db);
        let mut app = crate::app::tests::tabs_app();
        let mut cfg = ConnectionConfig::new(7, Driver::Sqlite);
        cfg.file_path = path.to_string_lossy().into_owned();
        app.conns.push(ConnState::new(cfg));
        app.object_browser.active = false;
        app.active = 0;
        app.consoles[0].conn = Some(7);
        app.consoles[0].cursor = Some((31, 31));
        app.consoles[0].text = "INSERT INTO notes VALUES (2);\nSELECT count(*) FROM notes;".into();
        let mut h = harness(app);
        h.state_mut().run_console(0, false);
        h.run_steps(2);
        assert!(!h.state().consoles[0].navigation.is_open());
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        while h.state().consoles[0].is_busy() {
            assert!(Instant::now() < deadline, "SQLite query timed out");
            h.run_steps(2);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let console = &h.state().consoles[0];
        assert_eq!(console.last_statements, vec!["SELECT count(*) FROM notes"]);
        assert_eq!(
            console.results.last().unwrap().rs.rows[0][0].as_deref(),
            Some("1")
        );
        let db = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM notes", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        drop(db);
        drop(h);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn immediate_scope_execution_preserves_the_existing_read_only_guard() {
        let mut app = metadata_app("INSERT INTO users VALUES(2); SELECT 1;");
        app.conns[0].cfg.read_only = true;
        app.consoles[0].cursor = Some((0, 0));
        let mut h = harness(app);
        h.state_mut().run_console(0, false);
        h.run_steps(2);
        let c = &h.state().consoles[0];
        assert!(!c.navigation.is_open());
        assert!(!c.is_busy());
        assert!(c.last_statements.is_empty());
        assert!(
            c.log
                .iter()
                .any(|e| e.is_error && e.message.contains("Read-only connection"))
        );
    }
}
