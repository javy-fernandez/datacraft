//! Database object list/grid and the selection's information pane.

use crate::app::{ConnState, ConnStatus};
use crate::db::{ObjectKind, TableInfo, object_info::ObjectInfo};
use crate::db::routines::{RoutineKind, RoutineRef};
use crate::i18n::tr;
use crate::ui::explorer::{ExplorerAction, TableRef};
use crate::ui::icons::{self, Ico};
use crate::ui::object_organization::Organization;
use crate::ui::theme::{self, pal};
use egui::{FontId, Key, Rect, RichText, Sense, Ui, pos2, vec2};
use egui_extras::{Column, TableBuilder};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectScope {
    pub conn: u64,
    pub db: String,
    pub schema: Option<String>,
    pub kind: Option<ObjectKind>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjectLayout {
    #[default]
    List,
    Grid,
}

#[derive(Default)]
pub struct ObjectBrowser {
    pub schema_search: crate::ui::schema_search::SchemaSearch,
    pub organizations: Vec<Organization>,
    current_group: Option<String>,
    selection: BTreeSet<ItemKey>,
    anchor: Option<ItemKey>,
    focused: Option<ItemKey>,
    group_dialog: Option<GroupDialog>,
    marquee: Option<Marquee>,
    pub active: bool,
    pub scope: Option<ObjectScope>,
    routine_kind: Option<RoutineKind>,
    selected_routine: Option<RoutineRef>,
    pub selected: Option<TableRef>,
    pub layout: ObjectLayout,
    inspector_mode: crate::ui::inspector::InspectorMode,
    pub search: String,
    pub info: Option<Result<ObjectInfo, String>>,
    pub loading: bool,
    pub request_id: u64,
    scroll_selection: bool,
    pub catalog: BTreeMap<(String, String), ObjectInfo>,
    pub catalog_loading: bool,
    pub catalog_request_id: u64,
    pub catalog_error: Option<String>,
}

pub enum BrowserAction {
    Search(Box<crate::worker::Request>),
    Explorer(ExplorerAction),
    Inspect { table: TableRef, count_rows: bool },
}

struct Entry<'a> {
    reference: TableRef,
    info: &'a TableInfo,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum ItemKey {
    Group(String),
    Table(TableRef),
}

impl ItemKey {
    fn name(&self) -> &str {
        match self {
            Self::Group(name) => name,
            Self::Table(table) => &table.table,
        }
    }
}

struct Item<'a> {
    key: ItemKey,
    table: Option<&'a TableInfo>,
}

struct ObjectDrag {
    tables: Vec<TableRef>,
}

struct GroupDialog {
    old_name: Option<String>,
    name: String,
    tables: Vec<TableRef>,
    focus: bool,
    invalid: bool,
}

struct Marquee {
    start: egui::Pos2,
    initial: BTreeSet<ItemKey>,
}

impl ObjectBrowser {
    pub fn open(&mut self, scope: ObjectScope) -> bool {
        let changed = self.scope.as_ref() != Some(&scope) || self.routine_kind.is_some();
        self.routine_kind = None;
        if changed {
            self.selected_routine = None;
            self.current_group = None;
            self.clear_selection();
            self.selected = None;
            self.info = None;
            self.loading = false;
            self.search.clear();
            self.request_id = self.request_id.wrapping_add(1);
            self.catalog.clear();
            self.catalog_error = None;
            self.catalog_loading = false;
            self.catalog_request_id = self.catalog_request_id.wrapping_add(1);
        }
        self.scope = Some(scope);
        self.active = true;
        changed
    }

    pub fn open_routines(&mut self, scope: ObjectScope, kind: RoutineKind) {
        self.open(scope);
        self.routine_kind = Some(kind);
    }

    pub fn start_catalog_request(&mut self) -> u64 {
        self.catalog_request_id = self.catalog_request_id.wrapping_add(1);
        self.catalog_loading = true;
        self.catalog_error = None;
        self.catalog_request_id
    }

    pub fn receive_catalog(
        &mut self,
        conn: u64,
        id: u64,
        result: Result<Vec<crate::db::object_info::ObjectInfoRow>, String>,
    ) {
        if self.catalog_request_id != id
            || !self.catalog_loading
            || !self.scope.as_ref().is_some_and(|s| s.conn == conn)
        {
            return;
        }
        self.catalog_loading = false;
        match result {
            Ok(rows) => {
                self.catalog = rows
                    .into_iter()
                    .map(|row| ((row.schema, row.name), row.info))
                    .collect()
            }
            Err(error) => self.catalog_error = Some(error),
        }
    }

    pub fn start_request(&mut self, table: TableRef) -> u64 {
        let key = ItemKey::Table(table.clone());
        if !self.selection.contains(&key) {
            self.selection = BTreeSet::from([key.clone()]);
            self.anchor = Some(key.clone());
            self.focused = Some(key.clone());
        }
        if self.focused.is_none() {
            self.focused = Some(key);
        }
        self.scroll_selection |= self.selected.as_ref() != Some(&table);
        self.selected = Some(table);
        self.info = None;
        self.loading = true;
        self.request_id = self.request_id.wrapping_add(1);
        self.request_id
    }

    pub fn receive(&mut self, conn: u64, request_id: u64, info: Result<ObjectInfo, String>) {
        if self.loading
            && self.request_id == request_id
            && self.selected.as_ref().is_some_and(|t| t.conn == conn)
        {
            self.loading = false;
            if let (Some(table), Ok(info)) = (&self.selected, &info) {
                self.catalog
                    .insert((table.schema.clone(), table.table.clone()), info.clone());
            }
            self.info = Some(info);
        }
    }

