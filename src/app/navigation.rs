//! Stable-id tab switching, cross-editor caret history and database reference navigation.
use super::*;
use crate::sql::navigation as source;
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
#[derive(Clone, Debug, PartialEq, Eq)]
enum Definition {
    Table(TableRef, Option<String>),
    Routine(crate::db::routines::RoutineRef),
    Schema(ConnId, String, String),
}

impl Definition {
    fn label(&self) -> String {
        match self {
            Self::Table(t, col) => format!(
                "{}.{}.{}{}",
                t.db,
                t.schema,
                t.table,
                col.as_ref().map(|s| format!(".{s}")).unwrap_or_default()
            ),
            Self::Routine(r) => format!("{}.{}.{}({})", r.db, r.schema, r.name, r.signature),
            Self::Schema(_, db, schema) => format!("{db}.{schema}"),
        }
    }
}
struct Definitions {
    items: Vec<Definition>,
    ddl: bool,
}

#[derive(Default)]
pub(super) struct State {
    pub(super) settings: console::navigation::Settings,
    pub(super) reveal_tab: Option<u64>,
    recent: Vec<Target>,
    switcher: Option<Switcher>,
    trail: Vec<Location>,
    at: usize,
    edits: Vec<Location>,
    edit_at: Option<usize>,
    texts: HashMap<u64, u64>,
    last_location: Option<Location>,
    replaying: bool,
    definitions: Option<Definitions>,
    pending_reference: Option<(u64, usize, bool, String)>,
    bookmarks: bool,
    bookmark_sort_name: bool,
}
impl State {
    pub fn new(settings: console::navigation::Settings) -> Self {
        Self {
            settings,
            ..Default::default()
        }
    }
    pub fn is_open(&self) -> bool {
        self.switcher.is_some() || self.definitions.is_some() || self.bookmarks
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

    fn record_navigation_jump(&mut self, from: Location, to: Location) {
        if from == to {
            return;
        }
        if self.navigation.trail.is_empty() {
            self.navigation.trail.push(from);
            self.navigation.at = 0;
        } else if self.navigation.trail[self.navigation.at].target == from.target {
            self.navigation.trail[self.navigation.at] = from;
        }
        self.navigation.trail.truncate(self.navigation.at + 1);
        self.navigation.trail.push(to);
        if self.navigation.trail.len() > 300 {
            self.navigation.trail.remove(0);
        }
        self.navigation.at = self.navigation.trail.len() - 1;
    }

    pub(super) fn observe_navigation(&mut self) {
        let ids: HashSet<_> = self.consoles.iter().map(|c| c.id).collect();
        self.navigation
            .recent
            .retain(|t| !matches!(t, Target::Tab(id) if !ids.contains(id)));
        self.navigation.texts.retain(|id, _| ids.contains(id));
        if self
            .navigation
            .reveal_tab
            .is_some_and(|id| !ids.contains(&id))
        {
            self.navigation.reveal_tab = None;
        }
        let current = self.navigation_location();
        let jump = if !self.object_browser.active {
            self.consoles
                .get_mut(self.active)
                .and_then(|c| c.navigation.jump.take())
        } else {
            None
        };
        if let Some(loc) = current {
            if self.navigation.trail.is_empty() {
                self.navigation.trail.push(loc);
                self.navigation.at = 0;
            }
            if !self.navigation.replaying {
                if let Some((from, to)) = jump {
                    self.record_navigation_jump(
                        Location {
                            target: loc.target,
                            position: from,
                        },
                        Location {
                            target: loc.target,
                            position: to,
                        },
                    );
                } else if let Some(previous) = self
                    .navigation
                    .last_location
                    .filter(|p| p.target != loc.target)
                {
                    self.record_navigation_jump(previous, loc);
                }
            }
            if self.navigation.recent.first() != Some(&loc.target) {
                if let Target::Tab(id) = loc.target {
                    self.navigation.reveal_tab = Some(id);
                }
                self.navigation.recent.retain(|t| *t != loc.target);
                self.navigation.recent.insert(0, loc.target);
            }
            self.navigation.last_location = Some(loc);
        }
        // The console updates its revision on edits. Inactive SQL is never
        // scanned merely because the caret blinks in another editor.
        for c in &self.consoles {
            if self
                .navigation
                .texts
                .insert(c.id, c.navigation.revision())
                .is_some_and(|old| old != c.navigation.revision())
            {
                let loc = Location {
                    target: Target::Tab(c.id),
                    position: c.cursor.map_or(0, |(_, b)| b),
                };
                if self.navigation.edits.last() != Some(&loc) {
                    self.navigation.edits.push(loc);
                }
                if self.navigation.edits.len() > 100 {
                    self.navigation.edits.remove(0);
                }
                self.navigation.edit_at = None;
            }
        }
    }

    fn activate_navigation(
        &mut self,
        ctx: &egui::Context,
        target: Target,
        position: Option<usize>,
    ) {
        self.observe_navigation();
        let before = self.navigation_location();
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
        let after = self.navigation_location();
        if !self.navigation.replaying
            && let (Some(from), Some(to)) = (before, after)
        {
            self.record_navigation_jump(from, to);
        }
        self.navigation.last_location = after;
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
        if !self.modal_open()
            && !self.object_browser.active
            && let Some(c) = self
                .consoles
                .get(self.active)
                .filter(|c| c.table.is_none() && ctx.memory(|m| m.has_focus(c.editor_id())))
        {
            if let Some(command) = ctx
                .input_mut(|i| console::navigation::take_binding(i, &c.navigation.saved.bindings))
            {
                self.navigation_command(ctx, command);
                return true;
            }
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
        let mut command = None;
        ctx.input_mut(|i| {
            for (key, cmd) in [
                (Key::ArrowLeft, Command::Back),
                (Key::ArrowRight, Command::Forward),
            ] {
                if console::take_key(i, Modifiers::COMMAND | Modifiers::ALT, key) {
                    command = Some(cmd);
                }
            }
            if console::take_key(i, Modifiers::COMMAND | Modifiers::SHIFT, Key::Backspace) {
                command = Some(Command::LastEdit);
            }
            if console::take_key(
                i,
                if cfg!(target_os = "macos") {
                    Modifiers::COMMAND | Modifiers::ALT | Modifiers::SHIFT
                } else {
                    Modifiers::SHIFT
                },
                if cfg!(target_os = "macos") {
                    Key::K
                } else {
                    Key::F11
                },
            ) {
                command = Some(Command::Bookmarks);
            }
        });
        if let Some(cmd) = command {
            self.navigation_command(ctx, cmd);
            return true;
        }
        // Numbered bookmarks use physical Control to leave Command+0 zoom intact on macOS.
        let keys = [
            Key::Num0,
            Key::Num1,
            Key::Num2,
            Key::Num3,
            Key::Num4,
            Key::Num5,
            Key::Num6,
            Key::Num7,
            Key::Num8,
            Key::Num9,
        ];
        let mut bookmark = None;
        for (n, key) in keys.into_iter().enumerate() {
            let digit = n.to_string();
            if let Some((id, p)) = self.consoles.iter().find_map(|c| {
                c.navigation
                    .saved
                    .bookmarks
                    .iter()
                    .find(|b| b.mnemonic == digit)
                    .map(|b| (c.id, b.position))
            }) {
                let pressed=ctx.input_mut(|i| {
                    let mut yes=false; i.events.retain(|e| { if matches!(e,egui::Event::Key{key:k,pressed:true,modifiers:m,..} if *k==key && m.ctrl && !m.alt && m.shift && !m.mac_cmd) {yes=true;false} else {true} }); yes
                });
                if pressed {
                    bookmark = Some((id, p));
                }
            }
        }
        if let Some((id, p)) = bookmark {
            self.activate_navigation(ctx, Target::Tab(id), Some(p));
            return true;
        }
        if self.explorer.focused
            && !ctx.memory(|m| {
                self.consoles
                    .get(self.active)
                    .is_some_and(|c| m.has_focus(c.editor_id()))
            })
            && ctx.input_mut(|i| console::take_key(i, Modifiers::COMMAND, Key::B))
        {
            if let Some(t) = self.explorer.selected_table.clone() {
                self.generate(ctx, &t, Generate::Ddl);
            } else if let Some(r) = self
                .conns
                .iter()
                .flat_map(|c| {
                    c.routines.iter().flat_map(move |(db, list)| {
                        list.items
                            .iter()
                            .flatten()
                            .map(move |r| crate::db::routines::RoutineRef::new(c.cfg.id, db, r))
                    })
                })
                .find(|r| self.explorer.selected.as_deref() == Some(&explorer::routine_key(r)))
            {
                self.request_routine_ddl(r);
            }
            return true;
        }
        false
    }

    pub(super) fn navigation_command(&mut self, ctx: &egui::Context, cmd: Command) {
        self.observe_navigation();
        match cmd {
            Command::Switcher => self.open_switcher(false, false),
            Command::Bookmarks => self.navigation.bookmarks = true,
            Command::Back | Command::Forward => {
                let forward = cmd == Command::Forward;
                let mut at = self.navigation.at;
                loop {
                    if forward {
                        if at + 1 >= self.navigation.trail.len() {
                            break;
                        }
                        at += 1;
                    } else {
                        if at == 0 {
                            break;
                        }
                        at -= 1;
                    }
                    let loc = self.navigation.trail[at];
                    if matches!(loc.target,Target::Tab(id) if !self.consoles.iter().any(|c| c.id==id))
                    {
                        continue;
                    }
                    self.navigation.at = at;
                    self.navigation.replaying = true;
                    self.activate_navigation(ctx, loc.target, Some(loc.position));
                    self.navigation.replaying = false;
                    break;
                }
            }
            Command::LastEdit => {
                let end = self
                    .navigation
                    .edit_at
                    .unwrap_or(self.navigation.edits.len());
                if let Some((at,loc))=self.navigation.edits[..end].iter().enumerate().rev().find(|(_,l)| Some(**l)!=self.navigation_location() && matches!(l.target,Target::Tab(id) if self.consoles.iter().any(|c| c.id==id))).map(|(i,l)|(i,*l)) {
                    self.navigation.edit_at=Some(at); self.activate_navigation(ctx,loc.target,Some(loc.position));
                }
            }
            Command::Explorer | Command::Declaration => {
                if let Some(c) = self
                    .consoles
                    .get(self.active)
                    .filter(|_| !self.object_browser.active)
                {
                    self.navigate_reference(
                        ctx,
                        c.id,
                        c.cursor.map_or(0, |(_, b)| b),
                        cmd == Command::Declaration,
                    );
                }
            }
            _ => {
                if let Some(c) = self
                    .consoles
                    .get_mut(self.active)
                    .filter(|c| c.table.is_none() && !self.object_browser.active)
                {
                    c.navigation.pending.push(cmd);
                }
            }
        }
    }

    fn navigate_reference(&mut self, ctx: &egui::Context, tab: u64, position: usize, ddl: bool) {
        let Some(c) = self.consoles.iter().find(|c| c.id == tab) else {
            return;
        };
        let driver = c
            .conn
            .and_then(|id| self.conn(id))
            .map_or(Driver::Postgres, |c| c.cfg.driver);
        let Some(r) = source::reference_at(&c.text, position, driver) else {
            self.error(tr("Place the caret on a database object name"));
            return;
        };
        let stmt = sql::statement_at(&c.text, crate::ui::char_to_byte(&c.text, position), driver);
        let stmt_start = stmt
            .as_ref()
            .map_or(0, |s| source::character(&c.text, s.start));
        let uses = source::table_uses(stmt.as_ref().map_or(&c.text, |s| s.text.as_str()), driver);
        if ddl {
            let alias = uses
                .iter()
                .find(|u| {
                    r.component == 0
                        && r.range.start != stmt_start + u.declaration
                        && u.alias
                            .as_ref()
                            .is_some_and(|a| a.matches(&r.parts[0].name))
                })
                .map(|u| stmt_start + u.declaration);
            let referenced = if r.parts.len() > 1 {
                &r.parts[0]
            } else {
                r.parts.last().unwrap()
            };
            let local = alias.or_else(|| {
                source::cte_declarations(stmt.as_ref().map_or(&c.text, |s| s.text.as_str()), driver)
                    .into_iter()
                    .find(|(name, _)| name.matches(&referenced.name))
                    .map(|(_, p)| stmt_start + p)
            });
            if let Some(p) = local {
                self.activate_navigation(ctx, Target::Tab(tab), Some(p));
                return;
            }
        }
        let Some(conn) = c.conn.and_then(|id| self.conn(id)) else {
            self.error(tr("Attach a connection to navigate database references"));
            return;
        };
        let Some(db) = conn.resolve_db(c.db.as_deref()).map(str::to_string) else {
            self.error(tr("Connect to load database definitions"));
            return;
        };
        let Some(schemas) = conn.schemas(Some(&db)) else {
            let id = conn.cfg.id;
            if conn.status == ConnStatus::Connected {
                self.navigation.pending_reference = Some((tab, position, ddl, c.text.clone()));
                self.load_database(id, db);
            } else {
                self.error(tr("Connect to load database definitions"));
            }
            return;
        };
        let mut items = Vec::new();
        let parts = &r.parts;
        let last = parts.last().unwrap();
        debug_assert!(r.range.start <= position && position <= r.range.end);
        let matches_table = |names: &[source::Identifier], schema: &str, table: &str| {
            names.last().is_some_and(|n| n.matches(table))
                && (names.len() < 2 || names[names.len() - 2].matches(schema))
                && (names.len() < 3 || names[names.len() - 3].matches(&db))
        };
        if items.is_empty() {
            for schema in schemas {
                if r.component == 0 && parts.len() > 1 && parts[0].matches(&schema.name) && !ddl {
                    items.push(Definition::Schema(
                        conn.cfg.id,
                        db.clone(),
                        schema.name.clone(),
                    ));
                    continue;
                }
                for table in &schema.tables {
                    let target = TableRef {
                        conn: conn.cfg.id,
                        db: db.clone(),
                        schema: schema.name.clone(),
                        table: table.name.clone(),
                    };
                    if matches_table(parts, &schema.name, &table.name) {
                        items.push(Definition::Table(target.clone(), None));
                    }
                    for col in &table.columns {
                        if !last.matches(&col.name) {
                            continue;
                        }
                        let binding = uses.iter().any(|u| {
                            matches_table(&u.parts, &schema.name, &table.name)
                                && (parts.len() == 1
                                    || u.alias
                                        .as_ref()
                                        .is_some_and(|a| a.matches(&parts[parts.len() - 2].name))
                                    || matches_table(
                                        &parts[..parts.len() - 1],
                                        &schema.name,
                                        &table.name,
                                    ))
                        });
                        if binding
                            || (parts.len() > 1
                                && matches_table(
                                    &parts[..parts.len() - 1],
                                    &schema.name,
                                    &table.name,
                                ))
                        {
                            let column = (r.component + 1 == parts.len()).then(|| col.name.clone());
                            items.push(Definition::Table(target.clone(), column));
                        }
                    }
                }
            }
            if let Some(list) = conn.routines.get(&db).and_then(|r| r.items.as_ref()) {
                for routine in list {
                    if last.matches(&routine.name)
                        && (parts.len() < 2 || parts[parts.len() - 2].matches(&routine.schema))
                    {
                        items.push(Definition::Routine(crate::db::routines::RoutineRef::new(
                            conn.cfg.id,
                            &db,
                            routine,
                        )));
                    }
                }
            }
        }
        let mut unique = Vec::new();
        for item in items {
            if !unique.contains(&item) {
                unique.push(item);
            }
        }
        match unique.len() {
            0 => self.error(tr(
                "No matching object in loaded metadata. Refresh the database and try again.",
            )),
            1 => self.open_definition(ctx, unique.remove(0), ddl),
            _ => self.navigation.definitions = Some(Definitions { items: unique, ddl }),
        }
    }

    fn open_definition(&mut self, ctx: &egui::Context, definition: Definition, ddl: bool) {
        match definition {
            Definition::Table(t, col) => {
                if ddl {
                    self.generate(ctx, &t, Generate::Ddl);
                    return;
                }
                self.show_explorer = true;
                self.explorer.filter.clear();
                self.explorer.focused = true;
                for key in [
                    format!("c:{}", t.conn),
                    explorer::db_key(t.conn, &t.db),
                    format!("s:{}:{}:{}", t.conn, t.db, t.schema),
                ] {
                    self.explorer.expanded.insert(key);
                }
                let kind = self
                    .table_info(t.conn, Some(&t.db), &t.schema, &t.table)
                    .map(|x| x.kind);
                self.explorer.expanded.insert(format!(
                    "g:{}:{}:{}:{}",
                    t.conn,
                    t.db,
                    t.schema,
                    if kind == Some(crate::db::ObjectKind::View) {
                        "v"
                    } else {
                        "t"
                    }
                ));
                let key = explorer::table_key(&t);
                self.explorer.selected_table = Some(t.clone());
                let selected = if let Some(col) = col {
                    self.explorer.expanded.insert(key.clone());
                    self.explorer.collapsed.remove(&explorer::columns_key(&t));
                    format!("{key}:{col}")
                } else {
                    key
                };
                self.explorer.selected = Some(selected.clone());
                self.explorer.reveal = Some(selected);
                ctx.memory_mut(|m| {
                    if let Some(id) = m.focused() {
                        m.surrender_focus(id);
                    }
                });
            }
            Definition::Routine(r) => {
                if ddl {
                    self.request_routine_ddl(r);
                    return;
                }
                self.show_explorer = true;
                self.explorer.filter.clear();
                for key in [
                    format!("c:{}", r.conn),
                    explorer::db_key(r.conn, &r.db),
                    format!("s:{}:{}:{}", r.conn, r.db, r.schema),
                    format!(
                        "g:{}:{}:{}:{}",
                        r.conn,
                        r.db,
                        r.schema,
                        if r.kind == crate::db::routines::RoutineKind::Function {
                            "f"
                        } else {
                            "p"
                        }
                    ),
                ] {
                    self.explorer.expanded.insert(key);
                }
                let key = explorer::routine_key(&r);
                self.explorer.selected = Some(key.clone());
                self.explorer.reveal = Some(key);
                self.explorer.selected_table = None;
            }
            Definition::Schema(conn, db, schema) => {
                self.show_explorer = true;
                self.explorer.filter.clear();
                self.explorer.expanded.insert(format!("c:{conn}"));
                self.explorer.expanded.insert(explorer::db_key(conn, &db));
                let key = format!("s:{conn}:{db}:{schema}");
                self.explorer.selected = Some(key.clone());
                self.explorer.reveal = Some(key);
            }
        }
    }

    pub(super) fn navigation_ui(&mut self, ctx: &egui::Context) {
        if self.navigation.bookmarks {
            let drivers: HashMap<_, _> = self
                .conns
                .iter()
                .map(|c| (c.cfg.id, c.cfg.driver))
                .collect();
            for c in self
                .consoles
                .iter_mut()
                .filter(|c| !c.navigation.saved.bookmarks.is_empty())
            {
                let driver = c
                    .conn
                    .and_then(|id| drivers.get(&id))
                    .copied()
                    .unwrap_or(Driver::Postgres);
                c.navigation.prepare_metadata(&c.text, driver);
            }

            let mut rows: Vec<_> = self
                .consoles
                .iter()
                .flat_map(|c| {
                    c.navigation
                        .saved
                        .bookmarks
                        .iter()
                        .enumerate()
                        .map(move |(i, b)| {
                            (
                                c.id,
                                i,
                                c.title.clone(),
                                b.clone(),
                                c.navigation.cached_line_column(b.position),
                            )
                        })
                })
                .collect();
            if self.navigation.bookmark_sort_name {
                rows.sort_by(|a, b| a.3.description.cmp(&b.3.description));
            } else {
                rows.sort_by(|a, b| a.2.cmp(&b.2).then(a.4.cmp(&b.4)));
            }
            let mut open = true;
            let mut jump = None;
            let mut remove = None;
            let mut typing = false;
            egui::Window::new(tr("Bookmarks"))
                .open(&mut open)
                .collapsible(false)
                .default_width(620.0)
                .anchor(egui::Align2::CENTER_TOP, vec2(0.0, 85.0))
                .show(ctx, |ui| {
                    ui.checkbox(
                        &mut self.navigation.bookmark_sort_name,
                        tr("Sort by description"),
                    );
                    egui::ScrollArea::vertical()
                        .max_height(360.0)
                        .show(ui, |ui| {
                            for (id, index, title, bookmark, (line, col)) in &rows {
                                ui.push_id((*id, bookmark.position), |ui| {
                                    ui.horizontal(|ui| {
                                        if ui.button(format!("{title}  {line}:{col}")).clicked() {
                                            jump = Some((*id, bookmark.position));
                                        }
                                        if let Some(c) =
                                            self.consoles.iter_mut().find(|c| c.id == *id)
                                            && let Some(b) =
                                                c.navigation.saved.bookmarks.get_mut(*index)
                                        {
                                            let r = ui.add(
                                                egui::TextEdit::singleline(&mut b.description)
                                                    .hint_text(tr("Description"))
                                                    .desired_width(180.0),
                                            );
                                            typing |= r.has_focus();
                                            ui.label(if b.mnemonic.is_empty() {
                                                "—"
                                            } else {
                                                &b.mnemonic
                                            });
                                        }
                                        if ui.button(tr("Delete")).clicked() {
                                            remove = Some((*id, *index));
                                        }
                                    });
                                });
                            }
                        });
                    if rows.is_empty() {
                        ui.label(tr(
                            "No bookmarks. Use Toggle bookmark in an editor to add one.",
                        ));
                    }
                });
            if !typing {
                let text = ctx.input(|i| {
                    i.events.iter().find_map(|e| {
                        if let egui::Event::Text(s) = e {
                            Some(s.clone())
                        } else {
                            None
                        }
                    })
                });
                if let Some(s) = text
                    && let Some((id, _, _, b, _)) = rows.iter().find(|(_, _, _, b, _)| {
                        !b.mnemonic.is_empty() && b.mnemonic.eq_ignore_ascii_case(&s)
                    })
                {
                    jump = Some((*id, b.position));
                }
            }
            if let Some((id, index)) = remove
                && let Some(c) = self.consoles.iter_mut().find(|c| c.id == id)
            {
                c.navigation.saved.bookmarks.remove(index);
            }
            if !open || ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                self.navigation.bookmarks = false;
            }
            if let Some((id, p)) = jump {
                self.navigation.bookmarks = false;
                self.activate_navigation(ctx, Target::Tab(id), Some(p));
            }
        }
        if let Some((tab, p, ddl, text)) = self.navigation.pending_reference.take() {
            let ready = self.consoles.iter().find(|c| c.id == tab).and_then(|c| {
                c.conn
                    .and_then(|id| self.conn(id))
                    .map(|conn| (conn, c.db.as_deref()))
            });
            let loaded = ready.is_some_and(|(conn, db)| conn.schemas(db).is_some());
            let loading = ready
                .is_some_and(|(conn, db)| conn.loading.contains(conn.resolve_db(db).unwrap_or("")));
            if loaded {
                if self.consoles.get(self.active).is_some_and(|c| {
                    c.id == tab && c.text == text && c.cursor.map_or(0, |(_, b)| b) == p
                }) {
                    self.navigate_reference(ctx, tab, p, ddl);
                }
            } else if loading {
                self.navigation.pending_reference = Some((tab, p, ddl, text));
            } else {
                self.error(tr(
                    "Could not load database definitions. Refresh the database and try again.",
                ));
            }
        }
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
        if let Some(d) = &self.navigation.definitions {
            let rows = d.items.clone();
            let ddl = d.ddl;
            let mut pick = None;
            let mut open = true;
            egui::Window::new(tr("Choose database definition"))
                .open(&mut open)
                .collapsible(false)
                .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label(tr("Several objects match. Choose the exact definition."));
                    for item in rows {
                        if ui.button(item.label()).clicked() {
                            pick = Some(item);
                        }
                    }
                });
            if !open || ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                self.navigation.definitions = None;
            }
            if let Some(item) = pick {
                self.navigation.definitions = None;
                self.open_definition(ctx, item, ddl);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;

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
    fn opening_bookmarks_does_not_analyze_unrelated_cold_editors() {
        let mut app = metadata_app("SELECT 1;");
        app.consoles[1]
            .navigation
            .saved
            .bookmarks
            .push(console::navigation::Bookmark {
                position: 0,
                mnemonic: "A".into(),
                description: String::new(),
            });
        let ctx = egui::Context::default();
        app.consoles[2].append_text(&ctx, &"SELECT 123;\n".repeat(10_000));
        let mut h = harness(app);
        assert_eq!(h.state().consoles[2].navigation.index_builds, 0);
        h.state_mut().navigation.bookmarks = true;
        h.run_steps(3);
        assert_eq!(h.state().consoles[2].navigation.index_builds, 0);
        assert_eq!(h.state().consoles[1].navigation.index_builds, 1);
    }

    #[test]
    fn bookmark_zero_does_not_replace_zoom_reset() {
        let mut app = metadata_app("SELECT 1;");
        app.zoom = 1.5;
        app.consoles[1]
            .navigation
            .saved
            .bookmarks
            .push(console::navigation::Bookmark {
                position: 0,
                mnemonic: "0".into(),
                description: String::new(),
            });
        let mut h = harness(app);
        h.key_press_modifiers(Modifiers::COMMAND, Key::Num0);
        h.run_steps(2);
        assert_eq!(h.state().active, 0);
        assert_eq!(h.state().zoom, 1.0);
    }

    #[test]
    fn ddl_uses_a_new_console_for_other_sources_or_table_editors() {
        for target in 0..3 {
            let mut app = metadata_app("SELECT 1;");
            match target {
                0 => app.consoles[0].db = Some("other".into()),
                1 => app.consoles[0].conn = None,
                _ => {
                    app.consoles[0] = Console::new_table(
                        1,
                        7,
                        Some("shop".into()),
                        "public".into(),
                        "users".into(),
                    )
                }
            }
            let count = app.consoles.len();
            let table = TableRef {
                conn: 7,
                db: "shop".into(),
                schema: "public".into(),
                table: "users".into(),
            };
            app.generate(&egui::Context::default(), &table, Generate::Ddl);
            assert_eq!(app.consoles.len(), count + 1);
            assert_eq!(app.active, count);
        }
    }

    #[test]
    fn inactive_programmatic_edits_are_observed_without_scanning_other_sql() {
        let mut app = metadata_app("SELECT 1;");
        let ctx = egui::Context::default();
        app.observe_navigation();
        app.consoles[2].append_text(&ctx, "-- inactive edit");
        let expected = app.consoles[2].cursor.map(|(_, b)| b).unwrap();
        app.observe_navigation();
        assert_eq!(app.navigation.edits.last().unwrap().target, Target::Tab(3));
        app.navigation_command(&ctx, Command::LastEdit);
        assert_eq!(app.active, 2);
        assert_eq!(app.consoles[2].cursor, Some((expected, expected)));
    }

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
    fn cross_tab_history_preserves_carets_and_skips_closed_tabs() {
        let mut app = metadata_app("SELECT 123;");
        let ctx = egui::Context::default();
        app.consoles[0].cursor = Some((3, 3));
        app.observe_navigation();
        app.active = 1;
        app.consoles[1].text = "SELECT 456;".into();
        app.consoles[1].cursor = Some((7, 7));
        app.observe_navigation();
        app.navigation_command(&ctx, Command::Back);
        app.observe_navigation();
        assert_eq!(app.active, 0);
        assert_eq!(app.consoles[0].cursor, Some((3, 3)));
        app.navigation_command(&ctx, Command::Forward);
        assert_eq!(app.active, 1);
        assert_eq!(app.consoles[1].cursor, Some((7, 7)));
        app.consoles.remove(0);
        app.active = 0;
        app.navigation_command(&ctx, Command::Back);
        assert_eq!(app.consoles[app.active].id, 2);
    }

    #[test]
    fn caret_history_records_jumps_and_last_edit_crosses_tabs() {
        let mut app = metadata_app("SELECT 12345678901234567890;");
        let ctx = egui::Context::default();
        app.consoles[0].cursor = Some((0, 0));
        app.observe_navigation();
        app.consoles[0].navigate_to(&ctx, 5, None);
        app.observe_navigation();
        app.consoles[0].navigate_to(&ctx, 10, None);
        app.observe_navigation();
        let jumps = app.navigation.trail.len();
        for pos in 11..16 {
            app.consoles[0].cursor = Some((pos, pos));
            app.consoles[0].navigation.caret = Some(pos);
            app.observe_navigation();
        }
        assert_eq!(app.navigation.trail.len(), jumps);
        app.navigation_command(&ctx, Command::Back);
        assert_eq!(app.consoles[0].cursor, Some((5, 5)));
        app.navigation_command(&ctx, Command::Forward);
        assert_eq!(app.consoles[0].cursor, Some((10, 10)));
        app.navigation_command(&ctx, Command::Back);
        app.consoles[0].navigate_to(&ctx, 20, None);
        app.observe_navigation();
        app.navigation_command(&ctx, Command::Forward);
        assert_eq!(app.consoles[0].cursor, Some((20, 20)));
        app.consoles[0].append_text(&ctx, "-- edit");
        let edited = app.consoles[0].cursor.map(|(_, b)| (b, b));
        app.observe_navigation();
        app.active = 1;
        app.observe_navigation();
        app.navigation_command(&ctx, Command::LastEdit);
        assert_eq!(app.active, 0);
        assert_eq!(app.consoles[0].cursor, edited);
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
    fn revealing_a_renamed_tab_consumes_the_scroll_request_and_settles() {
        let mut app = metadata_app("SELECT 1;");
        app.renaming = Some((app.consoles[0].id, "Renaming query".into()));
        app.navigation.reveal_tab = Some(app.consoles[0].id);
        let mut h = harness(app);
        assert!(h.state().navigation.reveal_tab.is_none());
        h.run_steps(40);
        assert!(
            h.try_run().is_ok(),
            "renaming must not keep a reveal repaint loop alive"
        );
    }

    #[test]
    fn editor_settings_apply_to_existing_and_new_tabs_without_sharing_bookmarks() {
        let mut app = metadata_app("SELECT 1;");
        app.consoles[0]
            .navigation
            .saved
            .bookmarks
            .push(console::navigation::Bookmark {
                position: 0,
                mnemonic: "A".into(),
                description: String::new(),
            });
        app.navigation
            .settings
            .bindings
            .push(console::navigation::Binding {
                command: Command::ParagraphForward,
                shortcut: egui::KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::ALT, Key::J),
            });
        app.consoles[0].navigation.pending.push(Command::Settings);
        let mut h = harness(app);
        h.get_by_label("Show line numbers").click();
        h.run_steps(2);
        let app = h.state_mut();
        assert!(!app.navigation.settings.line_numbers);
        let new = app.new_console(None, None, "SELECT 2;".into());
        for console in &app.consoles {
            assert!(!console.navigation.saved.line_numbers);
            assert_eq!(console.navigation.saved.bindings.len(), 1);
        }
        assert_eq!(app.consoles[0].navigation.saved.bookmarks.len(), 1);
        assert!(app.consoles[new].navigation.saved.bookmarks.is_empty());
    }

    #[test]
    fn reconnect_starts_timing_when_pending_sql_is_sent() {
        use egui_kittest::kittest::NodeT;
        let path =
            std::env::temp_dir().join(format!("datacraft-reconnect-{}.db", uuid::Uuid::new_v4()));
        drop(rusqlite::Connection::open(&path).unwrap());
        let mut app = crate::app::tests::tabs_app();
        let mut config = ConnectionConfig::new(7, Driver::Sqlite);
        config.file_path = path.to_string_lossy().into_owned();
        app.conns.push(ConnState::new(config));
        app.active = 0;
        app.object_browser.active = false;
        app.consoles[0].conn = Some(7);
        app.consoles[0].text = "SELECT 42;".into();
        app.consoles[0].cursor = Some((0, 0));
        let mut h = harness(app);
        for _ in 0..2 {
            h.state_mut().run_console(0, false);
            assert!(h.state().consoles[0].pending.is_some());
            assert!(
                h.state().consoles[0].run_started.is_none(),
                "connection setup must not consume the prepared timing plan"
            );
            let deadline = Instant::now() + std::time::Duration::from_secs(10);
            while h.state().consoles[0].is_busy() {
                assert!(Instant::now() < deadline, "SQLite query timed out");
                h.run_steps(1);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            h.run_steps(2);
            assert!(!h.state().consoles[0].log.iter().any(|entry| entry.is_error));
            assert!(
                [" ms", " s"].iter().any(|suffix| h
                    .query_all_by_label_contains(suffix)
                    .into_iter()
                    .any(|node| {
                        node.accesskit_node()
                            .value()
                            .or_else(|| node.accesskit_node().label())
                            .is_some_and(|label| {
                                label.split_once(' ').is_some_and(|(value, unit)| {
                                    value.parse::<f64>().is_ok() && matches!(unit, "ms" | "s")
                                })
                            })
                    })),
                "the first query after connecting must show its timing beside the SQL"
            );
            h.state_mut().disconnect(7);
            h.run_steps(2);
        }
        drop(h);
        std::fs::remove_file(path).unwrap();
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
    fn qualified_alias_column_reveals_exact_explorer_node_without_opening_a_table() {
        let mut app = metadata_app("SELECT u.id FROM public.users AS u;");
        let ctx = egui::Context::default();
        let before = app.consoles.len();
        app.navigate_reference(&ctx, 1, 10, false);
        assert_eq!(
            app.explorer.selected.as_deref(),
            Some("t:7:shop:public:users:id")
        );
        assert!(app.explorer.expanded.contains("g:7:shop:public:t"));
        assert_eq!(app.consoles.len(), before);
        app.navigate_reference(&ctx, 1, 7, false);
        assert_eq!(
            app.explorer.selected.as_deref(),
            Some("t:7:shop:public:users")
        );
    }

    #[test]
    fn ambiguous_names_require_a_choice_and_quoted_names_are_case_sensitive() {
        let mut app = metadata_app("SELECT * FROM users;");
        let mut schema = app.conns[0].databases[0].schemas.as_ref().unwrap()[0].clone();
        schema.name = "archive".into();
        app.conns[0].databases[0]
            .schemas
            .as_mut()
            .unwrap()
            .push(schema);
        let ctx = egui::Context::default();
        app.navigate_reference(&ctx, 1, 15, false);
        assert_eq!(app.navigation.definitions.as_ref().unwrap().items.len(), 2);
        app.navigation.definitions = None;
        app.consoles[0].text = "SELECT * FROM \"Users\";".into();
        app.navigate_reference(&ctx, 1, 16, false);
        assert!(app.navigation.definitions.is_none());
        assert!(app.status_msg.as_ref().unwrap().1);
    }

    #[test]
    fn ddl_reuses_a_matching_editor_and_local_ctes_navigate_locally() {
        let mut app = metadata_app("SELECT * FROM public.users;");
        let ctx = egui::Context::default();
        let original = app.consoles[0].text.clone();
        app.navigate_reference(&ctx, 1, 23, true);
        assert!(app.consoles[0].text.starts_with(&original));
        assert!(
            app.consoles[app.active]
                .text
                .to_ascii_uppercase()
                .contains("CREATE TABLE")
        );
        assert_eq!(app.active, 0);
        app.active = 0;
        app.consoles[0].text = "WITH totals AS (SELECT 1) SELECT * FROM totals;".into();
        app.navigate_reference(&ctx, 1, 42, true);
        assert_eq!(app.active, 0);
        assert_eq!(app.consoles[0].cursor, Some((5, 5)));
    }

    #[test]
    fn bookmarks_from_other_tabs_are_visible_and_jump_to_their_own_editor() {
        let mut app = metadata_app("SELECT 1;");
        app.consoles[1].text = "SELECT 222;".into();
        app.consoles[1]
            .navigation
            .saved
            .bookmarks
            .push(console::navigation::Bookmark {
                position: 7,
                mnemonic: "A".into(),
                description: "Other query".into(),
            });
        app.navigation.bookmarks = true;
        let mut h = harness(app);
        assert!(h.query_by_label("Query 2  1:8").is_some());
        h.get_by_label("Query 2  1:8").click();
        h.run_steps(2);
        assert_eq!(h.state().active, 1);
        assert_eq!(h.state().consoles[1].cursor, Some((7, 7)));
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

    #[test]
    fn numbered_bookmark_uses_ctrl_shift_and_local_ctes_work_without_a_connection() {
        let mut app = metadata_app("WITH totals AS (SELECT 1) SELECT * FROM totals;");
        let ctx = egui::Context::default();
        app.consoles[0].conn = None;
        app.navigate_reference(&ctx, 1, 42, true);
        assert_eq!(app.consoles[0].cursor, Some((5, 5)));
        app.consoles[1].text = "SELECT 22;".into();
        app.consoles[1]
            .navigation
            .saved
            .bookmarks
            .push(console::navigation::Bookmark {
                position: 7,
                mnemonic: "3".into(),
                description: String::new(),
            });
        let mut h = harness(app);
        h.key_press_modifiers(Modifiers::CTRL | Modifiers::SHIFT, Key::Num3);
        h.run_steps(2);
        assert_eq!(h.state().active, 1);
        assert_eq!(h.state().consoles[1].cursor, Some((7, 7)));
    }
}