    fn entries<'a>(&self, connections: &'a [ConnState]) -> Vec<Entry<'a>> {
        let Some(scope) = &self.scope else {
            return Vec::new();
        };
        let schemas = connections
            .iter()
            .find(|c| c.cfg.id == scope.conn)
            .and_then(|c| c.databases.iter().find(|d| d.name == scope.db))
            .and_then(|d| d.schemas.as_ref());
        let search = self.search.to_lowercase();
        let mut entries: Vec<_> = schemas
            .into_iter()
            .flatten()
            .filter(|s| scope.schema.as_ref().is_none_or(|name| *name == s.name))
            .flat_map(|s| {
                s.tables
                    .iter()
                    .filter(|t| scope.kind.is_none_or(|kind| kind == t.kind))
                    .filter(|t| {
                        search.is_empty()
                            || t.name.to_lowercase().contains(&search)
                            || s.name.to_lowercase().contains(&search)
                    })
                    .map(|t| Entry {
                        reference: TableRef {
                            conn: scope.conn,
                            db: scope.db.clone(),
                            schema: s.name.clone(),
                            table: t.name.clone(),
                        },
                        info: t,
                    })
            })
            .collect();
        entries.sort_by(|a, b| {
            a.reference
                .table
                .to_lowercase()
                .cmp(&b.reference.table.to_lowercase())
                .then_with(|| a.reference.schema.cmp(&b.reference.schema))
                .then_with(|| a.reference.table.cmp(&b.reference.table))
        });
        entries
    }

    fn clear_selection(&mut self) {
        self.selection.clear();
        self.anchor = None;
        self.focused = None;
        self.marquee = None;
        self.selected = None;
        self.info = None;
        self.loading = false;
        self.request_id = self.request_id.wrapping_add(1);
    }

    pub fn has_dialog(&self) -> bool {
        self.group_dialog.is_some()
    }

    fn items<'a>(&self, entries: &[Entry<'a>], organization: &Organization) -> Vec<Item<'a>> {
        let search = self.search.to_lowercase();
        let mut items = Vec::new();
        if self.current_group.is_none() {
            let mut groups = organization.groups.clone();
            groups.sort_by_key(|g| g.to_lowercase());
            items.extend(
                groups
                    .into_iter()
                    .filter(|g| search.is_empty() || g.to_lowercase().contains(&search))
                    .map(|name| Item {
                        key: ItemKey::Group(name),
                        table: None,
                    }),
            );
        }
        items.extend(
            entries
                .iter()
                .filter(|e| {
                    let group = organization.group(&e.reference.schema, &e.reference.table);
                    match self.current_group.as_deref() {
                        Some(current) => group == Some(current),
                        None => !search.is_empty() || group.is_none(),
                    }
                })
                .map(|e| Item {
                    key: ItemKey::Table(e.reference.clone()),
                    table: Some(e.info),
                }),
        );
        items
    }

    fn selected_tables(&self) -> Vec<TableRef> {
        self.selection
            .iter()
            .filter_map(|key| match key {
                ItemKey::Table(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    fn selected_groups(&self) -> Vec<String> {
        self.selection
            .iter()
            .filter_map(|key| match key {
                ItemKey::Group(name) => Some(name.clone()),
                _ => None,
            })
            .collect()
    }

    fn select_item(
        &mut self,
        key: &ItemKey,
        keys: &[ItemKey],
        mods: egui::Modifiers,
        actions: &mut Vec<BrowserAction>,
    ) {
        if mods.shift {
            let a = self
                .anchor
                .as_ref()
                .and_then(|k| keys.iter().position(|v| v == k))
                .unwrap_or(0);
            let b = keys.iter().position(|k| k == key).unwrap_or(a);
            if !mods.command {
                self.selection.clear();
            }
            self.selection
                .extend(keys[a.min(b)..=a.max(b)].iter().cloned());
        } else if mods.command {
            if !self.selection.remove(key) {
                self.selection.insert(key.clone());
            }
            self.anchor = Some(key.clone());
        } else {
            self.selection = BTreeSet::from([key.clone()]);
            self.anchor = Some(key.clone());
        }
        self.focused = Some(key.clone());
        let primary = match key {
            ItemKey::Table(t) if self.selection.contains(key) => Some(t.clone()),
            _ => self.selected_tables().first().cloned(),
        };
        if self.selected != primary {
            self.selected = primary.clone();
            self.info = None;
            self.loading = false;
            self.request_id = self.request_id.wrapping_add(1);
            if let Some(table) = primary {
                actions.push(BrowserAction::Inspect {
                    table,
                    count_rows: false,
                });
            }
        }
    }

    fn enter_group(&mut self, group: Option<String>) {
        self.current_group = group;
        self.clear_selection();
    }

    fn new_group_dialog(&mut self, old_name: Option<String>, tables: Vec<TableRef>) {
        self.group_dialog = Some(GroupDialog {
            name: old_name.clone().unwrap_or_default(),
            old_name,
            tables,
            focus: true,
            invalid: false,
        });
    }

    fn group_dialog_ui(&mut self, ctx: &egui::Context, org_index: usize) {
        let Some(dialog) = &mut self.group_dialog else {
            return;
        };
        let mut close = false;
        let mut created = None;
        egui::Modal::new(egui::Id::new("object_group_dialog")).show(ctx, |ui| {
            ui.set_width(320.0);
            ui.heading(if dialog.old_name.is_some() {
                tr("Rename group")
            } else {
                tr("New group")
            });
            let response = ui.add(
                egui::TextEdit::singleline(&mut dialog.name)
                    .char_limit(120)
                    .hint_text(tr("Group name")),
            );
            if std::mem::take(&mut dialog.focus) {
                response.request_focus();
            }
            if dialog.invalid {
                ui.colored_label(ui.visuals().error_fg_color, tr("Enter a unique group name"));
            }
            ui.horizontal(|ui| {
                let enter = (response.has_focus() || response.lost_focus())
                    && ui.input(|i| i.key_pressed(Key::Enter));
                if ui.button(tr("Save")).clicked() || enter {
                    let name = dialog.name.trim().to_string();
                    let organization = &mut self.organizations[org_index];
                    let valid = match &dialog.old_name {
                        Some(old) if old == &name => true,
                        Some(old) => organization.rename(old, &name),
                        None => organization.create(&name),
                    };
                    if valid {
                        organization.assign(&dialog.tables, Some(&name));
                        if dialog.old_name.is_none() {
                            created = Some(name.clone());
                        }
                        if self.current_group == dialog.old_name && dialog.old_name.is_some() {
                            self.current_group = Some(name);
                        }
                        close = true;
                    } else {
                        dialog.invalid = true;
                    }
                }
                if ui.button(tr("Cancel")).clicked() {
                    close = true;
                }
            });
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                close = true;
            }
        });
        if close {
            self.group_dialog = None;
        }
        if let Some(name) = created {
            self.enter_group(None);
            let key = ItemKey::Group(name);
            self.selection.insert(key.clone());
            self.focused = Some(key.clone());
            self.anchor = Some(key);
        }
    }

    pub fn show(&mut self, ui: &mut Ui, connections: &[ConnState]) -> Vec<BrowserAction> {
        ui.set_min_size(ui.available_size());
        let mut actions = self.schema_search.show(ui.ctx(), connections);
        let Some(scope) = self.scope.clone() else {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() / 3.0);
                ui.label(RichText::new(tr("Select a database in the explorer to browse its objects.")).color(pal(ui).muted));
            });
            return actions;
        };
        if let Some(kind) = self.routine_kind {
            self.show_routines(ui, connections, &scope, kind, &mut actions);
            return actions;
        }
        let org_index = self
            .organizations
            .iter()
            .position(|o| o.conn == scope.conn && o.db == scope.db)
            .unwrap_or_else(|| {
                let mut organization = Organization::default();
                organization.conn = scope.conn;
                organization.db.clone_from(&scope.db);
                self.organizations.push(organization);
                self.organizations.len() - 1
            });
        self.group_dialog_ui(ui.ctx(), org_index);
        if self
            .current_group
            .as_ref()
            .is_some_and(|g| !self.organizations[org_index].groups.contains(g))
        {
            self.enter_group(None);
        }
        let p = pal(ui);
        let connection = connections.iter().find(|c| c.cfg.id == scope.conn);
        let connected = connection.is_some_and(|c| c.status == ConnStatus::Connected);
        let writable = connected && connection.is_some_and(|c| !c.cfg.read_only);
        // New / Edit View: SQL engines only.
        let views = writable && connection.is_some_and(|c| !c.cfg.driver.is_nosql());
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            if let Some(group) = self.current_group.clone() {
                let back = ui.button(tr("All objects"));
                if back.clicked() {
                    self.enter_group(None);
                }
                if let Some(payload) = back.dnd_release_payload::<ObjectDrag>()
                    && self.organizations[org_index].assign(&payload.tables, None)
                {
                    self.enter_group(None);
                }
                let color = self.organizations[org_index]
                    .group_color(&group)
                    .unwrap_or(p.text);
                ui.label(RichText::new(group).font(theme::bold(16.0)).color(color));
            } else {
                let title = match scope.kind {
                    Some(ObjectKind::Table) => tr("Tables"),
                    Some(ObjectKind::View) => tr("Views"),
                    None => tr("Objects"),
                };
                ui.label(RichText::new(title).font(theme::bold(16.0)));
            }
            ui.label(
                RichText::new(match &scope.schema {
                    Some(s) => format!("{} / {s}", scope.db),
                    None => scope.db.clone(),
                })
                .color(p.muted),
            );

        });
        ui.add_space(8.0);
        ui.separator();
        if self.catalog_loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(tr("Loading object statistics…"));
            });
        }
        if let Some(error) = &self.catalog_error {
            ui.colored_label(p.error, error);
        }
        if !connected {
            ui.label(
                RichText::new(tr("Connect to load current object information")).color(p.muted),
            );
        }
        let entries = self.entries(connections);
        let items = self.items(&entries, &self.organizations[org_index]);
        let keys: Vec<_> = items.iter().map(|i| i.key.clone()).collect();
        let visible: BTreeSet<_> = keys.iter().collect();
        self.selection.retain(|key| visible.contains(key));
        if self
            .selected
            .as_ref()
            .is_some_and(|t| !keys.contains(&ItemKey::Table(t.clone())))
        {
            self.selected = None;
            self.info = None;
            self.loading = false;
            self.request_id = self.request_id.wrapping_add(1);
        }
        ui.set_clip_rect(ui.clip_rect().intersect(ui.available_rect_before_wrap()));
        let background = ui.available_rect_before_wrap();
        let grid_rows = ((ui.available_height() - 14.0) / (22.0 + ui.spacing().item_spacing.y))
            .floor()
            .max(1.0) as usize;
        if !items.is_empty()
            && ui.is_enabled()
            && !egui::Popup::is_any_open(ui.ctx())
            && ui.memory(|m| m.focused().is_none())
        {
            let input = ui.input_mut(|i| {
                let all = i.consume_key(egui::Modifiers::COMMAND, Key::A);
                let key = [
                    Key::ArrowDown,
                    Key::ArrowUp,
                    Key::ArrowLeft,
                    Key::ArrowRight,
                    Key::Home,
                    Key::End,
                    Key::Enter,
                ]
                .into_iter()
                .find(|key| {
                    i.consume_key(
                        if i.modifiers.shift {
                            egui::Modifiers::SHIFT
                        } else {
                            egui::Modifiers::NONE
                        },
                        *key,
                    )
                });
                (all, key, i.modifiers)
            });
            if input.0 {
                self.selection = keys.iter().cloned().collect();
                self.focused = keys
                    .iter()
                    .find(|k| matches!(k, ItemKey::Table(_)))
                    .or(keys.first())
                    .cloned();
                self.anchor = self.focused.clone();
            }
            if let Some(key) = input.1 {
                let current = self
                    .focused
                    .as_ref()
                    .and_then(|k| keys.iter().position(|v| v == k));
                let next = object_navigation(
                    current,
                    key,
                    keys.len(),
                    if self.layout == ObjectLayout::Grid {
                        grid_rows
                    } else {
                        keys.len()
                    },
                );
                if key == Key::Enter {
                    match &keys[next] {
                        ItemKey::Group(name) => self.enter_group(Some(name.clone())),
                        ItemKey::Table(t) => actions.push(BrowserAction::Explorer(
                            ExplorerAction::OpenTable(t.clone()),
                        )),
                    }
                } else {
                    self.select_item(&keys[next], &keys, input.2, &mut actions);
                    self.scroll_selection = true;
                }
            }
        }
        let scroll_to = if std::mem::take(&mut self.scroll_selection) {
            self.focused
                .as_ref()
                .and_then(|k| keys.iter().position(|v| v == k))
        } else {
            None
        };
        let organization = &self.organizations[org_index];
        let mut responses = Vec::new();
        match self.layout {
            ObjectLayout::List => {
                egui::ScrollArea::horizontal()
                    .id_salt("object_details_horizontal")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_min_width(1160.0);
                        ui.style_mut().override_font_id = Some(FontId::proportional(12.0));
                        let mut table = TableBuilder::new(ui)
                            .id_salt("object_list")
                            .resizable(true)
                            .sense(Sense::click_and_drag());
                        if let Some(row) = scroll_to {
                            table = table.scroll_to_row(row, None);
                        }
                        table
                            .column(Column::initial(220.0).at_least(100.0).clip(true))
                            .column(Column::initial(75.0).clip(true))
                            .column(Column::initial(95.0).clip(true))
                            .column(Column::initial(90.0).clip(true))
                            .column(Column::initial(155.0).clip(true))
                            .column(Column::initial(155.0).clip(true))
                            .column(Column::initial(170.0).clip(true))
                            .column(Column::remainder().at_least(140.0).clip(true))
                            .header(23.0, |mut header| {
                                for label in [
                                    tr("Name"),
                                    tr("Rows"),
                                    tr("Data length"),
                                    tr("Engine"),
                                    tr("Created date"),
                                    tr("Modified date"),
                                    tr("Collation"),
                                    tr("Comment"),
                                ] {
                                    header.col(|ui| {
                                        ui.label(RichText::new(label).strong().color(p.text));
                                    });
                                }
                            })
                            .body(|body| {
                                body.rows(21.0, items.len(), |mut row| {
                                    let item = &items[row.index()];
                                    let selected = self.selection.contains(&item.key);
                                    row.set_selected(selected);
                                    row.col(|ui| {
                                        ui.horizontal(|ui| {
                                            let (rect, _) = ui.allocate_exact_size(
                                                vec2(16.0, 16.0),
                                                Sense::hover(),
                                            );
                                            let color =
                                                item_color(item, organization).unwrap_or(p.icon);
                                            icons::paint(
                                                ui.painter(),
                                                rect,
                                                item.table
                                                    .map_or(Ico::Folder, |t| object_icon(t.kind)),
                                                color,
                                                &p,
                                                true,
                                            );
                                            ui.add(
                                                egui::Label::new(
                                                    RichText::new(item.key.name()).color(
                                                        if selected {
                                                            p.text
                                                        } else {
                                                            item_color(item, organization)
                                                                .unwrap_or(p.text)
                                                        },
                                                    ),
                                                )
                                                .truncate(),
                                            );
                                        });
                                    });
                                    let info = match &item.key {
                                        ItemKey::Table(t) => {
                                            self.catalog.get(&(t.schema.clone(), t.table.clone()))
                                        }
                                        _ => None,
                                    };
                                    let rows = info.and_then(|i| i.rows).map(|n| {
                                        if info.is_some_and(|i| i.rows_exact) {
                                            n.to_string()
                                        } else {
                                            format!("≈ {n}")
                                        }
                                    });
                                    let size = info.and_then(|i| i.data_bytes).map(format_bytes);
                                    for value in [
                                        rows.as_deref(),
                                        size.as_deref(),
                                        info.and_then(|i| i.engine.as_deref()),
                                        info.and_then(|i| i.created.as_deref()),
                                        info.and_then(|i| i.modified.as_deref()),
                                        info.and_then(|i| i.collation.as_deref()),
                                        info.and_then(|i| i.comment.as_deref()),
                                    ] {
                                        row.col(|ui| {
                                            ui.add(
                                                egui::Label::new(
                                                    RichText::new(value.unwrap_or("—"))
                                                        .color(p.text),
                                                )
                                                .truncate(),
                                            );
                                        });
                                    }
                                    let response = row.response();
                                    if matches!(item.key, ItemKey::Group(_))
                                        && response.dnd_hover_payload::<ObjectDrag>().is_some()
                                    {
                                        response
                                            .ctx
                                            .layer_painter(response.layer_id)
                                            .with_clip_rect(background)
                                            .rect_stroke(
                                                response.rect,
                                                2.0,
                                                egui::Stroke::new(2.0, p.accent),
                                                egui::StrokeKind::Inside,
                                            );
                                    }
                                    responses.push((item, response));
                                });
                            });
                    });
            }
            ObjectLayout::Grid => {
                let pitch = 22.0 + ui.spacing().item_spacing.y;
                let width = 240.0;
                let columns = items.len().div_ceil(grid_rows);
                let height = (ui.available_height() - 14.0).max(22.0);
                let mut scroll = egui::ScrollArea::horizontal()
                    .id_salt("object_grid")
                    .auto_shrink([false, false]);
                if let Some(index) = scroll_to {
                    scroll = scroll.horizontal_scroll_offset((index / grid_rows) as f32 * width);
                }
                scroll.show_viewport(ui, |ui, viewport| {
                    ui.set_min_size(vec2(columns as f32 * width, height));
                    let origin = ui.min_rect().min;
                    let first = (viewport.min.x / width).floor().max(0.0) as usize;
                    let last = ((viewport.max.x / width).ceil() as usize + 1).min(columns);
                    for column in first..last {
                        for row in 0..grid_rows {
                            let index = column * grid_rows + row;
                            let Some(item) = items.get(index) else { break };
                            let rect = Rect::from_min_size(
                                origin + vec2(column as f32 * width, row as f32 * pitch),
                                vec2(width - 6.0, 22.0),
                            );
                            let response = ui.interact(
                                rect,
                                ui.id().with(("object_cell", &item.key)),
                                Sense::click_and_drag(),
                            );
                            let selected = self.selection.contains(&item.key);
                            response.widget_info(|| {
                                egui::WidgetInfo::selected(
                                    egui::WidgetType::SelectableLabel,
                                    ui.is_enabled(),
                                    selected,
                                    item.key.name(),
                                )
                            });
                            if selected || response.hovered() {
                                ui.painter().rect_filled(
                                    rect.shrink(1.0),
                                    3.0,
                                    if selected { p.selection } else { p.row_hover },
                                );
                            }
                            if matches!(item.key, ItemKey::Group(_))
                                && response.dnd_hover_payload::<ObjectDrag>().is_some()
                            {
                                ui.painter().rect_stroke(
                                    rect,
                                    2.0,
                                    egui::Stroke::new(2.0, p.accent),
                                    egui::StrokeKind::Inside,
                                );
                            }
                            let icon = Rect::from_min_size(
                                pos2(rect.left() + 5.0, rect.center().y - 8.0),
                                vec2(16.0, 16.0),
                            );
                            icons::paint(
                                ui.painter(),
                                icon,
                                item.table.map_or(Ico::Folder, |t| object_icon(t.kind)),
                                item_color(item, organization).unwrap_or(p.icon),
                                &p,
                                true,
                            );
                            ui.painter_at(rect.shrink(3.0)).text(
                                pos2(rect.left() + 27.0, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                item.key.name(),
                                FontId::proportional(12.0),
                                if selected {
                                    p.text
                                } else {
                                    item_color(item, organization).unwrap_or(p.text)
                                },
                            );
                            responses.push((item, response));
                        }
                    }
                });
            }
        }
        let pointer = ui.input(|i| i.pointer.hover_pos());
        let over_object = responses
            .iter()
            .any(|(_, r)| pointer.is_some_and(|p| r.rect.contains(p)));
        let rects: Vec<_> = responses
            .iter()
            .map(|(item, r)| (item.key.clone(), r.rect.intersect(background)))
            .collect();
        for (item, response) in responses {
            self.item_response(response, item, &keys, org_index, &items, crate::ui::table_menu::TableContext { driver: connection.map_or(crate::db::Driver::Sqlite, |c| c.cfg.driver), connected, read_only: !writable }, &mut actions);
        }
        if items.is_empty() {
            ui.label(RichText::new(tr("No matching objects")).color(p.muted));
        }
        if !over_object || self.marquee.is_some() {
            self.background_menu(ui, background, &scope, &rects, views, &mut actions);
        }
        if let Some(payload) = egui::DragAndDrop::payload::<ObjectDrag>(ui.ctx())
            && let Some(position) = pointer
        {
            egui::Area::new(egui::Id::new("object_drag_label"))
                .order(egui::Order::Tooltip)
                .fixed_pos(position + vec2(14.0, 14.0))
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.label(crate::i18n::trf(
                            "{n} tables",
                            &[("n", &payload.tables.len())],
                        ));
                    });
                });
        }
        actions
    }

    /// `copyable`: "Copy Table To…" is offered (SQL tables and MongoDB collections, not Redis).
    #[allow(clippy::too_many_arguments)]
    fn item_response(
        &mut self,
        response: egui::Response,
        item: &Item<'_>,
        keys: &[ItemKey],
        org_index: usize,
        items: &[Item<'_>],
        table_context: crate::ui::table_menu::TableContext,
        actions: &mut Vec<BrowserAction>,
    ) {
        let mods = response.ctx.input(|i| i.modifiers);
        if response.clicked() {
            self.select_item(&item.key, keys, mods, actions);
        }
        if response.secondary_clicked() && !self.selection.contains(&item.key) {
            self.select_item(&item.key, keys, egui::Modifiers::NONE, actions);
        }
        if response.double_clicked() {
            match &item.key {
                ItemKey::Group(name) => self.enter_group(Some(name.clone())),
                ItemKey::Table(table) => actions.push(BrowserAction::Explorer(
                    ExplorerAction::OpenTable(table.clone()),
                )),
            }
        }
        if response.drag_started_by(egui::PointerButton::Primary)
            && matches!(item.key, ItemKey::Table(_))
        {
            if !self.selection.contains(&item.key) {
                self.select_item(&item.key, keys, egui::Modifiers::NONE, actions);
            }
            response.dnd_set_drag_payload(ObjectDrag {
                tables: self.selected_tables(),
            });
        }
        if let ItemKey::Group(name) = &item.key
            && let Some(payload) = response.dnd_release_payload::<ObjectDrag>()
            && self.organizations[org_index].assign(&payload.tables, Some(name))
        {
            self.clear_selection();
        }
        response.context_menu(|ui| match &item.key {
            ItemKey::Group(name) => {
                let groups = self.selected_groups();
                let tables = self.selected_tables();
                self.organizations[org_index].color_menu(ui, &tables, &groups);
                ui.separator();
                if ui.button(tr("Open group")).clicked() {
                    self.enter_group(Some(name.clone()));
                    ui.close();
                }
                if ui.button(tr("Rename group")).clicked() {
                    self.new_group_dialog(Some(name.clone()), Vec::new());
                    ui.close();
                }
                if ui.button(tr("Delete group")).clicked() {
                    self.organizations[org_index].remove(name);
                    self.clear_selection();
                    ui.close();
                }
            }
            ItemKey::Table(_) => {
                let tables = self.selected_tables();
                let groups = self.selected_groups();
                if self.organizations[org_index].menu(ui, &tables, &groups) {
                    self.new_group_dialog(None, tables.clone());
                }
                ui.separator();
                if !groups.is_empty() { ui.label(tr("Select only tables for table actions")); return; }
                let mut kinds = items.iter().filter(|item| self.selection.contains(&item.key)).filter_map(|item| item.table.map(|t| t.kind));
                let kind = kinds.next().filter(|first| kinds.all(|kind| kind == *first));
                let mut selected_actions = Vec::new();
                crate::ui::table_menu::show(ui, &tables, kind, table_context, &mut selected_actions);
                actions.extend(selected_actions.into_iter().map(BrowserAction::Explorer));
            }
        });
    }

    fn background_menu(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        scope: &ObjectScope,
        rects: &[(ItemKey, Rect)],
        views: bool,
        actions: &mut Vec<BrowserAction>,
    ) {
        let response = ui.interact(
            rect,
            ui.id().with("object_background"),
            Sense::click_and_drag(),
        );
        if response.clicked() {
            self.clear_selection();
        }
        if response.drag_started_by(egui::PointerButton::Primary)
            && let Some(start) = ui.input(|i| i.pointer.press_origin())
        {
            let initial = if ui.input(|i| i.modifiers.command || i.modifiers.shift) {
                self.selection.clone()
            } else {
                BTreeSet::new()
            };
            self.clear_selection();
            self.marquee = Some(Marquee { start, initial });
        }
        if let Some(marquee) = &self.marquee
            && let Some(pointer) = ui.input(|i| i.pointer.hover_pos())
        {
            let area = Rect::from_two_pos(marquee.start, pointer).intersect(rect);
            self.selection = marquee.initial.clone();
            self.selection.extend(
                rects
                    .iter()
                    .filter(|(_, r)| r.is_positive() && area.intersects(*r))
                    .map(|(key, _)| key.clone()),
            );
            ui.painter()
                .rect_filled(area, 0.0, pal(ui).selection.gamma_multiply(0.35));
            ui.painter().rect_stroke(
                area,
                0.0,
                egui::Stroke::new(1.0, pal(ui).accent),
                egui::StrokeKind::Inside,
            );
            ui.ctx().request_repaint();
            if ui.input(|i| i.pointer.any_released() || !i.pointer.primary_down()) {
                self.marquee = None;
            }
        }
        response.context_menu(|ui| {
            if ui.button(tr("Find in Schema…")).clicked() {
                self.schema_search.open(scope);
                ui.close();
            }
            if ui.button(tr("New group…")).clicked() {
                self.new_group_dialog(None, Vec::new());
                ui.close();
            }
            if views && ui.button(tr("New View…")).clicked() {
                actions.push(BrowserAction::Explorer(ExplorerAction::NewView {
                    conn: scope.conn,
                    db: scope.db.clone(),
                    schema: scope.schema.clone(),
                }));
                ui.close();
            }
            if ui.button(tr("Refresh objects")).clicked() {
                actions.push(BrowserAction::Explorer(ExplorerAction::LoadDatabase(
                    scope.conn,
                    scope.db.clone(),
                )));
                ui.close();
            }
        });
    }

    fn show_routines(&mut self, ui: &mut Ui, connections: &[ConnState], scope: &ObjectScope, kind: RoutineKind, actions: &mut Vec<BrowserAction>) {
        let p = pal(ui);
        let icon = if kind == RoutineKind::Procedure { Ico::Procedure } else { Ico::Function };
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            let (rect, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
            icons::paint(ui.painter(), rect, icon, p.routine_icon, &p, true);
            ui.label(RichText::new(if kind == RoutineKind::Procedure { tr("Procedures") } else { tr("Functions") }).font(theme::bold(16.0)));
            ui.label(RichText::new(format!("{} / {}", scope.db, scope.schema.as_deref().unwrap_or_default())).color(p.muted));
        });
        ui.add_space(8.0);
        ui.separator();
        let Some(connection) = connections.iter().find(|c| c.cfg.id == scope.conn) else { return };
        let Some(list) = connection.routines.get(&scope.db) else { return };
        if list.loading { ui.spinner(); }
        if let Some(error) = &list.error { ui.colored_label(p.error, error); }
        let search = self.search.to_lowercase();
        let mut items: Vec<_> = list.items.iter().flatten().filter(|r| r.kind == kind && scope.schema.as_ref().is_none_or(|s| *s == r.schema) && (r.name.to_lowercase().contains(&search) || r.schema.to_lowercase().contains(&search))).collect();
        items.sort_by_key(|r| (r.name.to_lowercase(), &r.schema, &r.signature));
        let mut responses = Vec::new();
        if self.layout == ObjectLayout::List {
            TableBuilder::new(ui).id_salt("routine_list").resizable(true).sense(Sense::click())
                .column(Column::initial(260.0).at_least(160.0)).column(Column::initial(240.0)).column(Column::remainder().at_least(120.0))
                .header(26.0, |mut header| {
                    for label in [tr("Name"), tr("Parameters"), tr("Returns")] { header.col(|ui| { ui.strong(label); }); }
                }).body(|body| {
                    body.rows(26.0, items.len(), |mut row| {
                        let routine = items[row.index()];
                        let reference = RoutineRef::new(scope.conn, &scope.db, routine);
                        row.set_selected(self.selected_routine.as_ref() == Some(&reference));
                        row.col(|ui| { ui.horizontal(|ui| {
                            let (rect, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                            icons::paint(ui.painter(), rect, icon, p.routine_icon, &p, true);
                            ui.add(egui::Label::new(&routine.name).truncate());
                        }); });
                        row.col(|ui| { ui.add(egui::Label::new(&routine.arguments).truncate()); });
                        row.col(|ui| { ui.add(egui::Label::new(&routine.returns).truncate()); });
                        let response = row.response();
                        response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, self.selected_routine.as_ref() == Some(&reference), routine.display()));
                        responses.push((routine, reference, response));
                    });
                });
        } else {
            let rows = ((ui.available_height() - 14.0) / 28.0).floor().max(1.0) as usize;
            let columns = items.len().div_ceil(rows);
            let height = (ui.available_height() - 14.0).max(28.0);
            egui::ScrollArea::horizontal().auto_shrink([false, false]).show(ui, |ui| {
                ui.set_min_size(vec2(columns as f32 * 280.0, height));
                let origin = ui.min_rect().min;
                for (index, routine) in items.iter().enumerate() {
                    let reference = RoutineRef::new(scope.conn, &scope.db, routine);
                    let rect = Rect::from_min_size(origin + vec2((index / rows) as f32 * 280.0, (index % rows) as f32 * 28.0), vec2(274.0, 26.0));
                    let response = ui.interact(rect, ui.id().with(("routine", &reference)), Sense::click());
                    let selected = self.selected_routine.as_ref() == Some(&reference);
                    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, routine.display()));
                    if selected || response.hovered() { ui.painter().rect_filled(rect.shrink(1.0), 3.0, if selected { p.selection } else { p.row_hover }); }
                    icons::paint(ui.painter(), Rect::from_min_size(rect.left_center() + vec2(5.0, -8.0), vec2(16.0, 16.0)), icon, p.routine_icon, &p, true);
                    ui.painter_at(rect).text(rect.left_center() + vec2(28.0, 0.0), egui::Align2::LEFT_CENTER, &routine.name, FontId::proportional(12.5), p.text);
                    responses.push((*routine, reference, response));
                }
            });
        }
        for (routine, reference, response) in responses {
            if response.clicked() || response.secondary_clicked() { self.selected_routine = Some(reference.clone()); }
            if response.double_clicked() { actions.push(BrowserAction::Explorer(ExplorerAction::EditRoutine(reference.clone()))); }
            response.on_hover_text(routine.display()).context_menu(|ui| {
                for (icon, label, action) in [
                    (Ico::Play, tr("Execute…"), ExplorerAction::ExecuteRoutine(reference.clone())),
                    (Ico::Edit, if kind == RoutineKind::Procedure { tr("Edit Procedure…") } else { tr("Edit Function…") }, ExplorerAction::EditRoutine(reference.clone())),
                    (Ico::Ddl, tr("Generate DDL"), ExplorerAction::RoutineDdl(reference.clone())),
                ] {
                    if theme::flat_button(ui, Some(icon), label, FontId::proportional(12.5), false, connection.status == ConnStatus::Connected).clicked() { actions.push(BrowserAction::Explorer(action)); ui.close(); }
                }
            });
        }
        if items.is_empty() && !list.loading { ui.label(RichText::new(tr("No matching objects")).color(p.muted)); }
    }

    pub fn status_controls(&mut self, ui: &mut Ui, connected: bool) -> Option<ExplorerAction> {
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::F)) {
            ui.memory_mut(|m| m.request_focus(egui::Id::new("object_search")));
        }
        if theme::icon_tab(ui, Ico::Grid, tr("Grid"), self.layout == ObjectLayout::Grid).clicked() { self.layout = ObjectLayout::Grid; }
        if theme::icon_tab(ui, Ico::List, tr("List"), self.layout == ObjectLayout::List).clicked() { self.layout = ObjectLayout::List; }
        let refresh = theme::icon_button_sized(ui, Ico::Refresh, connected, tr("Refresh objects"), 24.0).clicked();
        let previous = self.search.clone();
        let search = ui.add(egui::TextEdit::singleline(&mut self.search).id(egui::Id::new("object_search")).desired_width(180.0).hint_text(tr("Search objects…")));
        search.widget_info(|| egui::WidgetInfo { label: Some(tr("Search objects…").into()), ..egui::WidgetInfo::text_edit(ui.is_enabled(), &previous, &self.search, tr("Search objects…")) });
        if refresh { self.scope.as_ref().map(|scope| ExplorerAction::LoadDatabase(scope.conn, scope.db.clone())) } else { None }
    }

    pub fn show_inspector(&mut self, ui: &mut Ui, connections: &[ConnState]) -> Vec<BrowserAction> {
        let mut actions = Vec::new();
        let Some(scope) = &self.scope else { return actions; };
        let connection = connections.iter().find(|connection| connection.cfg.id == scope.conn);
        let connected = connection.is_some_and(|connection| connection.status == ConnStatus::Connected);
        if let Some(kind) = self.routine_kind {
            let routine = self.selected_routine.as_ref().and_then(|selected| connection?.routines.get(&scope.db)?.items.as_ref()?.iter().find(|r| RoutineRef::new(scope.conn, &scope.db, r) == *selected));
            egui::Panel::right("object_information").default_size(300.0).size_range(220.0..=520.0).resizable(true).show(ui, |ui| {
                let p = pal(ui);
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
                    icons::paint(ui.painter(), rect, if kind == RoutineKind::Procedure { Ico::Procedure } else { Ico::Function }, p.routine_icon, &p, true);
                    ui.strong(routine.map_or_else(|| if kind == RoutineKind::Procedure { tr("Procedures") } else { tr("Functions") }, |r| r.name.as_str()));
                });
                ui.separator();
                if let Some(routine) = routine {
                    ui.label(RichText::new(format!("{} / {}", scope.db, routine.schema)).color(p.muted));
                    ui.add_space(12.0);
                    ui.strong(tr("Parameters"));
                    ui.add(egui::Label::new(RichText::new(format!("({})", routine.arguments)).monospace()).wrap());
                    if !routine.returns.is_empty() { ui.strong(tr("Returns")); ui.label(&routine.returns); }
                    ui.add_space(12.0);
                    let reference = RoutineRef::new(scope.conn, &scope.db, routine);
                    if theme::flat_button(ui, Some(Ico::Edit), if kind == RoutineKind::Procedure { tr("Edit Procedure…") } else { tr("Edit Function…") }, FontId::proportional(12.5), false, connected).clicked() { actions.push(BrowserAction::Explorer(ExplorerAction::EditRoutine(reference.clone()))); }
                    if theme::flat_button(ui, Some(Ico::Play), tr("Execute…"), FontId::proportional(12.5), false, connected).clicked() { actions.push(BrowserAction::Explorer(ExplorerAction::ExecuteRoutine(reference))); }
                } else {
                    ui.label(tr("Select an object to see its information"));
                }
            });
            return actions;
        }
        let entries = self.entries(connections);
        egui::Panel::right("object_information")
            .default_size(300.0)
            .size_range(220.0..=520.0)
            .resizable(true)
            .show(ui, |ui| {
                if self.selection.len() > 1 {
                    ui.label(crate::i18n::trf(
                        "{n} objects selected",
                        &[("n", &self.selection.len())],
                    ));
                }
                self.information(ui, &entries, connection, connected, &mut actions);
            });
        actions
    }

    fn information(
        &mut self,
        ui: &mut Ui,
        entries: &[Entry<'_>],
        connection: Option<&ConnState>,
        connected: bool,
        actions: &mut Vec<BrowserAction>,
    ) {
        use crate::ui::inspector::{self, InspectorMode};
        let p = pal(ui);
        ui.horizontal(|ui| {
            inspector::tabs(ui, &mut self.inspector_mode, false);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let more = theme::icon_button_sized(ui, Ico::MoreVert, self.selected.is_some(), tr("Actions"), 24.0);
                egui::Popup::menu(&more).show(|ui| {
                    if let Some(selected) = &self.selected {
                        for (label, action) in [(tr("Open data"), ExplorerAction::OpenTable(selected.clone())), ("DDL", ExplorerAction::GenerateDdl(selected.clone()))] {
                            if ui.add_enabled(connected, egui::Button::new(label)).clicked() { actions.push(BrowserAction::Explorer(action)); ui.close(); }
                        }
                        if ui.add_enabled(connected && !self.loading, egui::Button::new(tr("Count rows exactly"))).on_hover_text(tr("Runs COUNT(*). Large tables and views may take time.")).clicked() {
                            actions.push(BrowserAction::Inspect { table: selected.clone(), count_rows: true }); ui.close();
                        }
                    }
                });
                if theme::icon_button_sized(ui, Ico::Refresh, connected && !self.loading && self.selected.is_some(), tr("Refresh information"), 24.0).clicked() && let Some(selected) = &self.selected {
                    actions.push(BrowserAction::Inspect { table: selected.clone(), count_rows: false });
                }
            });
        });
        let connection_name = connection.map_or("", |c| c.cfg.name.as_str());
        egui::ScrollArea::vertical().id_salt("object_information_scroll").auto_shrink([false, false]).show(ui, |ui| {
            let Some(selected) = &self.selected else {
                if let Some(scope) = &self.scope {
                    inspector::header(ui, &scope.db, tr("Database"), connection_name, scope.schema.as_deref().unwrap_or(""), Ico::Database);
                    ui.label(RichText::new(tr("Select an object to see its information")).small().color(p.muted));
                }
                return;
            };
            let table = entries.iter().find(|e| &e.reference == selected).map(|e| e.info);
            let kind = table.map(|t| t.kind).unwrap_or(ObjectKind::Table);
            inspector::header(ui, &selected.table, object_kind(kind), connection_name, &selected.db, object_icon(kind));
            if self.loading { ui.spinner(); }
            if let Some(Err(error)) = &self.info { ui.colored_label(p.error, error); }
            if self.inspector_mode == InspectorMode::Columns {
                if let Some(table) = table { for column in &table.columns {
                    let value = format!("{}{}{}", column.data_type, if column.nullable { "" } else { " · NOT NULL" }, if column.primary_key { " · PK" } else { "" });
                    inspector::property(ui, &column.name, Some(&value));
                } }
            } else {
                inspector::table_fields(ui, self.info.as_ref().and_then(|result| result.as_ref().ok()));
            }
        });
    }

}

fn object_navigation(
    current: Option<usize>,
    key: Key,
    total: usize,
    rows_per_column: usize,
) -> usize {
    let i = current.unwrap_or(0);
    match key {
        Key::ArrowDown if i % rows_per_column + 1 < rows_per_column => {
            (i + usize::from(current.is_some())).min(total - 1)
        }
        Key::ArrowUp if !i.is_multiple_of(rows_per_column) => i - 1,
        Key::ArrowLeft if i >= rows_per_column => i - rows_per_column,
        Key::ArrowRight if i + rows_per_column < total => i + rows_per_column,
        Key::Home => 0,
        Key::End => total - 1,
        _ => i,
    }
}

fn object_icon(kind: ObjectKind) -> Ico {
    if kind == ObjectKind::Table {
        Ico::Table
    } else {
        Ico::View
    }
}
fn object_kind(kind: ObjectKind) -> &'static str {
    if kind == ObjectKind::Table {
        tr("Table")
    } else {
        tr("View")
    }
}
pub(crate) fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn item_color(item: &Item<'_>, organization: &Organization) -> Option<egui::Color32> {
    match &item.key {
        ItemKey::Table(t) => organization.color(&t.schema, &t.table),
        ItemKey::Group(name) => organization.group_color(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ColumnInfo, ConnectionConfig, DatabaseInfo, Driver, SchemaInfo};
    use egui_kittest::kittest::{NodeT, Queryable};

    fn connection() -> ConnState {
        let mut connection = ConnState::new(ConnectionConfig::new(7, Driver::Postgres));
        connection.status = ConnStatus::Connected;
        let columns = vec![
            ColumnInfo {
                name: "id".into(),
                data_type: "integer".into(),
                nullable: false,
                primary_key: true,
            },
            ColumnInfo {
                name: "name".into(),
                data_type: "text".into(),
                nullable: true,
                primary_key: false,
            },
        ];
        connection.databases.push(DatabaseInfo {
            name: "shop".into(),
            current: true,
            schemas: Some(vec![
                SchemaInfo {
                    name: "public".into(),
                    tables: vec![
                        TableInfo {
                            name: "customers".into(),
                            kind: ObjectKind::Table,
                            columns: columns.clone(),
                        },
                        TableInfo {
                            name: "orders".into(),
                            kind: ObjectKind::Table,
                            columns: columns.clone(),
                        },
                        TableInfo {
                            name: "active_customers".into(),
                            kind: ObjectKind::View,
                            columns: columns.clone(),
                        },
                    ],
                },
                SchemaInfo {
                    name: "audit".into(),
                    tables: vec![TableInfo {
                        name: "events".into(),
                        kind: ObjectKind::Table,
                        columns,
                    }],
                },
            ]),
        });
        connection
    }

    fn scope(kind: Option<ObjectKind>) -> ObjectScope {
        ObjectScope {
            conn: 7,
            db: "shop".into(),
            schema: Some("public".into()),
            kind,
        }
    }

    #[test]
    fn scope_filters_category_schema_and_search_without_changing_identity() {
        let connections = vec![connection()];
        let mut browser = ObjectBrowser::default();
        browser.open(scope(Some(ObjectKind::Table)));
        let entries = browser.entries(&connections);
        assert_eq!(
            entries
                .iter()
                .map(|e| e.reference.table.as_str())
                .collect::<Vec<_>>(),
            vec!["customers", "orders"]
        );
        assert!(
            entries
                .iter()
                .all(|e| e.reference.conn == 7 && e.reference.schema == "public")
        );
        browser.search = "ORDER".into();
        assert_eq!(browser.entries(&connections)[0].reference.table, "orders");
        browser.open(scope(Some(ObjectKind::View)));
        assert_eq!(
            browser.entries(&connections)[0].reference.table,
            "active_customers"
        );
        let mut all = scope(None);
        all.schema = None;
        browser.open(all);
        assert_eq!(browser.entries(&connections).len(), 4);
    }

    #[test]
    fn late_metadata_cannot_replace_a_new_selection_or_a_different_scope() {
        let mut browser = ObjectBrowser::default();
        browser.open(scope(None));
        let first = TableRef {
            conn: 7,
            db: "shop".into(),
            schema: "public".into(),
            table: "customers".into(),
        };
        let mut second = first.clone();
        second.table = "orders".into();
        let old = browser.start_request(first);
        let current = browser.start_request(second);
        browser.receive(7, old, Ok(ObjectInfo::default()));
        assert!(browser.info.is_none() && browser.loading);
        browser.receive(9, current, Ok(ObjectInfo::default()));
        assert!(browser.info.is_none());
        browser.receive(
            7,
            current,
            Ok(ObjectInfo {
                rows: Some(42),
                ..Default::default()
            }),
        );
        assert_eq!(
            browser.info.as_ref().unwrap().as_ref().unwrap().rows,
            Some(42)
        );
        browser.open(scope(Some(ObjectKind::View)));
        browser.receive(7, current, Ok(ObjectInfo::default()));
        assert!(browser.info.is_none() && browser.selected.is_none());
        let request = browser.start_catalog_request();
        browser.open(scope(Some(ObjectKind::Table)));
        browser.receive_catalog(7, request, Ok(Vec::new()));
        assert!(browser.catalog.is_empty());
        let current = browser.start_catalog_request();
        browser.receive_catalog(
            7,
            current,
            Ok(vec![crate::db::object_info::ObjectInfoRow {
                schema: "public".into(),
                name: "orders".into(),
                info: ObjectInfo {
                    rows: Some(7),
                    ..Default::default()
                },
            }]),
        );
        assert_eq!(
            browser
                .catalog
                .get(&("public".into(), "orders".into()))
                .unwrap()
                .rows,
            Some(7)
        );
    }

    struct State {
        browser: ObjectBrowser,
        connections: Vec<ConnState>,
        opened: Option<TableRef>,
        truncated: Option<TableRef>,
    }

    fn harness(layout: ObjectLayout, dark: bool) -> egui_kittest::Harness<'static, State> {
        let mut browser = ObjectBrowser::default();
        browser.open(scope(Some(ObjectKind::Table)));
        browser.layout = layout;
        let mut first = true;
        let mut h = egui_kittest::Harness::builder()
            .with_size(vec2(1180.0, 720.0))
            .build_ui_state(
                move |ui, state: &mut State| {
                    if std::mem::take(&mut first) {
                        theme::install_fonts(ui.ctx());
                        theme::apply(ui.ctx(), dark);
                        return;
                    }
                    egui::Panel::bottom("fixture_status_bar").exact_size(28.0).show(ui, |ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            state.browser.status_controls(ui, true);
                            ui.label("42 rows · 8 ms");
                        });
                    });
                    let mut actions = state.browser.show_inspector(ui, &state.connections);
                    actions.extend(state.browser.show(ui, &state.connections));
                    for action in actions {
                        match action {
                            BrowserAction::Inspect { table, count_rows } => {
                                let request = state.browser.start_request(table);
                                state.browser.receive(
                                    7,
                                    request,
                                    Ok(ObjectInfo {
                                        rows: Some(42),
                                        rows_exact: count_rows,
                                        data_bytes: Some(16384),
                                        index_bytes: Some(8192),
                                        created: None,
                                        modified: None,
                                        engine: None,
                                        comment: Some("Customer records".into()),
                                        ..Default::default()
                                    }),
                                );
                            }
                            BrowserAction::Explorer(ExplorerAction::OpenTable(table)) => {
                                state.opened = Some(table)
                            }
                            BrowserAction::Explorer(ExplorerAction::TruncateTable(table)) => {
                                state.truncated = Some(table)
                            }
                            _ => {}
                        }
                    }
                },
                State {
                    browser,
                    connections: vec![connection()],
                    opened: None,
                    truncated: None,
                },
            );
        h.run_steps(3);
        h
    }

    fn routine_harness(layout: ObjectLayout) -> egui_kittest::Harness<'static, State> {
        use crate::db::routines::{RoutineInfo, RoutineList};
        let mut h = harness(layout, true);
        h.state_mut().connections[0].routines.insert("shop".into(), RoutineList {
            items: Some(vec![
                RoutineInfo { schema: "public".into(), name: "archive".into(), kind: RoutineKind::Procedure, ..Default::default() },
                RoutineInfo { schema: "public".into(), name: "tax".into(), kind: RoutineKind::Function, ..Default::default() },
                RoutineInfo { schema: "other".into(), name: "other_proc".into(), kind: RoutineKind::Procedure, ..Default::default() },
            ]), ..Default::default()
        });
        h.state_mut().browser.open_routines(scope(None), RoutineKind::Procedure);
        h.run_steps(3);
        h
    }

    #[test]
    fn routine_scope_displays_only_matching_routines() {
        for layout in [ObjectLayout::List, ObjectLayout::Grid] {
        let mut h = routine_harness(layout);
        assert!(h.query_by_label("archive()").is_some());
        assert!(h.query_by_label("tax()").is_none());
        assert!(h.query_by_label("other_proc()").is_none());
        h.get_by_label("archive()").click(); h.run_steps(2);
        assert_eq!(h.state().browser.selected_routine.as_ref().unwrap().name, "archive");
        assert!(h.query_by_label("Edit Procedure…").is_some());
        h.state_mut().browser.open_routines(scope(None), RoutineKind::Function);
        h.run_steps(3);
        assert!(h.query_by_label("tax()").is_some());
        assert!(h.query_by_label("archive()").is_none());
        }
    }

    #[test]
    #[ignore = "Requires a GPU; renders routine list/grid and their icons using fixture data"]
    fn render_routine_object_views() {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/routine-snapshots");
        std::fs::create_dir_all(&directory).unwrap();
        for (layout, name) in [(ObjectLayout::List, "routine-list"), (ObjectLayout::Grid, "routine-grid")] {
            let mut h = routine_harness(layout);
            let list = h.state_mut().connections[0].routines.get_mut("shop").unwrap();
            let items = list.items.as_mut().unwrap();
            for name in ["archive_orders", "rebuild_indexes", "refresh_statistics", "sync_inventory", "update_prices", "validate_orders"] {
                items.push(crate::db::routines::RoutineInfo { schema: "public".into(), name: name.into(), arguments: "before_date timestamp".into(), kind: RoutineKind::Procedure, valid: true, ..Default::default() });
            }
            for name in ["calculate_discount", "get_order_total", "get_stock_level", "normalize_address"] {
                items.push(crate::db::routines::RoutineInfo { schema: "public".into(), name: name.into(), arguments: "id integer".into(), returns: "numeric".into(), kind: RoutineKind::Function, valid: true, ..Default::default() });
            }
            for (kind, suffix) in [(RoutineKind::Procedure, "procedures"), (RoutineKind::Function, "functions")] {
                h.state_mut().browser.open_routines(scope(None), kind); h.run_steps(3);
                h.render().unwrap().save(directory.join(format!("{name}-{suffix}.png"))).unwrap();
            }
        }
    }

    #[test]
    fn list_and_grid_selection_show_metadata_and_open_the_selected_object() {
        for layout in [ObjectLayout::List, ObjectLayout::Grid] {
            let mut h = harness(layout, false);
            h.get_by_label("customers").click();
            h.run_steps(3);
            assert_eq!(
                h.state().browser.selected.as_ref().unwrap().table,
                "customers"
            );
            assert!(h.query_all_by_label("≈ 42").next().is_some());
            h.get_by_label("Actions").click(); h.run_steps(2);
            h.get_by_label("Count rows exactly").click();
            h.run_steps(3);
            assert!(
                h.state()
                    .browser
                    .info
                    .as_ref()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .rows_exact
            );
            h.get_by_label("Actions").click(); h.run_steps(2);
            h.get_by_label("Open data").click();
            h.run_steps(2);
            assert_eq!(h.state().opened.as_ref().unwrap().table, "customers");
        }
    }

    #[test]
    fn inspector_tabs_separate_columns_from_metadata_and_layout_icons_stay_at_the_bottom() {
        let mut h = harness(ObjectLayout::Grid, true);
        assert!(h.get_by_label("List").rect().top() > 670.0);
        assert!(h.get_by_label("Grid").rect().width() <= 26.0);
        assert!(h.get_by_label("Search objects…").rect().top() > 670.0);
        assert!(h.get_by_label("Information").rect().top() < 35.0);
        h.key_press_modifiers(egui::Modifiers::COMMAND, Key::F); h.run_steps(2);
        h.input_mut().events.push(egui::Event::Text("orders".into())); h.run_steps(3);
        assert_eq!(h.state().browser.search, "orders");
        h.state_mut().browser.search.clear(); h.run_steps(2);
        h.get_by_label("List").click(); h.run_steps(2);
        assert_eq!(h.state().browser.layout, ObjectLayout::List);
        h.get_by_label("customers").click(); h.run_steps(3);
        assert!(h.query_by_label("Open data").is_none());
        h.get_by_label("Columns").click(); h.run_steps(2);
        assert_eq!(h.state().browser.inspector_mode, crate::ui::inspector::InspectorMode::Columns);
        assert!(h.query_by_label("integer · NOT NULL · PK").is_some());
        h.get_by_label("Information").click(); h.run_steps(2);
        assert!(h.query_by_label("integer · NOT NULL · PK").is_none());
    }

    #[test]
    fn compact_grid_shows_only_object_names_while_list_has_statistics_columns() {
        let grid = harness(ObjectLayout::Grid, false);
        assert!(grid.query_by_label("customers").is_some());
        assert!(grid.query_by_label("Rows").is_none());
        assert_eq!(grid.query_all_by_label("public").count(), 1, "schema belongs to the database inspector, not the grid entries");
        let list = harness(ObjectLayout::List, false);
        assert!(list.query_by_label("Rows").is_some());
        assert!(list.query_by_label("Data length").is_some());
        assert!(list.query_by_label("Engine").is_some());
    }

    #[test]
    fn empty_space_menu_opens_schema_search_and_object_menu_keeps_organization_actions() {
        let mut h = harness(ObjectLayout::Grid, true);
        let position = pos2(500.0, 500.0);
        h.event(egui::Event::PointerMoved(position));
        h.run_steps(2);
        for pressed in [true, false] {
            h.event(egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Secondary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
            h.run_steps(2);
        }
        h.get_by_label("Find in Schema…").click();
        h.run_steps(3);
        assert!(h.state().browser.schema_search.open);
        h.get_by_label("Close").click();
        h.run_steps(3);
        h.get_by_label("customers").click_secondary();
        h.run_steps(3);
        assert!(h.query_by_label("Add to group… ⏵").is_some());
        assert!(h.query_by_label("Color ⏵").is_some());
        assert!(h.query_all_by_label("Open data").next().is_some());
    }

    fn create_group(h: &mut egui_kittest::Harness<'_, State>, name: &str) {
        let position = pos2(500.0, 500.0);
        h.event(egui::Event::PointerMoved(position));
        h.run_steps(2);
        for pressed in [true, false] {
            h.event(egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Secondary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
            h.run_steps(2);
        }
        h.get_by_label("New group…").click();
        h.run_steps(3);
        h.input_mut().events.push(egui::Event::Text(name.into()));
        h.run_steps(2);
        h.get_by_label("Save").click();
        h.run_steps(3);
        assert!(
            h.state().browser.organizations[0]
                .groups
                .contains(&name.to_string())
        );
    }

    fn color_item(h: &mut egui_kittest::Harness<'_, State>, label: &str, color: &str) {
        h.get_all_by_label(label).last().unwrap().click_secondary();
        h.run_steps(3);
        h.get_by_label("Color ⏵").click();
        h.run_steps(2);
        h.get_by_label(color).click();
        h.run_steps(3);
    }

    fn drag_selected_to_group(
        h: &mut egui_kittest::Harness<'_, State>,
        source: &str,
        group: &str,
        render: bool,
    ) {
        let source = h.get_all_by_label(source).last().unwrap().rect().center();
        let target = h.get_by_label(group).rect().center();
        h.event(egui::Event::PointerMoved(source));
        h.run_steps(2);
        h.event(egui::Event::PointerButton {
            pos: source,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        h.run_steps(2);
        h.event(egui::Event::PointerMoved(source + vec2(16.0, 0.0)));
        h.run_steps(2);
        h.event(egui::Event::PointerMoved(target));
        h.run_steps(3);
        assert_eq!(
            egui::DragAndDrop::payload::<ObjectDrag>(&h.ctx)
                .unwrap()
                .tables
                .len(),
            2
        );
        if render {
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target/object-browser-snapshots");
            std::fs::create_dir_all(&dir).unwrap();
            h.render()
                .unwrap()
                .save(dir.join("group-drag.png"))
                .unwrap();
        }
        h.event(egui::Event::PointerButton {
            pos: target,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        h.run_steps(3);
    }

    #[test]
    fn inline_folders_accept_multiple_tables_by_drag_and_drop_in_both_layouts() {
        for layout in [ObjectLayout::Grid, ObjectLayout::List] {
            let mut h = harness(layout, true);
            assert!(h.query_by_label("Groups").is_none());
            create_group(&mut h, "Sales");
            h.get_by_label("customers").click();
            h.run_steps(2);
            h.get_by_label("orders")
                .click_modifiers(egui::Modifiers::COMMAND);
            h.run_steps(2);
            assert_eq!(h.state().browser.selected_tables().len(), 2);
            drag_selected_to_group(&mut h, "customers", "Sales", false);
            for name in ["customers", "orders"] {
                assert_eq!(
                    h.state().browser.organizations[0].group("public", name),
                    Some("Sales")
                );
                assert!(h.query_by_label(name).is_none());
            }
            h.get_by_label("Sales").click();
            h.run_steps(2);
            h.key_press(Key::Enter);
            h.run_steps(3);
            assert!(h.query_by_label("customers").is_some());
            assert!(h.query_by_label("orders").is_some());
            assert!(h.query_by_label("All objects").is_some());
        }
    }

    #[test]
    fn shift_and_select_all_cover_tables_and_context_colors_apply_to_the_selection() {
        let mut h = harness(ObjectLayout::Grid, true);
        h.get_by_label("customers").click();
        h.run_steps(2);
        h.get_by_label("orders")
            .click_modifiers(egui::Modifiers::SHIFT);
        h.run_steps(2);
        assert_eq!(h.state().browser.selected_tables().len(), 2);
        h.get_all_by_label("orders")
            .last()
            .unwrap()
            .click_secondary();
        h.run_steps(2);
        h.get_by_label("Color ⏵").click();
        h.run_steps(2);
        h.get_by_label("Blue").click();
        h.run_steps(3);
        for name in ["customers", "orders"] {
            assert!(
                h.state().browser.organizations[0]
                    .color("public", name)
                    .is_some()
            );
        }
        h.get_by_label("customers").click();
        h.run_steps(2);
        h.key_press_modifiers(egui::Modifiers::COMMAND, Key::A);
        h.run_steps(2);
        assert_eq!(h.state().browser.selected_tables().len(), 2);
    }

    #[test]
    fn groups_expose_the_same_color_menu_as_tables() {
        for layout in [ObjectLayout::List, ObjectLayout::Grid] {
            let mut h = harness(layout, true);
            create_group(&mut h, "Sales");
            h.get_by_label("Sales").click_secondary();
            h.run_steps(3);
            assert!(h.query_by_label("Color ⏵").is_some());
            h.get_by_label("Color ⏵").click();
            h.run_steps(2);
            h.get_by_label("Red").click();
            h.run_steps(3);
            assert_eq!(
                h.state().browser.organizations[0].group_color("Sales"),
                Some(egui::Color32::from_rgb(210, 85, 90))
            );
            let saved = serde_json::to_string(&h.state().browser.organizations[0]).unwrap();
            let restored: Organization = serde_json::from_str(&saved).unwrap();
            assert_eq!(
                restored.group_color("Sales"),
                Some(egui::Color32::from_rgb(210, 85, 90))
            );
            h.get_by_label("customers")
                .click_modifiers(egui::Modifiers::COMMAND);
            h.run_steps(2);
            h.get_all_by_label("customers")
                .last()
                .unwrap()
                .click_secondary();
            h.run_steps(2);
            h.get_by_label("Color ⏵").click();
            h.run_steps(2);
            h.get_by_label("Green").click();
            h.run_steps(3);
            let green = Some(egui::Color32::from_rgb(65, 163, 112));
            assert_eq!(
                h.state().browser.organizations[0].group_color("Sales"),
                green
            );
            assert_eq!(
                h.state().browser.organizations[0].color("public", "customers"),
                green
            );
            h.get_by_label("Sales").click_secondary();
            h.run_steps(2);
            h.get_by_label("Color ⏵").click();
            h.run_steps(2);
            h.get_by_label("Default").click();
            h.run_steps(3);
            assert!(
                h.state().browser.organizations[0]
                    .group_color("Sales")
                    .is_none()
            );
            assert!(
                h.state().browser.organizations[0]
                    .color("public", "customers")
                    .is_none()
            );
        }
    }

    #[test]
    fn table_context_menu_exposes_complete_workflows_in_list_and_grid() {
        for layout in [ObjectLayout::List, ObjectLayout::Grid] {
            let mut h = harness(layout, true);
            h.get_by_label("customers").click_secondary();
            h.run_steps(2);
            for label in ["New Table…", "Import Data from File…", "Rename…", "Empty Table…",
                "Duplicate Table ⏵", "Set Privileges…", "Export Table…", "Dump SQL File ⏵",
                "Print Table…", "Reverse Tables to Model…", "Create Open Table Shortcut…",
                "Refresh", "Object Information", "Drop…"] {
                assert!(h.query_all_by_label(label).next().is_some(), "missing {label}");
            }
        }
    }

    #[test]
    fn truncate_is_available_only_for_one_writable_table_and_uses_the_existing_action() {
        let mut h = harness(ObjectLayout::Grid, true);
        h.get_by_label("customers").click_secondary();
        h.run_steps(2);
        h.get_by_label("Truncate…").click();
        h.run_steps(2);
        assert_eq!(h.state().truncated.as_ref().unwrap().table, "customers");
        h.state_mut().connections[0].cfg.read_only = true;
        h.get_by_label("orders").click_secondary();
        h.run_steps(2);
        assert!(h.get_by_label("Truncate…").accesskit_node().is_disabled());
        h.key_press(Key::Escape);
        h.run_steps(2);
        h.state_mut().connections[0].cfg.read_only = false;
        h.state_mut().browser.open(scope(Some(ObjectKind::View)));
        h.run_steps(3);
        h.get_by_label("active_customers").click_secondary();
        h.run_steps(2);
        assert!(h.get_by_label("Truncate…").accesskit_node().is_disabled());
    }

    #[test]
    fn dragging_a_selection_rectangle_from_empty_space_selects_multiple_objects() {
        let mut h = harness(ObjectLayout::Grid, true);
        let start = pos2(400.0, 250.0);
        let end = pos2(8.0, 60.0);
        h.event(egui::Event::PointerMoved(start));
        h.run_steps(2);
        h.event(egui::Event::PointerButton {
            pos: start,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        h.run_steps(2);
        h.event(egui::Event::PointerMoved(start - vec2(15.0, 15.0)));
        h.run_steps(2);
        h.event(egui::Event::PointerMoved(end));
        h.run_steps(3);
        h.event(egui::Event::PointerButton {
            pos: end,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        h.run_steps(3);
        assert_eq!(h.state().browser.selected_tables().len(), 2);
        assert!(h.state().browser.marquee.is_none());
    }

    #[test]
    #[ignore = "Requires a GPU; renders dragging a multi-selection onto a local folder"]
    fn render_group_drag() {
        let mut h = harness(ObjectLayout::Grid, true);
        create_group(&mut h, "Sales");
        color_item(&mut h, "Sales", "Purple");
        color_item(&mut h, "customers", "Blue");
        color_item(&mut h, "orders", "Green");
        h.get_all_by_label("customers").last().unwrap().click();
        h.run_steps(2);
        h.get_by_label("orders")
            .click_modifiers(egui::Modifiers::COMMAND);
        h.run_steps(2);
        drag_selected_to_group(&mut h, "customers", "Sales", true);
        h.get_by_label("Sales").click();
        h.run_steps(2);
        h.key_press(Key::Enter);
        h.run_steps(3);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/object-browser-snapshots");
        h.render()
            .unwrap()
            .save(dir.join("group-contents.png"))
            .unwrap();
    }

    #[test]
    fn grid_navigation_fills_down_then_moves_to_the_next_column() {
        assert_eq!(object_navigation(Some(0), Key::ArrowDown, 70, 30), 1);
        assert_eq!(object_navigation(Some(29), Key::ArrowDown, 70, 30), 29);
        assert_eq!(object_navigation(Some(0), Key::ArrowRight, 70, 30), 30);
        assert_eq!(object_navigation(Some(30), Key::ArrowLeft, 70, 30), 0);
        assert_eq!(object_navigation(Some(30), Key::ArrowUp, 70, 30), 30);
        assert_eq!(object_navigation(Some(69), Key::End, 70, 30), 69);
    }

    #[test]
    #[ignore = "Requires a GPU; writes object browser previews to target/object-browser-snapshots"]
    fn render_object_browser() {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/object-browser-snapshots");
        std::fs::create_dir_all(&directory).unwrap();
        for (layout, name) in [(ObjectLayout::List, "list"), (ObjectLayout::Grid, "grid")] {
            let mut h = harness(layout, true);
            h.set_size(vec2(1800.0, 1000.0));
            {
                let state = h.state_mut();
                state.browser.organizations[0] = serde_json::from_value(serde_json::json!({
                    "conn": 7, "db": "shop", "groups": ["Sales", "Archive"],
                    "objects": [
                        { "schema": "public", "table": "customers", "group": "Sales", "color": [81, 143, 221] },
                        { "schema": "public", "table": "orders", "group": "Sales", "color": [65, 163, 112] }
                    ]
                })).unwrap();
                let schema = &mut state.connections[0].databases[0].schemas.as_mut().unwrap()[0];
                let columns = schema.tables[0].columns.clone();
                for i in 1..=40 {
                    schema.tables.push(TableInfo {
                        name: format!("table_{i:02}"),
                        kind: ObjectKind::Table,
                        columns: columns.clone(),
                    });
                }
                for table in &schema.tables {
                    state.browser.catalog.insert(
                        (schema.name.clone(), table.name.clone()),
                        ObjectInfo {
                            rows: Some(42),
                            data_bytes: Some(16384),
                            engine: Some("InnoDB".into()),
                            created: Some("2026-01-01 12:00:00".into()),
                            collation: Some("utf8mb4_unicode_ci".into()),
                            ..Default::default()
                        },
                    );
                }
            }
            h.run_steps(3);
            if layout == ObjectLayout::Grid {
                h.render().unwrap().save(directory.join("database-overview.png")).unwrap();
            }
            for (label, color) in [
                ("Sales", "Purple"),
                ("Archive", "Orange"),
                ("table_01", "Red"),
                ("table_02", "Green"),
                ("table_03", "Blue"),
                ("table_04", "Pink"),
            ] {
                color_item(&mut h, label, color);
            }
            h.get_all_by_label("table_05").last().unwrap().click();
            h.run_steps(3);
            h.render()
                .unwrap()
                .save(directory.join(format!("{name}.png")))
                .unwrap();
            h.get_by_label("Columns").click(); h.run_steps(3);
            h.render().unwrap().save(directory.join(format!("{name}-columns.png"))).unwrap();
            h.get_by_label("Information").click(); h.run_steps(3);
            if layout == ObjectLayout::Grid {
                h.set_size(vec2(1180.0, 720.0));
                h.run_steps(3);
                h.get_by_label("Sales").click_secondary();
                h.run_steps(3);
                h.get_by_label("Color ⏵").click();
                h.run_steps(3);
                h.render()
                    .unwrap()
                    .save(directory.join("group-color-menu.png"))
                    .unwrap();
            }
        }
    }
}
