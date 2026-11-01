//! Main application state and UI composition (DataGrip "Islands" style).
//!
//! Debug environment variable: `DATACRAFT_DEMO=<path.db>` (or a server URL like
//! `mariadb://root:pw@127.0.0.1:3307/shop`, see `demo_server_config`) starts with a temporary
//! connection (not saved to `connections.json`) to that file, connects it, opens the data
//! editor of its first table and a console with a sample query. In that mode the UI state
//! is persisted to a temporary directory (see `main.rs`).
//! `DATACRAFT_DEMO_DARK=1` forces Islands Dark, `DATACRAFT_DEMO_DIALOG=1` opens the new
//! connection dialog and `DATACRAFT_DEMO_TAB=console` leaves the console active.
//! `DATACRAFT_DEMO_ZOOM=<percent>` sets the IDE zoom (e.g. 125).
//! `DATACRAFT_DEMO_EDIT=cell|changes|preview` simulates in memory, on the data editor of the
//! first table, a cell being edited, several pending changes (modified cell, NULL, new row
//! and a row marked for deletion) or those changes with the preview window open.
//! They are never sent to the database.
//! `DATACRAFT_DEMO_EXPLORER_W=<pt>` fixes the Database Explorer width (150–700, not
//! resizable), `DATACRAFT_DEMO_WINDOW=<width>x<height>` resizes the window at startup
//! (may go below the normal minimum) and `DATACRAFT_DEMO_MULTI=1` uses a console with
//! several queries, run with "Run all" (one result tab each).
//! `DATACRAFT_DEMO_FILTER=<column>` opens that column's funnel with its first value unchecked;
//! `<column>=<text>` types that text in its search box (which also filters the grid).
//! `DATACRAFT_DEMO_LANG=en|es|fr|pt|de|pl|zh|ja` forces the interface language.
//! `DATACRAFT_DEMO_IMPORT=<file.csv>` opens "Import Data from File" on the first table with
//! that file loaded; `DATACRAFT_DEMO_IMPORT_SUBMIT=1` also imports it.
//! `DATACRAFT_DEMO_COPY=1` opens "Copy Table To…" on the first table, into `<name>_copy`;
//! `mask` also applies "Suggest masking".
//! `DATACRAFT_DEMO_MASK_EXPORT=1` opens "Export with Masking…" on the first table's data.
//! `DATACRAFT_DEMO_GRID=record|clone` opens the record view of the first row of the first
//! grid shown, or clones that row (pending, never saved).
//! `DATACRAFT_DEMO_CREATE=database` opens "New Database" with its options;
//! `DATACRAFT_DEMO_CREATE=table` opens the "New Table" dialog with a few sample columns;
//! `table-submit` also creates that table in the demo database.
//! `DATACRAFT_DEMO_DROP=table|truncate|rename` opens the confirmation to drop, truncate or
//! rename (to `<name>_renamed`) the first table; with `-submit` (e.g. `rename-submit`) it also runs it.
//! `DATACRAFT_DEMO_VIEW=new|edit` opens "New View" over the first table or "Edit View" on the first view.
//! `DATACRAFT_DEMO_MODIFY=1` opens "Modify Table…" on the first table; `edit` also makes a few
//! sample changes (shown in the preview, never run).
//! `DATACRAFT_DEMO_USERS=1` expands the "Users" / "Roles" folder; `new|edit|drop` also opens that
//! dialog (edit and drop on the first account that is not root).

use crate::config;
use crate::db::{ColumnInfo, ConnectionConfig, DatabaseInfo, Driver, SchemaInfo, Session, TableInfo};
use crate::edit;
use crate::i18n::{self, Lang, tr, trf};
use crate::sql;
use crate::ui::connection_dialog::{ConnectionDialog, DialogResult};
use crate::ui::console::{self, CloseResults, Console, ConsoleAction, ConsoleCtx, ResultTab};
use crate::ui::create_dialog::{self, CreateDialog, CreateResult, CreateTarget};
use crate::ui::drop_dialog::{DropDialog, DropKind, DropResult};
use crate::ui::copy_dialog::{CopyAction, CopyDialog};
use crate::ui::import_dialog::{ImportDialog, ImportResult};
use crate::ui::mask_export_dialog::{MaskExportAction, MaskExportDialog};
use crate::ui::modify_table_dialog::{ModifyResult, ModifyTableDialog};
use crate::ui::editing::{self, RowRef};
use crate::ui::explorer::{self, ExplorerAction, ExplorerState, TableRef};
use crate::ui::icons::{self, Ico};
use crate::ui::results::{self, OutputEntry, ResultView};
use crate::ui::theme::{self, palette};
use crate::ui::{ConnStatusKind, ConnSummary, quote_ident_if_needed};
use crate::worker::{ConnId, Request, Response, Worker};
use egui::{FontId, Key, Margin, Modifiers, Rect, RichText, Sense, Ui, pos2, vec2};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

mod routines;
mod table_actions;
mod views;

#[cfg(all(test, target_os = "windows"))]
mod readme_screenshots;

const STORAGE_KEY: &str = "datacraft_ui_state";
const DEFAULT_FONT_SIZE: f32 = 13.0;

/// IDE zoom steps.
pub const ZOOM_STEPS: [f32; 11] = [0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0];

/// Next zoom step above `z`.
pub fn zoom_in(z: f32) -> f32 {
    ZOOM_STEPS.iter().copied().find(|s| *s > z + 0.001).unwrap_or(ZOOM_STEPS[ZOOM_STEPS.len() - 1])
}

/// Previous zoom step below `z`.
pub fn zoom_out(z: f32) -> f32 {
    ZOOM_STEPS.iter().rev().copied().find(|s| *s < z - 0.001).unwrap_or(ZOOM_STEPS[0])
}

/// Valid zoom (within 50%–200%; non-finite values go back to 100%).
pub fn clamp_zoom(z: f32) -> f32 {
    if z.is_finite() { z.clamp(ZOOM_STEPS[0], ZOOM_STEPS[ZOOM_STEPS.len() - 1]) } else { 1.0 }
}

pub fn zoom_label(z: f32) -> String {
    format!("{:.0}%", z * 100.0)
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConnStatus {
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

impl ConnStatus {
    fn kind(&self) -> ConnStatusKind {
        match self {
            ConnStatus::Disconnected => ConnStatusKind::Disconnected,
            ConnStatus::Connecting => ConnStatusKind::Connecting,
            ConnStatus::Connected => ConnStatusKind::Connected,
            ConnStatus::Error(_) => ConnStatusKind::Error,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            ConnStatus::Disconnected => tr("disconnected"),
            ConnStatus::Connecting => tr("connecting…"),
            ConnStatus::Connected => tr("connected"),
            ConnStatus::Error(_) => tr("error"),
        }
    }
}

pub struct ConnState {
    pub cfg: ConnectionConfig,
    pub status: ConnStatus,
    /// Server databases (empty if not connected).
    pub databases: Vec<DatabaseInfo>,
    /// Databases whose schema is being loaded.
    pub loading: HashSet<String>,
    /// Last error loading each database.
    pub db_errors: HashMap<String, String>,
    /// The "Users" / "Roles" folder.
    pub users: crate::ui::user_dialog::UsersNode,
    /// Procedures and functions of each database (engines that have them).
    pub routines: HashMap<String, crate::db::routines::RoutineList>,
}

impl ConnState {
    pub fn new(cfg: ConnectionConfig) -> Self {
        Self {
            cfg,
            status: ConnStatus::Disconnected,
            databases: Vec::new(),
            loading: HashSet::new(),
            db_errors: HashMap::new(),
            users: Default::default(),
            routines: HashMap::new(),
        }
    }

    fn clear(&mut self) {
        self.databases.clear();
        self.loading.clear();
        self.db_errors.clear();
        self.users = Default::default();
        self.routines.clear();
    }

    /// The connection's default database (if connected).
    pub fn default_db(&self) -> Option<&str> {
        self.databases.iter().find(|d| d.current).map(|d| d.name.as_str())
    }

    /// Actual name of `db` (`None` = the default one).
    pub fn resolve_db<'a>(&'a self, db: Option<&'a str>) -> Option<&'a str> {
        db.filter(|d| !d.is_empty()).or_else(|| self.default_db())
    }

    pub fn database(&self, db: Option<&str>) -> Option<&DatabaseInfo> {
        let name = self.resolve_db(db)?;
        self.databases.iter().find(|d| d.name == name)
    }

    /// Schemas of `db` if already loaded.
    pub fn schemas(&self, db: Option<&str>) -> Option<&[SchemaInfo]> {
        self.database(db)?.schemas.as_deref()
    }

    fn summary(&self) -> ConnSummary {
        ConnSummary {
            id: self.cfg.id,
            name: self.cfg.name.clone(),
            driver: self.cfg.driver,
            status: self.status.kind(),
            databases: self.databases.iter().map(|d| d.name.clone()).collect(),
            default_db: self.default_db().map(str::to_string),
        }
    }
}

/// Action awaiting confirmation because it would discard unsaved changes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Discard {
    Close(u64),
    CloseAll,
    ApplyFilter(u64),
    Page(u64, console::PageMove),
    CloseTabs(u64, CloseResults),
    Reload(u64),
    Run(u64, bool),
    /// Close a console's result tabs.
    CloseResults(u64, CloseResults),
}

/// "Preview pending changes" window.
struct PreviewState {
    console: u64,
    view: usize,
    statements: Vec<String>,
    error: Option<String>,
    applying: bool,
    /// false on a direct submit (⇧ / ⌘S): the window is not shown.
    visible: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DemoEdit {
    Cell,
    Changes,
    Preview,
}

#[derive(Serialize, Deserialize)]
struct PersistedConsole {
    #[serde(default)]
    routine_comment: Option<crate::ui::routine_dialog::RoutineComment>,
    #[serde(default)]
    routine: Option<(crate::db::routines::RoutineRef, String, bool)>,
    title: String,
    text: String,
    conn: Option<u64>,
    /// (schema, table) if the tab is a data editor.
    #[serde(default)]
    table: Option<(String, String)>,
    #[serde(default)]
    table_view: console::TableViewOptions,
    /// The tab's database (`None` = the connection's default).
    #[serde(default)]
    db: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Persisted {
    object_organizations: Vec<crate::ui::object_organization::Organization>,
    backups: Vec<crate::db::backup::BackupRecord>,
    object_layout: crate::ui::object_browser::ObjectLayout,
    save_on_row_change: bool,
    /// Islands Dark theme (Islands Light by default). Replaces the old `dark` (Darcula).
    islands_dark: bool,
    font_size: f32,
    max_rows: usize,
    /// Completion list opens while typing in the console.
    auto_complete: bool,
    consoles: Vec<PersistedConsole>,
    active: usize,
    console_counter: u64,
    show_explorer: bool,
    show_results: bool,
    /// IDE zoom (1.0 = 100%).
    zoom: f32,
    /// Interface language (`None` = the system one).
    lang: Option<Lang>,
    render_mode: crate::ui::performance::RenderMode,
    show_frame_stats: bool,
    /// Look for a newer release on GitHub a little after launch.
    check_updates: bool,
    /// "Skip this version": no notice until a newer one.
    skipped_version: Option<String>,
}

impl Default for Persisted {
    fn default() -> Self {
        Self {
            object_organizations: Vec::new(),
            backups: Vec::new(),
            object_layout: crate::ui::object_browser::ObjectLayout::default(),
            save_on_row_change: false,
            islands_dark: false,
            font_size: DEFAULT_FONT_SIZE,
            max_rows: 1000,
            auto_complete: true,
            consoles: Vec::new(),
            active: 0,
            console_counter: 0,
            show_explorer: true,
            show_results: true,
            zoom: 1.0,
            lang: None,
            render_mode: Default::default(),
            show_frame_stats: false,
            check_updates: true,
            skipped_version: None,
        }
    }
}

pub struct DataCraftApp {
    worker: Worker,
    conns: Vec<ConnState>,
    /// Editor tabs: consoles and data editors.
    consoles: Vec<Console>,
    active: usize,
    next_console_id: u64,
    console_counter: u64,
    result_salt: u64,
    explorer: ExplorerState,
    backup: Option<crate::ui::backup_dialog::BackupDialog>,
    scheduler: Option<crate::scheduler::dialog::Dialog>,
    object_browser: crate::ui::object_browser::ObjectBrowser,
    save_on_row_change: bool,
    dialog: Option<ConnectionDialog>,
    confirm_delete: Option<ConnId>,
    /// "Explain Analyze" of a statement that writes, waiting for confirmation: (console, explain).
    confirm_analyze: Option<(usize, String)>,
    /// A connection imported without its password, and the password being typed.
    ask_password: Option<(ConnId, String)>,
    /// "New Database" / "New Table" dialog.
    create: Option<CreateDialog>,
    /// Confirmation to drop a database, table or view.
    drop: Option<DropDialog>,
    /// "New View" / "Edit View" dialog and pending view definitions.
    views: views::ViewState,
    /// Procedure and function dialogs and pending definitions.
    routines: routines::RoutineState,
    table_actions: table_actions::TableActions,
    import: Option<ImportDialog>,
    /// "Copy Table To…".
    copy: Option<CopyDialog>,
    /// "Export with Masking…".
    mask_export: Option<MaskExportDialog>,
    /// "Modify Table…".
    modify: Option<ModifyTableDialog>,
    /// "New User…" / "Edit User…" / "Drop User…" (and the PostgreSQL role ones).
    user_dialog: Option<crate::ui::user_dialog::UserDialog>,
    renaming: Option<(u64, String)>,
    show_about: bool,
    show_shortcuts: bool,
    show_settings: bool,
    script: Option<crate::ui::script_dialog::ScriptDialog>,
    check_updates: bool,
    skipped_version: Option<String>,
    /// The update check under way, and whether it was asked for ("Check for Updates…").
    update_check: Option<(std::sync::mpsc::Receiver<Result<crate::update::Release, String>>, bool)>,
    /// A newer release, shown in the corner until skipped or dismissed.
    update: Option<crate::update::Release>,
    /// Native macOS menu bar (None if it could not be installed).
    native_menu: Option<crate::native_menu::NativeMenu>,
    show_explorer: bool,
    show_results: bool,
    dark: bool,
    applied_dark: Option<bool>,
    font_size: f32,
    max_rows: usize,
    auto_complete: bool,
    status_msg: Option<(String, bool)>,
    /// Last connection id handed out (so ids are monotonic).
    last_conn_id: u64,
    /// Demo mode: open the first table's data editor on connect.
    demo_pending: bool,
    /// Demo mode: simulated changes to apply when the data editor loads.
    demo_edit: Option<DemoEdit>,
    /// IDE zoom and the last one applied to the context.
    zoom: f32,
    applied_zoom: Option<f32>,
    preview: Option<PreviewState>,
    confirm_discard: Option<Discard>,
    /// Demo mode: fixed Database Explorer width (`DATACRAFT_DEMO_EXPLORER_W`).
    demo_explorer_w: Option<f32>,
    /// Demo mode: window size to apply on the first frame (`DATACRAFT_DEMO_WINDOW`).
    demo_window: Option<egui::Vec2>,
    /// Language chosen by the user (`None` = follow the system).
    lang: Option<Lang>,
    /// Low-power rendering for software adapters and Remote Desktop.
    perf: crate::ui::performance::Performance,
}

impl DataCraftApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let demo = std::env::var("DATACRAFT_DEMO").ok().filter(|s| !s.is_empty());
        let persisted: Persisted = if demo.is_some() {
            Persisted::default()
        } else {
            cc.storage.and_then(|s| eframe::get_value(s, STORAGE_KEY)).unwrap_or_default()
        };
        // Before any text is created (native menu, first console...).
        // Demo mode can force the language: DATACRAFT_DEMO_LANG=en|es|fr|pt|de|pl|zh|ja.
        let demo_lang = demo.as_ref().and(std::env::var("DATACRAFT_DEMO_LANG").ok()).map(|l| Lang::from_locale(&l));
        i18n::set_lang(demo_lang.or(persisted.lang).unwrap_or_else(Lang::system));
        let conns: Vec<ConnState> = if demo.is_some() {
            Vec::new()
        } else {
            config::load_connections()
                .into_iter()
                .map(ConnState::new)
                .collect()
        };
        let last_conn_id = conns.iter().map(|c| c.cfg.id).max().unwrap_or(0);

        theme::install_fonts(&cc.egui_ctx);
        let mut app = Self {
            worker: Worker::start(cc.egui_ctx.clone()),
            conns,
            consoles: Vec::new(),
            active: 0,
            next_console_id: 1,
            console_counter: persisted.console_counter,
            result_salt: 0,
            explorer: { let mut explorer = ExplorerState::default(); explorer.backups = persisted.backups; explorer },
            backup: None,
            scheduler: None,
            object_browser: {
                let mut browser = crate::ui::object_browser::ObjectBrowser::default();
                browser.organizations = persisted.object_organizations;
                browser.layout = persisted.object_layout;
                browser
            },
            save_on_row_change: persisted.save_on_row_change,
            dialog: None,
            confirm_delete: None,
            confirm_analyze: None,
            ask_password: None,
            create: None,
            drop: None,
            views: Default::default(),
            routines: Default::default(),
            table_actions: Default::default(),
            import: None,
            copy: None,
            mask_export: None,
            modify: None,
            user_dialog: None,
            renaming: None,
            show_about: false,
            show_shortcuts: false,
            show_settings: false,
            script: None,
            check_updates: persisted.check_updates,
            skipped_version: persisted.skipped_version,
            update_check: None,
            update: None,
            native_menu: crate::native_menu::NativeMenu::install(&cc.egui_ctx),
            show_explorer: persisted.show_explorer,
            show_results: persisted.show_results,
            dark: persisted.islands_dark,
            applied_dark: None,
            font_size: persisted.font_size.clamp(8.0, 32.0),
            max_rows: persisted.max_rows.max(1),
            auto_complete: persisted.auto_complete,
            status_msg: None,
            last_conn_id,
            demo_pending: false,
            demo_edit: None,
            zoom: clamp_zoom(persisted.zoom),
            applied_zoom: None,
            preview: None,
            confirm_discard: None,
            demo_explorer_w: None,
            demo_window: None,
            lang: persisted.lang,
            perf: crate::ui::performance::Performance::new(cc.wgpu_render_state.as_ref(), persisted.render_mode, persisted.show_frame_stats),
        };
        app.restore_tabs(persisted.consoles, persisted.active);
        if app.check_updates && demo.is_none() {
            app.update_check = Some((crate::update::check(cc.egui_ctx.clone(), std::time::Duration::from_secs(5)), false));
        }
        if let Some(path) = demo {
            app.start_demo(&path);
        }
        theme::apply(&cc.egui_ctx, app.dark);
        app.applied_dark = Some(app.dark);
        app.perf.apply_style(&cc.egui_ctx);
        // Keyboard zoom is handled by the app (⌘= / ⌘- / ⌘0) so it is not applied twice.
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
        cc.egui_ctx.set_zoom_factor(app.zoom);
        app.applied_zoom = Some(app.zoom);
        app
    }

    /// Keep saved SQL accessible without selecting or creating a console at startup.
    fn restore_tabs(&mut self, consoles: Vec<PersistedConsole>, active: usize) {
        for pc in consoles {
            let conn = pc.conn.filter(|id| self.conns.iter().any(|c| c.cfg.id == *id));
            let id = self.alloc_console_id();
            match (pc.table, conn) {
                (Some((schema, table)), Some(conn)) => {
                    let mut console = Console::new_table(id, conn, pc.db, schema, table);
                    if let Some(t) = &mut console.table { t.save_on_row_change = self.save_on_row_change; t.view = pc.table_view; }
                    self.consoles.push(console);
                }
                (Some(_), None) => {}
                (None, _) => {
                    let mut c = Console::new(id, pc.title, pc.text, conn);
                    c.db = pc.db.filter(|_| conn.is_some());
                    if let Some((reference, saved, transaction)) = pc.routine
                        && let Some(driver) = conn.and_then(|id| self.conn(id)).map(|c| c.cfg.driver)
                    {
                        let spec = crate::db::routines::RoutineSpec { kind: reference.kind, schema: reference.schema.clone(), name: reference.name.clone(), ..Default::default() };
                        let mut dialog = crate::ui::routine_dialog::RoutineDialog::edit_routine(reference.conn, driver, reference.db.clone(), spec, 0);
                        dialog.loading = None;
                        dialog.raw = Some(c.text.clone());
                        c.routine = Some(crate::ui::routine_dialog::RoutineEditor { comment: pc.routine_comment.unwrap_or_default(), reference, dialog, saved, submitted: None, transaction, page: Default::default(), word_wrap: false, outline: true, snippets: true });
                    }
                    self.consoles.push(c);
                }
            }
        }
        self.active = active.min(self.consoles.len().saturating_sub(1));
        self.object_browser.active = true;
    }

    /// Demo mode (`DATACRAFT_DEMO`): temporary SQLite connection, console and data editor.
    fn start_demo(&mut self, path: &str) {
        let id = self.next_conn_id();
        let cfg = match demo_server_config(id, path) {
            Some(cfg) => cfg,
            None => {
                let mut cfg = ConnectionConfig::new(id, Driver::Sqlite);
                cfg.name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.into());
                cfg.file_path = path.to_string();
                cfg
            }
        };
        self.conns.push(ConnState::new(cfg));
        self.explorer.expanded.insert(format!("c:{id}"));
        self.explorer.selected = Some(format!("c:{id}"));
        self.dark = std::env::var("DATACRAFT_DEMO_DARK").is_ok_and(|v| v == "1");
        if std::env::var("DATACRAFT_DEMO_DIALOG").is_ok_and(|v| v == "1") {
            self.open_new_connection_dialog();
        }
        if let Some(z) = std::env::var("DATACRAFT_DEMO_ZOOM").ok().and_then(|v| v.trim().trim_end_matches('%').parse::<f32>().ok()) {
            self.zoom = clamp_zoom(z / 100.0);
        }
        self.demo_explorer_w = std::env::var("DATACRAFT_DEMO_EXPLORER_W").ok().and_then(|v| v.trim().parse::<f32>().ok()).map(|w| w.clamp(150.0, 700.0));
        self.demo_window = std::env::var("DATACRAFT_DEMO_WINDOW").ok().and_then(|v| {
            let (w, h) = v.trim().split_once('x')?;
            Some(vec2(w.trim().parse::<f32>().ok()?.max(400.0), h.trim().parse::<f32>().ok()?.max(300.0)))
        });
        self.demo_edit = match std::env::var("DATACRAFT_DEMO_EDIT").as_deref() {
            Ok("cell") => Some(DemoEdit::Cell),
            Ok("changes") => Some(DemoEdit::Changes),
            Ok("preview") => Some(DemoEdit::Preview),
            _ => None,
        };
        let driver = self.conn(id).map(|c| c.cfg.driver);
        let mut text = match driver {
            Some(Driver::Redis) => String::from("HGETALL user:1\nINFO server\n"),
            Some(Driver::MongoDb) => String::from("db.orders.find({total: {$gt: 10}}).sort({total: -1})\n"),
            _ => String::from("SELECT o.id, u.name, o.total\nFROM orders o\nJOIN users u ON u.id = o.user_id\nWHERE o.total > 10\nORDER BY o.total DESC;\n"),
        };
        if std::env::var("DATACRAFT_DEMO_MULTI").is_ok_and(|v| v == "1") && !driver.is_some_and(|d| d.is_nosql()) {
            text.push_str("\nSELECT * FROM users;\n\nSELECT count(*) AS n FROM orders;\n");
        }
        let idx = self.new_console(Some(id), None, text);
        self.run_console(idx, true);
        // The first table's data editor opens when the schema arrives.
        self.demo_pending = true;
    }

    fn alloc_console_id(&mut self) -> u64 {
        let id = self.next_console_id;
        self.next_console_id += 1;
        id
    }

    /// Creates a console and activates it. Returns its index.
    fn new_console(&mut self, conn: Option<ConnId>, title: Option<String>, text: String) -> usize {
        self.object_browser.active = false;
        let id = self.alloc_console_id();
        let title = title.unwrap_or_else(|| {
            self.console_counter += 1;
            format!("console_{}", self.console_counter)
        });
        self.consoles.push(Console::new(id, title, text, conn));
        self.active = self.consoles.len() - 1;
        self.active
    }

    fn conn(&self, id: ConnId) -> Option<&ConnState> {
        self.conns.iter().find(|c| c.cfg.id == id)
    }

    fn conn_mut(&mut self, id: ConnId) -> Option<&mut ConnState> {
        self.conns.iter_mut().find(|c| c.cfg.id == id)
    }

    fn console_by_id(&mut self, id: u64) -> Option<&mut Console> {
        self.consoles.iter_mut().find(|c| c.id == id)
    }

    fn info(&mut self, msg: impl Into<String>) {
        self.status_msg = Some((msg.into(), false));
    }

    fn error(&mut self, msg: impl Into<String>) {
        self.status_msg = Some((msg.into(), true));
    }

    fn persist_connections(&mut self) {
        let list: Vec<ConnectionConfig> = self.conns.iter().map(|c| c.cfg.clone()).collect();
        if let Err(e) = config::save_connections(&list) {
            self.error(trf("Could not save the connections: {error}", &[("error", &format!("{e:#}"))]));
        }
    }

    /// Known table of a database (for primary keys, DDL and editing).
    fn table_info(&self, conn: ConnId, db: Option<&str>, schema: &str, table: &str) -> Option<&TableInfo> {
        self.conn(conn)?.schemas(db)?.iter().find(|s| s.name == schema)?.tables.iter().find(|t| t.name == table)
    }

    fn table_columns(&self, conn: ConnId, db: Option<&str>, schema: &str, table: &str) -> Option<&[ColumnInfo]> {
        self.table_info(conn, db, schema, table).map(|t| t.columns.as_slice())
    }

    /// Asks the worker for a database's schema.
    fn load_database(&mut self, conn: ConnId, db: String) {
        let Some(c) = self.conn_mut(conn) else { return };
        if !matches!(c.status, ConnStatus::Connected) || c.loading.contains(&db) {
            return;
        }
        c.loading.insert(db.clone());
        c.db_errors.remove(&db);
        self.worker.send(Request::LoadDatabase { conn, db });
    }

    /// Loads the database if not loaded yet (autocomplete and data editors).
    fn ensure_db_loaded(&mut self, conn: ConnId, db: Option<&str>) {
        let Some(c) = self.conn(conn) else { return };
        if !matches!(c.status, ConnStatus::Connected) {
            return;
        }
        let Some(name) = c.resolve_db(db).map(str::to_string) else { return };
        let needs = c.databases.iter().any(|d| d.name == name && d.schemas.is_none())
            && !c.loading.contains(&name)
            && !c.db_errors.contains_key(&name);
        if needs {
            self.load_database(conn, name);
        }
    }

    // ---------------------------------------------------------------- connection

    fn connect(&mut self, id: ConnId) {
        if self.conn(id).is_some_and(|c| c.cfg.ask_password && c.cfg.password.is_empty()) {
            self.ask_password = Some((id, String::new()));
            return;
        }
        if let Some(c) = self.conn_mut(id) {
            if matches!(c.status, ConnStatus::Connecting) {
                return;
            }
            c.status = ConnStatus::Connecting;
            let cfg = c.cfg.clone();
            self.worker.send(Request::Connect(cfg));
        }
    }

    fn disconnect(&mut self, id: ConnId) {
        if let Some(c) = self.conn_mut(id) {
            c.status = ConnStatus::Disconnected;
            c.clear();
            self.worker.send(Request::Disconnect(id));
        }
        // Whatever was waiting for this connection will no longer run.
        for console in self.consoles.iter_mut().filter(|c| c.conn == Some(id)) {
            if console.pending.take().is_some() {
                console.run_started = None;
            }
        }
    }

    fn handle_responses(&mut self) {
        for resp in self.worker.poll() {
            match resp {
                Response::Connected { conn, databases } => {
                    // Stale response: the connection was disconnected, edited or deleted while connecting.
                    if !self.conn(conn).is_some_and(|c| c.status == ConnStatus::Connecting) {
                        self.worker.send(Request::Disconnect(conn));
                        continue;
                    }
                    if let Some(c) = self.conn_mut(conn) {
                        c.status = ConnStatus::Connected;
                        c.clear();
                        c.databases = databases;
                    }
                    let max_rows = self.max_rows;
                    for console in self.consoles.iter_mut().filter(|c| c.conn == Some(conn)) {
                        if let Some(statements) = console.pending.take() {
                            console.running = true;
                            let db = console.db.clone();
                            self.worker.send(Request::Execute { conn, db, console: console.id, statements, max_rows });
                        }
                    }
                    self.recheck_edit_targets(conn);
                    self.open_startup_table(conn);
                    if self.demo_pending {
                        self.demo_pending = false;
                        if std::env::var("DATACRAFT_DEMO_USERS").is_ok() {
                            self.explorer.expanded.insert(format!("u:{conn}"));
                            self.load_users(conn);
                        }
                        let first = self.conn(conn).and_then(|c| {
                            let db = c.default_db()?.to_string();
                            c.schemas(None)?
                                .iter()
                                .find_map(|s| s.tables.first().map(|t| (db.clone(), s.name.clone(), t.name.clone())))
                        });
                        if let Some((db, schema, table)) = first {
                            let tref = TableRef { conn, db, schema, table };
                            self.explorer.expanded.insert(explorer::table_key(&tref));
                            self.explorer.selected = Some(explorer::table_key(&tref));
                            self.explorer.selected_table = Some(tref.clone());
                            self.open_table(&tref);
                        }
                        if std::env::var("DATACRAFT_DEMO_TAB").is_ok_and(|v| v == "console") {
                            self.active = 0;
                        }
                        let demo_drop = std::env::var("DATACRAFT_DEMO_DROP").unwrap_or_default();
                        let demo_kind = match demo_drop.split('-').next() {
                            Some("table") => Some(DropKind::Table),
                            Some("truncate") => Some(DropKind::Truncate),
                            Some("rename") => Some(DropKind::Rename),
                            _ => None,
                        };
                        if let Some(kind) = demo_kind
                            && let Some(t) = self.explorer.selected_table.clone()
                            && let Some(driver) = self.conn(conn).map(|c| c.cfg.driver)
                        {
                            let mut d = DropDialog::object(conn, driver, kind, t.db.clone(), t.schema, t.table);
                            if kind == DropKind::Rename {
                                d.new_name = format!("{}_renamed", d.name);
                            }
                            if demo_drop.ends_with("-submit") {
                                d.running = true;
                                let statements = vec![d.sql()];
                                self.worker.send(Request::RunDdl { conn, db: Some(t.db), statements, close_db: None, transaction: false });
                            }
                            self.drop = Some(d);
                        }
                        if let Ok(csv) = std::env::var("DATACRAFT_DEMO_IMPORT")
                            && let Some(t) = self.explorer.selected_table.clone()
                        {
                            self.open_import(t);
                            if let Some(d) = self.import.as_mut() {
                                d.load(std::path::Path::new(&csv));
                            }
                            if std::env::var("DATACRAFT_DEMO_IMPORT_SUBMIT").is_ok_and(|v| v == "1") {
                                self.submit_import();
                            }
                        }
                        let demo_copy = std::env::var("DATACRAFT_DEMO_COPY").unwrap_or_default();
                        if matches!(demo_copy.as_str(), "1" | "mask")
                            && let Some(t) = self.explorer.selected_table.clone()
                        {
                            self.open_copy(t);
                            if let Some(d) = self.copy.as_mut() {
                                d.target_table.push_str("_copy");
                                if demo_copy == "mask" {
                                    d.suggest_masks();
                                }
                            }
                        }
                        self.demo_view(conn);
                        if matches!(std::env::var("DATACRAFT_DEMO_MODIFY").as_deref(), Ok("1" | "edit"))
                            && let Some(t) = self.explorer.selected_table.clone()
                        {
                            self.open_modify_table(t);
                        }
                        let demo_create = std::env::var("DATACRAFT_DEMO_CREATE").unwrap_or_default();
                        if demo_create == "database" && self.conn(conn).is_some_and(|c| c.cfg.driver.has_databases()) {
                            self.create = Some(CreateDialog::database(conn, self.conn(conn).map_or(Driver::Postgres, |c| c.cfg.driver)));
                            if let Some(d) = self.create.as_mut() {
                                d.name = "inventory".into();
                            }
                            self.worker.send(Request::DatabaseChoices { conn });
                        }
                        if demo_create.starts_with("table")
                            && let Some(db) = self.conn(conn).and_then(|c| c.default_db().map(str::to_string))
                        {
                            self.open_new_table(conn, db.clone(), None);
                            if let Some(d) = self.create.as_mut() {
                                d.name = "customers".into();
                                d.columns.extend(create_dialog::demo_columns());
                                if demo_create == "table-submit" {
                                    d.running = true;
                                    let statements = vec![d.sql()];
                                    self.worker.send(Request::RunDdl { conn, db: Some(db), statements, close_db: None, transaction: false });
                                }
                            }
                        }
                    }
                }
                Response::ConnectFailed { conn, error } => {
                    let was_connected = self.conn(conn).is_some_and(|c| c.status == ConnStatus::Connected);
                    if let Some(c) = self.conn_mut(conn) {
                        // Also if it was connected: the session is lost and the next run
                        // will reconnect.
                        c.status = ConnStatus::Error(error.clone());
                    }
                    if was_connected {
                        self.error(trf("Connection lost: {error}", &[("error", &error)]));
                    }
                    for console in self.consoles.iter_mut().filter(|c| c.conn == Some(conn)) {
                        if console.pending.take().is_some() {
                            console.run_started = None;
                            console.log.push(OutputEntry {
                                time: now(),
                                statement: String::new(),
                                message: trf("Could not connect: {error}", &[("error", &error)]),
                                is_error: true,
                            });
                            console.result_tab = ResultTab::Output;
                        }
                    }
                }
                Response::TestResult { conn, result } => {
                    if let Some(d) = self.dialog.as_mut() {
                        d.set_test_result(conn, result);
                    }
                }
                Response::Disconnected(conn) => {
                    if let Some(c) = self.conn_mut(conn)
                        && c.status != ConnStatus::Connecting
                    {
                        c.status = ConnStatus::Disconnected;
                        c.clear();
                    }
                }
                Response::Refreshed { conn, databases } => self.refreshed(conn, databases),
                Response::DatabaseLoaded { conn, db, result } => {
                    if let Some(c) = self.conn_mut(conn) {
                        c.loading.remove(&db);
                        match result {
                            Ok(schemas) => {
                                c.db_errors.remove(&db);
                                if let Some(d) = c.databases.iter_mut().find(|d| d.name == db) {
                                    d.schemas = Some(schemas);
                                }
                                // The routines follow in their own query: the tree shows first.
                                self.load_routines(conn, &db);
                            }
                            Err(e) => {
                                c.db_errors.insert(db.clone(), e);
                            }
                        }
                    }
                    self.recheck_edit_targets(conn);
                    if self.object_browser.active && let Some(t) = self.object_browser.selected.clone().filter(|t| t.conn == conn && t.db == db) {
                        self.inspect_object(t, false);
                    }
                    if self.object_browser.active && self.object_browser.scope.as_ref().is_some_and(|scope| scope.conn == conn && scope.db == db) {
                        self.load_object_catalog();
                    }
                }
                Response::Executed { console, results, error, elapsed } => {
                    self.result_salt += 1;
                    let salt = self.result_salt;
                    if let Some(c) = self.console_by_id(console) {
                        apply_execution(c, results, error, elapsed, salt);
                    }
                    self.attach_edit_targets(console);
                    self.apply_demo_edit(console);
                }
                Response::TableOperation { request_id, result } => self.table_operation_received(request_id, result),
                Response::TableArtifact { result } => match result { Ok(path) => self.info(trf("Saved to {path}", &[("path", &path.to_string_lossy())])), Err(error) => self.error(error) },
                Response::ObjectInfo { conn, request_id, result } => self.object_browser.receive(conn, request_id, result.map(|info| *info)),
                Response::ObjectCatalog { conn, request_id, result } => self.object_browser.receive_catalog(conn, request_id, result),
                Response::SchemaSearch { request_id, result } => self.object_browser.schema_search.receive(request_id, result),
                Response::InspectTable { console, generation, page_rows, result } => {
                    if let Some(index) = self.consoles.iter().position(|c| c.id == console) {
                        let mut reload = false;
                        if let Some(t) = self.consoles[index].table.as_mut().filter(|t| t.info_generation == generation) {
                            t.info_loading = false;
                            let was_counting = t.counting_page;
                            if t.counting_page {
                                t.counting_page = false;
                                if let Some(rows) = page_rows && let Ok(rows) = usize::try_from(rows) {
                                        t.offset = rows.saturating_sub(1) / t.page_size.max(1) * t.page_size.max(1); t.page_input_reset(); reload = true;
                                }
                            }
                            t.info = Some(if was_counting && result.is_ok() && !reload { Err(tr("Exact row count unavailable").into()) } else { result.map(|info| *info) });
                        }
                        if reload { self.reload_table(index); }
                    }
                }
                Response::Backup { conn, id, request, result } => {
                    if let Ok(bytes) = &result {
                        let destination = match self.conn(conn).map(|c| c.cfg.driver) {
                            Some(Driver::Oracle) => format!("{}:{}", request.directory, request.destination),
                            Some(Driver::Snowflake) => format!("{}.{}.{}", request.db, request.directory, request.destination),
                            _ => request.destination.clone(),
                        };
                        self.explorer.backups.push(crate::db::backup::BackupRecord { conn, db: request.db.clone(), destination, mode: request.mode, created: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(), bytes: *bytes });
                        if self.explorer.backups.len() > 500 { self.explorer.backups.remove(0); }
                    }
                    if let Some(dialog) = self.backup.as_mut().filter(|d| d.id == id) { dialog.completed(&result); }
                }
                Response::ChangesApplied { console, result } => self.changes_applied(console, result),
                Response::DdlDone { conn, result } => self.ddl_done(conn, result),
                Response::RoutineSaved { console, result, identity } => {
                    let saved = result.is_ok();
                    self.routine_tab_saved(console, result);
                    if saved && let Some(identity) = identity
                        && let Some(editor) = self.consoles.iter_mut().find(|c| c.id == console).and_then(|c| c.routine.as_mut())
                    { editor.reference = identity; }
                }
                Response::ViewDefinition { conn, request_id, result } => self.view_definition_received(conn, request_id, result),
                Response::RoutinesLoaded { conn, db, result } => self.routines_loaded(conn, db, result),
                Response::RoutineDefinition { conn, request_id, result } => self.routine_definition_received(conn, request_id, result.map(|d| *d)),
                Response::TableDetails { request_id, result } => self.modify_loaded(request_id, result.map(|d| *d)),
                Response::TableCopied { request_id, result } => self.copy_done(request_id, result),
                Response::Users { conn, result } => self.users_response(conn, result),
                Response::DatabaseChoices { conn, result } => {
                    if let Some(d) = self.create.as_mut().filter(|d| d.conn == conn && matches!(d.target, CreateTarget::Database)) {
                        match result {
                            Ok(choices) => d.set_choices(choices),
                            // Without the lists the dialog still works with the server's defaults.
                            Err(e) => {
                                d.set_choices(Default::default());
                                d.error = Some(e);
                            }
                        }
                    }
                }
            }
        }
    }

    /// New database list after a refresh: already loaded schemas are kept while they are
    /// requested again.
    fn refreshed(&mut self, conn: ConnId, mut databases: Vec<DatabaseInfo>) {
        let Some(c) = self.conn_mut(conn) else { return };
        let mut reload = Vec::new();
        for d in &mut databases {
            if d.schemas.is_some() {
                continue;
            }
            if let Some(old) = c.databases.iter_mut().find(|o| o.name == d.name)
                && let Some(schemas) = old.schemas.take()
            {
                d.schemas = Some(schemas);
                reload.push(d.name.clone());
            }
        }
        c.databases = databases;
        c.db_errors.clear();
        let names: HashSet<String> = c.databases.iter().map(|d| d.name.clone()).collect();
        c.loading.retain(|d| names.contains(d));
        for db in &reload {
            c.loading.remove(db);
        }
        for db in reload {
            self.load_database(conn, db);
        }
        self.recheck_edit_targets(conn);
        self.info(tr("Schema refreshed"));
    }

    /// Decides again whether a connection's read-only results are editable
    /// (e.g. when its database schema arrives).
    fn recheck_edit_targets(&mut self, conn: ConnId) {
        let ids: Vec<u64> = self.consoles.iter().filter(|c| c.conn == Some(conn)).map(|c| c.id).collect();
        for id in ids {
            if let Some(c) = self.console_by_id(id) {
                for v in c.results.iter_mut().filter(|v| v.edit.is_none()) {
                    v.edit_checked = false;
                }
            }
            self.attach_edit_targets(id);
        }
    }

    /// Decides which of a tab's new results can be edited.
    fn attach_edit_targets(&mut self, console: u64) {
        let Some(idx) = self.consoles.iter().position(|c| c.id == console) else { return };
        let c = &self.consoles[idx];
        let mut targets = Vec::new();
        for (i, v) in c.results.iter().enumerate() {
            if v.edit_checked || v.rs.is_command() {
                continue;
            }
            let read_only = c.conn.and_then(|id| self.conn(id)).is_some_and(|cs| cs.cfg.read_only);
            let driver = c.conn.and_then(|id| self.conn(id)).map(|cs| cs.cfg.driver);
            // ClickHouse: existing rows are read-only (mutations), but inserts still work.
            let insert_only = driver == Some(Driver::ClickHouse);
            let nosql = driver.is_some_and(|d| d.is_nosql());
            let target = match (c.conn, &c.table) {
                (None, _) => Err(tr("No connection").to_string()),
                _ if read_only => Err(tr("Read-only connection").to_string()),
                // Points are written with line protocol, which the data editor doesn't produce, and
                // Kusto ingests rows rather than editing them.
                _ if matches!(driver, Some(Driver::InfluxDb | Driver::Kusto)) => Err(tr("Editing is not available for this engine").to_string()),
                // Redis and MongoDB: only a key group or a collection opened in the data editor.
                (Some(_), None) if nosql => Err(tr("Only data opened from the explorer can be edited on this engine").to_string()),
                (Some(conn), Some(t)) => match self.table_info(conn, c.db.as_deref(), &t.schema, &t.table) {
                    Some(info) if driver == Some(Driver::MongoDb) => {
                        editing::document_target(&v.rs.columns, &t.schema, info.kind, &info.name, &info.columns)
                    }
                    Some(info) => editing::target_for(&v.rs.columns, &t.schema, info.kind, &info.name, &info.columns),
                    None => Err(tr("Table structure not loaded yet").to_string()),
                },
                (Some(conn), None) => match self.conn(conn).and_then(|cs| cs.schemas(c.db.as_deref())) {
                    Some(schemas) => editing::console_target(&v.rs.statement, &v.rs.columns, schemas),
                    None => Err(tr("Database schema not loaded yet").to_string()),
                },
            };
            targets.push((i, target.map(|t| crate::ui::editing::EditTarget { insert_only, ..t })));
        }
        let c = &mut self.consoles[idx];
        for (i, t) in targets {
            if let Some(v) = c.results.get_mut(i) {
                v.set_edit_target(t);
            }
        }
    }

    /// Demo mode: simulated changes (never saved) on the first data editor.
    fn apply_demo_edit(&mut self, console: u64) {
        // DATACRAFT_DEMO_MASK_EXPORT=1: "Export with Masking…" on the first table's data.
        if self.mask_export.is_none()
            && std::env::var("DATACRAFT_DEMO").is_ok_and(|v| !v.is_empty())
            && std::env::var("DATACRAFT_DEMO_MASK_EXPORT").is_ok_and(|v| v == "1")
            && let Some(idx) = self.consoles.iter().position(|c| c.id == console && c.table.is_some())
        {
            self.open_mask_export(idx);
        }
        let Some(kind) = self.demo_edit else { return };
        let Some(idx) = self.consoles.iter().position(|c| c.id == console && c.table.is_some()) else { return };
        let Some(v) = self.consoles[idx].results.last_mut() else { return };
        if v.edit.is_none() {
            return;
        }
        self.demo_edit = None;
        self.active = idx;
        let n_cols = v.rs.columns.len();
        let last = n_cols.saturating_sub(1);
        if kind == DemoEdit::Cell {
            v.begin_edit(1, last);
            return;
        }
        if let Some(e) = v.edit.as_mut() {
            let t = e.target.clone();
            e.state.set_value(&v.rs, RowRef::Existing(0), last, Some("edited".into()));
            e.state.set_value(&v.rs, RowRef::Existing(2), 2.min(last), None);
            e.state.set_value(&v.rs, RowRef::Existing(4), last, Some("new note".into()));
            let n = e.state.add_row(n_cols);
            e.state.set_value(&v.rs, RowRef::New(n), last, Some("new row".into()));
            e.state.delete_rows(&v.rs, &t, &[RowRef::Existing(3)]);
        }
        if kind == DemoEdit::Preview {
            let vi = self.consoles[idx].results.len() - 1;
            self.open_preview(idx, vi);
        }
    }

    // ---------------------------------------------------------------- data editing

    /// Opens the preview window with the SQL of the pending changes.
    fn open_preview(&mut self, idx: usize, view: usize) {
        self.prepare_changes(idx, view, true);
    }

    /// Saves the pending changes without showing the SQL (that's what Preview is for).
    fn submit_now(&mut self, idx: usize, view: usize) {
        if !self.prepare_changes(idx, view, false) { return; }
        match self.preview.as_ref() {
            Some(pv) if !pv.visible => match pv.error.clone() {
                Some(err) => {
                    self.preview = None;
                    self.error(err);
                }
                None => {
                    self.submit_preview();
                    // If it could not be sent (no connection, busy tab), report it and discard.
                    if let Some(pv) = self.preview.as_ref().filter(|p| !p.visible && !p.applying) {
                        let err = pv.error.clone().unwrap_or_default();
                        self.preview = None;
                        self.error(err);
                    }
                }
            },
            _ => {}
        }
    }

    fn prepare_changes(&mut self, idx: usize, view: usize, visible: bool) -> bool {
        if self.preview.as_ref().is_some_and(|p| p.applying) {
            self.error(tr("Another save is in progress; try again when it finishes"));
            return false;
        }
        let Some(c) = self.consoles.get_mut(idx) else { return false };
        let Some(v) = c.results.get_mut(view) else { return false };
        v.finish_inline_edit();
        let c = &self.consoles[idx];
        let v = &c.results[view];
        let Some(e) = v.edit.as_ref() else { return false };
        if !e.state.is_dirty() {
            return false;
        }
        let driver = c.conn.and_then(|id| self.conns.iter().find(|x| x.cfg.id == id)).map(|x| x.cfg.driver).unwrap_or(Driver::Postgres);
        // The NoSQL commands need types: each field's sampled type (MongoDB), each key's (Redis).
        let types: HashMap<String, String> = match driver {
            Driver::MongoDb => c
                .conn
                .and_then(|conn| self.table_columns(conn, c.db.as_deref(), &e.target.schema, &e.target.table))
                .unwrap_or_default()
                .iter()
                .map(|col| (col.name.clone(), col.data_type.clone()))
                .collect(),
            Driver::Redis => {
                let col = |name: &str| v.rs.columns.iter().position(|c| c == name);
                match (col("key"), col("type")) {
                    (Some(k), Some(t)) => v.rs.rows.iter().filter_map(|r| Some((r.get(k)?.clone()?, r.get(t)?.clone()?))).collect(),
                    _ => HashMap::new(),
                }
            }
            _ => HashMap::new(),
        };
        let built = e.state.build_changes(&v.rs, &e.target).and_then(|changes| {
            edit::change_statements(driver, &e.target.schema, &e.target.table, &changes, &types)
        });
        let (statements, error) = match built {
            Ok(statements) => (statements, None),
            Err(err) => (Vec::new(), Some(err)),
        };
        self.preview = Some(PreviewState { console: c.id, view, statements, error, applying: false, visible });
        true
    }

    /// Sends the preview's changes in a transaction.
    fn submit_preview(&mut self) {
        let Some(pv) = self.preview.as_ref() else { return };
        if pv.applying || pv.statements.is_empty() {
            return;
        }
        let (console, statements) = (pv.console, pv.statements.clone());
        let Some(c) = self.consoles.iter().find(|c| c.id == console) else {
            self.preview = None;
            return;
        };
        let (conn, db, busy) = (c.conn, c.db.clone(), c.is_busy());
        let connected = conn.and_then(|id| self.conn(id)).is_some_and(|c| matches!(c.status, ConnStatus::Connected));
        let read_only = conn.and_then(|id| self.conn(id)).is_some_and(|c| c.cfg.read_only);
        let Some(pv) = self.preview.as_mut() else { return };
        match conn {
            _ if read_only => pv.error = Some(tr("Read-only connection").into()),
            _ if busy => pv.error = Some(tr("The tab is running another operation").into()),
            Some(conn) if connected => {
                pv.applying = true;
                pv.error = None;
                if let Some(c) = self.console_by_id(console) {
                    c.running = true;
                    c.run_started = Some(Instant::now());
                }
                self.worker.send(Request::ApplyChanges { conn, db, console, statements });
            }
            _ => pv.error = Some(tr("The connection is not open; connect it and try again").into()),
        }
    }

    fn changes_applied(&mut self, console: u64, result: Result<u64, String>) {
        let preview_view = self.preview.as_ref().filter(|p| p.console == console).map(|p| p.view);
        let Some(idx) = self.consoles.iter().position(|c| c.id == console) else {
            self.preview = None;
            return;
        };
        let c = &mut self.consoles[idx];
        c.running = false;
        c.run_started = None;
        match result {
            Ok(n) => {
                if preview_view.is_some() {
                    self.preview = None;
                }
                let msg = if n == 1 { tr("1 row modified").to_string() } else { trf("{n} rows modified", &[("n", &n)]) };
                c.log.push(OutputEntry { time: now(), statement: String::new(), message: msg.clone(), is_error: false });
                let vi = preview_view.or_else(|| c.edit_view_index());
                if let Some(v) = vi.and_then(|i| c.results.get_mut(i)) {
                    v.mark_saved();
                }
                self.info(msg);
                // Reloads that tab's data.
                let c = &mut self.consoles[idx];
                if c.table.is_some() {
                    c.replace_view = vi;
                    self.reload_table(idx);
                } else if let Some(i) = vi
                    && let Some(stmt) = c.results.get(i).map(|v| v.rs.statement.clone())
                {
                    c.replace_view = Some(i);
                    self.run_statements(idx, vec![stmt]);
                }
            }
            Err(e) => {
                c.log.push(OutputEntry {
                    time: now(),
                    statement: String::new(),
                    message: trf("Changes were not saved: {error}", &[("error", &e)]),
                    is_error: true,
                });
                match self.preview.as_mut().filter(|p| p.console == console && p.visible) {
                    Some(pv) => {
                        pv.applying = false;
                        pv.error = Some(e);
                    }
                    None => {
                        if self.preview.as_ref().is_some_and(|p| p.console == console) {
                            self.preview = None;
                        }
                        self.error(trf("Changes were not saved: {error}", &[("error", &e)]));
                    }
                }
            }
        }
    }

    /// Closes a console's result tabs (asks for confirmation if they have changes).
    fn close_results(&mut self, idx: usize, which: CloseResults) {
        let Some(c) = self.consoles.get(idx).filter(|c| c.table.is_none() && !c.is_busy()) else { return };
        if self.guard(Discard::CloseResults(c.id, which)) {
            self.close_results_now(idx, which);
        }
    }

    fn close_results_now(&mut self, idx: usize, which: CloseResults) {
        let Some(c) = self.consoles.get_mut(idx).filter(|c| c.table.is_none()) else { return };
        let id = c.id;
        let removed = c.close_results(which);
        // The changes preview keeps the index of its view.
        if let Some(pv) = self.preview.as_mut().filter(|p| p.console == id) {
            match console::remap_index(pv.view, &removed) {
                Some(v) => pv.view = v,
                None => self.preview = None,
            }
        }
    }

    /// true if the action can go ahead now; with unsaved changes, asks for confirmation.
    fn guard(&mut self, action: Discard) -> bool {
        let closing = match action {
            Discard::Close(id) => self.consoles.iter().position(|c| c.id == id).into_iter().collect(),
            Discard::CloseTabs(id, which) => self.tab_close_indices(id, which),
            Discard::CloseAll => (0..self.consoles.len()).collect(),
            _ => Vec::new(),
        };
        if closing.iter().any(|&i| self.consoles[i].routine.as_ref().is_some_and(|r| r.dialog.running)) { return false; }
        let dirty = match action {
            Discard::CloseTabs(id, which) => self.tab_close_indices(id, which).iter().any(|&i| self.consoles[i].has_pending_edits()),
            Discard::CloseAll => self.consoles.iter().any(|c| c.has_pending_edits()),
            Discard::Close(id) | Discard::Reload(id) | Discard::Run(id, _) | Discard::ApplyFilter(id) | Discard::Page(id, _) => {
                self.consoles.iter().any(|c| c.id == id && c.has_pending_edits())
            }
            Discard::CloseResults(id, which) => self.consoles.iter().any(|c| c.id == id && c.closing_dirty(which)),
        };
        if dirty {
            self.confirm_discard = Some(action);
        }
        !dirty
    }

    /// Runs a confirmed action, discarding the pending changes.
    fn discard_and_do(&mut self, action: Discard) {
        match action {
            Discard::ApplyFilter(id) => {
                if let Some(i) = self.consoles.iter().position(|c| c.id == id) { self.consoles[i].discard_edits(); self.apply_filter_now(i); }
            }
            Discard::Page(id, page) => {
                if let Some(i) = self.consoles.iter().position(|c| c.id == id) { self.consoles[i].discard_edits(); self.navigate_page(i, page); }
            }
            Discard::CloseTabs(id, which) => self.close_tabs_now(id, which),
            Discard::CloseAll => {
                self.consoles.clear();
                self.active = 0;
                self.object_browser.scope = None;
                self.object_browser.active = false;
            }
            Discard::Close(id) => {
                if let Some(i) = self.consoles.iter().position(|c| c.id == id) {
                    self.close_console_now(i);
                }
            }
            Discard::CloseResults(id, which) => {
                if let Some(i) = self.consoles.iter().position(|c| c.id == id) {
                    self.close_results_now(i, which);
                }
            }
            Discard::Reload(id) | Discard::Run(id, _) => {
                let Some(i) = self.consoles.iter().position(|c| c.id == id) else { return };
                self.consoles[i].discard_edits();
                match action {
                    Discard::Run(_, all) => self.run_console(i, all),
                    _ => self.reload_table(i),
                }
            }
        }
    }

    // ---------------------------------------------------------------- execution

    fn run_console(&mut self, idx: usize, all: bool) {
        if let Some(reference) = self.consoles.get(idx).and_then(|c| c.routine.as_ref()).map(|r| r.reference.clone()) { self.open_execute_routine(reference); return; }
        let Some(console) = self.consoles.get(idx) else { return };
        let id = console.id;
        if console.is_busy() {
            return;
        }
        if console.table.is_some() {
            if self.guard(Discard::Reload(id)) {
                self.reload_table(idx);
            }
            return;
        }
        if !self.guard(Discard::Run(id, all)) {
            return;
        }
        let Some(console) = self.consoles.get(idx) else { return };
        let conn_id = console.conn;
        let driver = conn_id.and_then(|id| self.conn(id)).map(|c| c.cfg.driver).unwrap_or(Driver::Postgres);
        let statements: Vec<String> = if all {
            sql::split_batches(&console.text, driver).into_iter().map(|s| s.text).collect()
        } else if let Some(sel) = console.selection() {
            sql::split_batches(sel, driver).into_iter().map(|s| s.text).collect()
        } else {
            sql::statement_at(&console.text, console.cursor_byte(), driver).into_iter().map(|s| s.text).collect()
        };
        self.run_statements(idx, statements);
    }

    /// Sends statements of tab `idx`, connecting first if needed.
    fn run_statements(&mut self, idx: usize, statements: Vec<String>) {
        let Some(console) = self.consoles.get(idx) else { return };
        if console.is_busy() {
            return;
        }
        let conn_id = console.conn;
        let status = conn_id.and_then(|id| self.conn(id)).map(|c| c.status.clone());
        let max_rows = self.max_rows;

        let Some(console) = self.consoles.get_mut(idx) else { return };
        if statements.is_empty() {
            console.log.push(OutputEntry { time: now(), statement: String::new(), message: tr("Nothing to run").into(), is_error: true });
            console.result_tab = ResultTab::Output;
            return;
        }
        let (Some(conn), Some(status)) = (conn_id, status) else {
            console.log.push(OutputEntry {
                time: now(),
                statement: String::new(),
                message: tr("Select a connection for the console").into(),
                is_error: true,
            });
            console.result_tab = ResultTab::Output;
            return;
        };
        let driver = self.conn(conn).map(|c| c.cfg.driver).unwrap_or(Driver::Postgres);
        if self.conn(conn).is_some_and(|c| c.cfg.read_only)
            && let Some(stmt) = statements
                .iter()
                .flat_map(|s| sql::split_statements(s, driver))
                .map(|s| s.text)
                .find(|s| !is_read_statement(driver, s))
        {
            let Some(console) = self.consoles.get_mut(idx) else { return };
            console.log.push(OutputEntry {
                time: now(),
                statement: stmt.trim().to_string(),
                message: tr("Read-only connection: only queries can run (SELECT, WITH, SHOW, EXPLAIN…)").into(),
                is_error: true,
            });
            console.result_tab = ResultTab::Output;
            return;
        }
        let Some(console) = self.consoles.get_mut(idx) else { return };
        console.last_statements = statements.clone();
        console.run_started = Some(Instant::now());
        match status {
            ConnStatus::Connected => {
                console.running = true;
                let id = console.id;
                let db = console.db.clone();
                self.worker.send(Request::Execute { conn, db, console: id, statements, max_rows });
            }
            ConnStatus::Connecting => console.pending = Some(statements),
            ConnStatus::Disconnected | ConnStatus::Error(_) => {
                console.pending = Some(statements);
                self.connect(conn);
            }
        }
    }

    fn sync_save_on_row_change(&mut self) {
        if let Some(value) = self.consoles.get(self.active).and_then(|c| c.table.as_ref()).map(|t| t.save_on_row_change)
            && value != self.save_on_row_change
        {
            self.save_on_row_change = value;
            for tab in self.consoles.iter_mut().filter_map(|c| c.table.as_mut()) { tab.save_on_row_change = value; }
        }
    }

    /// Opens (or activates) a table's data editor.
    fn open_table(&mut self, t: &TableRef) {
        self.object_browser.active = false;
        let default_db = self.conn(t.conn).and_then(|c| c.default_db()).map(str::to_string);
        if let Some(i) = self.consoles.iter().position(|c| {
            c.conn == Some(t.conn)
                && c.db.as_ref().or(default_db.as_ref()) == Some(&t.db)
                && c.table.as_ref().is_some_and(|x| x.schema == t.schema && x.table == t.table)
        }) {
            self.active = i;
            // With pending changes it is only activated (reloading would discard them).
            if !self.consoles[i].has_pending_edits() && !self.consoles[i].is_busy() {
                self.reload_table(i);
            }
            return;
        }
        let id = self.alloc_console_id();
        self.consoles.push(Console::new_table(id, t.conn, Some(t.db.clone()), t.schema.clone(), t.table.clone()));
        if let Some(t) = self.consoles.last_mut().and_then(|c| c.table.as_mut()) { t.save_on_row_change = self.save_on_row_change; }
        self.active = self.consoles.len() - 1;
        self.reload_table(self.active);
    }

    /// Reloads the data editor with its filters, limiting rows in the SQL itself.
    fn apply_filter_now(&mut self, index: usize) {
        if let Some(table) = self.consoles.get_mut(index).and_then(|c| c.table.as_mut())
            && let Some((where_clause, order_by)) = table.filter_sort.pending.take() {
            table.where_clause = where_clause; table.order_by = order_by; table.offset = 0; table.page_input_reset();
            self.reload_table(index);
        }
    }

    fn navigate_page(&mut self, index: usize, page: console::PageMove) {
        let Some(c) = self.consoles.get(index) else { return; };
        if c.is_busy() { return; }
        if page == console::PageMove::Last { self.request_table_details(index, true, true); return; }
        if let console::PageMove::Size(size) = page { self.max_rows = size; }
        let Some(t) = self.consoles.get_mut(index).and_then(|c| c.table.as_mut()) else { return; };
        let offset = match page {
            console::PageMove::First | console::PageMove::Size(_) => Some(0),
            console::PageMove::Previous => Some(t.offset.saturating_sub(t.page_size)),
            console::PageMove::Next => t.offset.checked_add(t.page_size),
            console::PageMove::Number(n) => n.checked_sub(1).and_then(|n| n.checked_mul(t.page_size)),
            console::PageMove::Last => None,
        };
        if let Some(offset) = offset { t.offset = offset; t.page_input_reset(); self.reload_table(index); }
        else { self.error(tr("Page number is too large")); }
    }

    fn request_table_details(&mut self, index: usize, count_rows: bool, last_page: bool) {
        let Some(c) = self.consoles.get(index) else { return; };
        let (Some(conn), Some(t)) = (c.conn, c.table.as_ref()) else { return; };
        let Some(source) = self.conn(conn).filter(|c| c.status == ConnStatus::Connected && !c.cfg.driver.is_nosql()) else { return; };
        let db = source.resolve_db(c.db.as_deref()).unwrap_or_default().to_string();
        let request = Request::InspectTable { conn, db, schema: t.schema.clone(), table: t.table.clone(), console: c.id, generation: t.info_generation.wrapping_add(1), count_rows, page_filter: last_page.then(|| t.where_clause.clone()) };
        if let Some(t) = self.consoles[index].table.as_mut() { t.info_requested = true; t.info_generation = t.info_generation.wrapping_add(1); t.info_loading = true; t.counting_page = last_page; }
        self.worker.send(request);
    }

    fn reload_table(&mut self, idx: usize) {
        let Some(c) = self.consoles.get(idx) else { return };
        let (Some(conn), Some(t)) = (c.conn, c.table.as_ref()) else { return };
        let Some(driver) = self.conn(conn).map(|c| c.cfg.driver) else { return };
        let order = if t.order_by.trim().is_empty() && !driver.is_nosql() {
            self.table_columns(conn, c.db.as_deref(), &t.schema, &t.table).unwrap_or(&[]).iter().filter(|c| c.primary_key).map(|c| driver.quote_ident(&c.name)).collect::<Vec<_>>().join(", ")
        } else { t.order_by.clone() };
        let sql = table_query_page(driver, &t.schema, &t.table, &t.where_clause, &order, self.max_rows, t.offset);
        if let Some(t) = self.consoles.get_mut(idx).and_then(|c| c.table.as_mut()) {
            t.loaded = true;
            t.page_size = self.max_rows;
        }
        self.run_statements(idx, vec![sql]);
    }

    fn generate(&mut self, ctx: &egui::Context, t: &TableRef, kind: Generate) {
        // Read catalog DDL when available; otherwise preserve generation from cached columns.
        if kind == Generate::Ddl && self.request_view_ddl(t) { return; }
        let Some(c) = self.conn(t.conn) else { return };
        let driver = c.cfg.driver;
        if kind == Generate::Ddl && c.status == ConnStatus::Connected && crate::db::table_definition::supported(driver) {
            self.handle_table_action(ctx, ExplorerAction::GenerateDdl(t.clone()));
            return;
        }
        let Some(columns) = self.table_columns(t.conn, Some(&t.db), &t.schema, &t.table) else {
            return;
        };
        let cols: Vec<String> = columns.iter().map(|c| quote_ident_if_needed(driver, &c.name)).collect();
        let table = Session::quote_table(driver, &t.schema, &t.table);
        let text = match kind {
            Generate::Insert => {
                let values: Vec<&str> = cols.iter().map(|_| "NULL").collect();
                format!("INSERT INTO {table} ({})\nVALUES ({});", cols.join(", "), values.join(", "))
            }
            Generate::Select if cols.is_empty() => format!("SELECT *\nFROM {table};"),
            Generate::Select => format!("SELECT {}\nFROM {table};", cols.join(", ")),
            Generate::Ddl => ddl(driver, &table, columns),
        };
        let default_db = c.default_db().map(str::to_string);
        let target = self
            .consoles
            .get(self.active)
            .filter(|c| {
                c.conn == Some(t.conn) && c.table.is_none() && c.db.as_ref().or(default_db.as_ref()) == Some(&t.db)
            })
            .map(|_| self.active);
        match target {
            Some(i) => self.consoles[i].append_text(ctx, &text),
            None => {
                let title = (kind == Generate::Ddl).then(|| format!("{}.sql", t.table));
                let i = self.new_console(Some(t.conn), title, text);
                self.consoles[i].db = Some(t.db.clone());
            }
        }
    }

    fn handle_explorer(&mut self, ctx: &egui::Context, actions: Vec<ExplorerAction>) {
        for a in actions {
            // SQL-only actions never reach Redis or MongoDB (other menus may still offer them).
            let sql_only = match &a {
                ExplorerAction::NewDatabase(id) | ExplorerAction::DropDatabase(id, _) => Some(*id),
                ExplorerAction::NewTable { conn, .. } | ExplorerAction::NewView { conn, .. } => Some(*conn),
                ExplorerAction::Backup(id, _) => Some(*id),
                ExplorerAction::DropObject(t, _)
                | ExplorerAction::TruncateTable(t)
                | ExplorerAction::EmptyTable(t)
                | ExplorerAction::ImportData(t)
                | ExplorerAction::RenameTable(t)
                | ExplorerAction::ModifyTable(t)
                | ExplorerAction::GenerateSelect(t)
                | ExplorerAction::GenerateInsert(t)
                | ExplorerAction::EditView(t) => Some(t.conn),
                _ => None,
            };
            // Copying works with MongoDB collections too, but not with Redis keys, InfluxDB
            // measurements nor Kusto tables.
            let not_copyable = match &a {
                ExplorerAction::CopyTable(t) => self.conn(t.conn).is_some_and(|c| !c.cfg.driver.can_copy()),
                _ => false,
            };
            if not_copyable || sql_only.and_then(|id| self.conn(id)).is_some_and(|c| c.cfg.driver.is_nosql()) {
                self.error(tr("Not available for this engine").to_string());
                continue;
            }
            match a {
                ExplorerAction::Backup(conn, db) => {
                    if let Some(c) = self.conn(conn).filter(|c| c.status == ConnStatus::Connected && !c.cfg.driver.is_nosql()) {
                        let mut dialog = crate::ui::backup_dialog::BackupDialog::new(conn, c.cfg.driver, db, c.cfg.read_only);
                        dialog.request.trust_cert = c.cfg.trust_cert;
                        self.backup = Some(dialog);
                    }
                }
                ExplorerAction::RunScript(conn, db) => self.run_script(conn, db),
                ExplorerAction::NewConnection => self.open_new_connection_dialog(),
                ExplorerAction::Connect(id) => self.connect(id),
                ExplorerAction::Disconnect(id) => self.disconnect(id),
                ExplorerAction::Refresh(id) => self.worker.send(Request::Refresh(id)),
                ExplorerAction::Edit(id) => {
                    if self.conn(id).is_some() {
                        self.dialog = Some(ConnectionDialog::edit(id, &self.conn_configs()));
                    }
                }
                ExplorerAction::Duplicate(id) => {
                    if let Some(c) = self.conn(id) {
                        let mut cfg = c.cfg.clone();
                        cfg.id = self.next_conn_id();
                        cfg.name = trf("{name} (copy)", &[("name", &cfg.name)]);
                        config::store_secrets(&cfg);
                        self.conns.push(ConnState::new(cfg));
                        self.persist_connections();
                    }
                }
                ExplorerAction::Delete(id) => self.confirm_delete = Some(id),
                ExplorerAction::LoadDatabase(id, db) => self.load_database(id, db),
                ExplorerAction::NewDatabase(id) => {
                    if self.refuse_read_only(id) {
                        continue;
                    }
                    if let Some(c) = self.conn(id).filter(|c| c.status == ConnStatus::Connected) {
                        self.create = Some(CreateDialog::database(id, c.cfg.driver));
                        self.worker.send(Request::DatabaseChoices { conn: id });
                    }
                }
                ExplorerAction::NewTable { conn, db, schema } => {
                    if !self.refuse_read_only(conn) {
                        self.open_new_table(conn, db, schema);
                    }
                }
                ExplorerAction::NewView { conn, db, schema } => self.open_new_view(conn, db, schema),
                ExplorerAction::EditView(t) => self.open_edit_view(t),
                ExplorerAction::NewRoutine { conn, db, schema, kind } => self.open_new_routine(conn, db, schema, kind),
                ExplorerAction::EditRoutine(r) => self.open_edit_routine(r),
                ExplorerAction::ExecuteRoutine(r) => self.open_execute_routine(r),
                ExplorerAction::RoutineDdl(r) => self.request_routine_ddl(r),
                ExplorerAction::DropRoutine(r) => self.open_drop_routine(r),
                ExplorerAction::DropDatabase(id, db) => {
                    if self.refuse_read_only(id) {
                        continue;
                    }
                    if let Some(c) = self.conn(id).filter(|c| c.status == ConnStatus::Connected) {
                        self.drop = Some(DropDialog::database(id, c.cfg.driver, db));
                    }
                }
                ExplorerAction::DropObject(t, kind) => self.open_object_dialog(t, kind.into()),
                ExplorerAction::TruncateTable(t) => self.open_object_dialog(t, DropKind::Truncate),
                ExplorerAction::EmptyTable(t) => self.open_object_dialog(t, DropKind::Empty),
                ExplorerAction::ImportData(t) => self.open_import(t),
                ExplorerAction::CopyTable(t) => self.open_copy(t),
                ExplorerAction::RenameTable(t) => self.open_object_dialog(t, DropKind::Rename),
                ExplorerAction::ModifyTable(t) => self.open_modify_table(t),
                ExplorerAction::NewConsole(id, db) => {
                    let i = self.new_console(Some(id), None, String::new());
                    self.consoles[i].db = db;
                    if self.conn(id).is_some_and(|c| matches!(c.status, ConnStatus::Disconnected | ConnStatus::Error(_))) {
                        self.connect(id);
                    }
                }
                // A single click in the explorer updates the Objects tab, but only brings it to
                // the front if it already is (or there is no other tab): it doesn't hide the
                // console or data editor being worked on.
                ExplorerAction::BrowseObjects(scope) => {
                    self.explorer.selected_table = None;
                    let focus = self.object_browser.active || self.consoles.is_empty();
                    if self.object_browser.open(scope) { self.load_object_catalog(); }
                    self.object_browser.active = focus;
                }
                ExplorerAction::BrowseRoutines(scope, kind) => {
                    self.explorer.selected_table = None;
                    let focus = self.object_browser.active || self.consoles.is_empty();
                    self.object_browser.open_routines(scope, kind);
                    self.object_browser.active = focus;
                }
                ExplorerAction::InspectObject(t, kind) => {
                    let focus = self.object_browser.active || self.consoles.is_empty();
                    let changed = self.object_browser.open(crate::ui::object_browser::ObjectScope {
                        conn: t.conn, db: t.db.clone(), schema: Some(t.schema.clone()), kind: Some(kind),
                    });
                    if changed { self.load_object_catalog(); }
                    self.object_browser.active = focus;
                    self.inspect_object(t, false);
                }
                ExplorerAction::OpenTable(t) => self.open_table(&t),
                ExplorerAction::GenerateSelect(t) => self.generate(ctx, &t, Generate::Select),
                ExplorerAction::GenerateInsert(t) => self.generate(ctx, &t, Generate::Insert),
                ExplorerAction::GenerateDdl(t) => self.generate(ctx, &t, Generate::Ddl),
                ExplorerAction::LoadUsers(id) => self.load_users(id),
                ExplorerAction::NewUser(id) => self.open_user_dialog(id, None, false),
                ExplorerAction::EditUser(id, account) => self.open_user_dialog(id, Some(account), false),
                ExplorerAction::DropUser(id, account) => self.open_user_dialog(id, Some(account), true),
                ExplorerAction::Hide => self.show_explorer = false,
                other => self.handle_table_action(ctx, other),
            }
        }
    }

    fn load_object_catalog(&mut self) {
        let Some(scope) = self.object_browser.scope.clone() else {
            return;
        };
        if self.conn(scope.conn).is_some_and(|c| {
            c.status == ConnStatus::Connected && c.schemas(Some(&scope.db)).is_none()
        }) {
            return;
        }
        let id = self.object_browser.start_catalog_request();
        if self
            .conn(scope.conn)
            .is_some_and(|c| c.status == ConnStatus::Connected)
        {
            self.worker.send(Request::ObjectCatalog {
                conn: scope.conn,
                db: scope.db,
                schema: scope.schema,
                request_id: id,
            });
        } else {
            self.object_browser.receive_catalog(
                scope.conn,
                id,
                Err(tr("Connect to load current object information").into()),
            );
        }
    }

    fn inspect_object(&mut self, table: TableRef, count_rows: bool) {
        let connected = self
            .conn(table.conn)
            .is_some_and(|c| c.status == ConnStatus::Connected);
        let request_id = self.object_browser.start_request(table.clone());
        self.explorer.selected_table = Some(table.clone());
        self.explorer.selected = Some(explorer::table_key(&table));
        if connected {
            self.worker.send(Request::ObjectInfo {
                conn: table.conn,
                db: table.db,
                schema: table.schema,
                table: table.table,
                request_id,
                count_rows,
            });
        } else {
            self.object_browser.receive(
                table.conn,
                request_id,
                Err(tr("Connect to load current object information").into()),
            );
        }
    }

    /// true (and reports it) if the connection is read-only.
    /// "Run SQL Script…": the files picked run on a connection of their own, without opening them.
    fn run_script(&mut self, conn: ConnId, db: Option<String>) {
        if self.refuse_read_only(conn) {
            return;
        }
        let Some(c) = self.conn(conn).filter(|c| c.status == ConnStatus::Connected) else { return };
        let target = match &db {
            Some(db) => format!("{} / {db}", c.cfg.name),
            None => c.cfg.name.clone(),
        };
        let Some(files) = rfd::FileDialog::new().set_title(tr("Run SQL Script")).add_filter("SQL", &["sql"]).add_filter(tr("All files"), &["*"]).pick_files() else { return };
        let run = std::sync::Arc::new(crate::db::script::ScriptRun::default());
        self.worker.send(Request::RunScript { conn, db, files: files.clone(), run: run.clone() });
        self.script = Some(crate::ui::script_dialog::ScriptDialog { conn, target, files, run });
    }

    fn refuse_read_only(&mut self, conn: ConnId) -> bool {
        let ro = self.conn(conn).is_some_and(|c| c.cfg.read_only);
        if ro {
            self.error(tr("Read-only connection").to_string());
        }
        ro
    }

    /// "Import Data from File" into a table whose structure is loaded.
    fn open_import(&mut self, t: TableRef) {
        if self.refuse_read_only(t.conn) {
            return;
        }
        let Some(driver) = self.conn(t.conn).filter(|c| c.status == ConnStatus::Connected).map(|c| c.cfg.driver) else { return };
        let Some(columns) = self.table_columns(t.conn, Some(&t.db), &t.schema, &t.table).map(<[ColumnInfo]>::to_vec) else {
            self.error(tr("Table structure not loaded yet").to_string());
            return;
        };
        self.import = Some(ImportDialog::new(t.conn, driver, t.db, t.schema, t.table, columns));
    }

    /// "Copy Table To…" from a table or view whose structure is loaded.
    fn open_copy(&mut self, t: TableRef) {
        let Some(source) = self.conn(t.conn).filter(|c| c.status == ConnStatus::Connected) else { return };
        let Some(columns) = self.table_columns(t.conn, Some(&t.db), &t.schema, &t.table).map(<[ColumnInfo]>::to_vec) else {
            self.error(tr("Table structure not loaded yet").to_string());
            return;
        };
        let d = CopyDialog::new(source, t.db, t.schema, t.table, columns, &self.conns);
        if !self.conns.iter().any(|c| c.cfg.id == d.target && c.status == ConnStatus::Connected && !c.cfg.read_only) {
            self.error(tr("Connect a connection that is not read-only to copy into").to_string());
            return;
        }
        self.copy = Some(d);
    }

    /// Checks the copy's filter (it must only read) and sends it to the worker.
    fn start_copy(&mut self, request: crate::db::copy::CopyRequest) {
        let Some(d) = self.copy.as_mut() else { return };
        let Some(driver) = self.conns.iter().find(|c| c.cfg.id == d.source).map(|c| c.cfg.driver) else { return };
        if driver == Driver::MongoDb {
            if !crate::db::copy::is_read_filter(&request.filter) {
                d.error = Some(tr("The filter must be one JSON document that only reads, e.g. {status: \"active\"}").to_string());
                return;
            }
        } else {
            let select = crate::db::copy::select_sql(driver, &request.source_schema, &request.source_table, &request.columns, &request.filter, None);
            if request.filter.contains(';') || !is_query(&select) {
                d.error = Some(tr("The WHERE condition can only read data").to_string());
                return;
            }
        }
        let mut request = request;
        if request.masks.iter().any(|m| *m != crate::db::copy::Mask::Keep) {
            request.mask_key = config::masking_key();
        }
        let cancel = std::sync::Arc::new(tokio::sync::Notify::new());
        d.started(cancel.clone());
        let (source, target, request_id) = (d.source, d.target, d.request_id);
        self.worker.send(Request::CopyTable { source, target, request_id, request, cancel });
    }

    /// Reply to a copy: on success the dialog closes and the target is reloaded.
    fn copy_done(&mut self, request_id: u64, result: Result<Vec<(String, u64)>, String>) {
        let Some(d) = self.copy.as_mut() else { return };
        if !d.finished(request_id) {
            return;
        }
        let report = match result {
            Ok(r) => r,
            Err(e) => {
                d.error = Some(e);
                return;
            }
        };
        let Some(d) = self.copy.take() else { return };
        self.load_database(d.target, d.target_db.clone());
        self.reload_table_editors(d.target, &d.target_db, &d.target_schema, &d.target_table);
        let n: u64 = report.iter().map(|(_, n)| n).sum();
        let msg = if report.len() > 1 {
            trf("{n} rows copied to {tables} tables", &[("n", &n), ("tables", &report.len())])
        } else {
            trf("{n} rows copied to “{table}”", &[("n", &n), ("table", &d.target_table)])
        };
        self.info(msg);
    }

    /// "Export with Masking…" for a console's last result.
    fn open_mask_export(&mut self, idx: usize) {
        let Some(c) = self.consoles.get(idx) else { return };
        let Some(view) = c.results.last().filter(|v| !v.rs.is_command()) else { return };
        let driver = c.conn.and_then(|id| self.conn(id)).map_or(Driver::Postgres, |x| x.cfg.driver);
        let (target, base) = match &c.table {
            Some(t) => (Session::quote_table(driver, &t.schema, &t.table), t.table.clone()),
            None => (tr("table").to_string(), tr("result").to_string()),
        };
        self.mask_export = Some(MaskExportDialog::new(c.id, view.rs.columns.clone(), c.export_format, target, base));
    }

    /// Exports or copies the masked result (a cancelled save dialog leaves it open).
    fn run_mask_export(&mut self, ctx: &egui::Context, save: bool) {
        let Some(d) = self.mask_export.take() else { return };
        let Some(view) = self.consoles.iter().find(|c| c.id == d.console).and_then(|c| c.results.last()) else { return };
        let key = if d.masks.iter().any(|m| *m != crate::db::copy::Mask::Keep) { config::masking_key() } else { String::new() };
        let masks = Some(crate::ui::results::ExportMasks { masks: &d.masks, key: &key });
        if save {
            match crate::ui::results::export_to_file(view, d.format, &d.insert_target, &d.base_name, masks) {
                Ok(true) => self.info(tr("Masked export saved")),
                Ok(false) => self.mask_export = Some(d),
                Err(e) => self.error(trf("Export failed: {error}", &[("error", &e)])),
            }
        } else {
            ctx.copy_text(crate::ui::results::format_rows_masked(view, d.format, &d.insert_target, masks));
            self.info(tr("Masked rows copied to the clipboard"));
        }
    }

    /// Demo mode: runs the import as if "Import" had been clicked.
    fn submit_import(&mut self) {
        let Some(d) = self.import.as_mut() else { return };
        let statements = d.start();
        if !statements.is_empty() {
            let (conn, db) = (d.conn, Some(d.db.clone()));
            self.worker.send(Request::RunDdl { conn, db, statements, close_db: None, transaction: true });
        }
    }

    /// Reply to an import's INSERTs.
    fn import_done(&mut self, result: Result<(), String>) {
        let Some(d) = self.import.as_mut() else { return };
        if let Err(e) = result {
            d.running = false;
            d.error = Some(e);
            return;
        }
        let Some(d) = self.import.take() else { return };
        self.reload_table_editors(d.conn, &d.db, &d.schema, &d.table);
        self.info(trf("{n} rows imported into “{table}”", &[("n", &d.imported), ("table", &d.table)]));
    }

    /// Reloads the open data editors of a table (that aren't busy and have no pending edits,
    /// which a reload would silently discard).
    fn reload_table_editors(&mut self, conn: ConnId, db: &str, schema: &str, table: &str) {
        let default_db = self.conn(conn).and_then(|c| c.default_db()).map(str::to_string);
        let editors: Vec<usize> = (0..self.consoles.len())
            .filter(|&i| {
                let c = &self.consoles[i];
                c.conn == Some(conn)
                    && c.db.as_deref().or(default_db.as_deref()) == Some(db)
                    && c.table.as_ref().is_some_and(|t| t.schema == schema && t.table == table)
                    && !c.is_busy()
                    && !c.has_pending_edits()
            })
            .collect();
        for i in editors {
            self.reload_table(i);
        }
    }

    /// Drop / truncate / rename confirmation for a table or view.
    fn open_object_dialog(&mut self, t: TableRef, kind: DropKind) {
        if self.table_has_pending_edits(&t) { self.error(tr("Save or discard this table's pending edits before changing it.")); return; }
        if self.refuse_read_only(t.conn) {
            return;
        }
        if let Some(c) = self.conn(t.conn).filter(|c| c.status == ConnStatus::Connected) {
            self.drop = Some(DropDialog::object(t.conn, c.cfg.driver, kind, t.db, t.schema, t.table));
        }
    }

    fn open_new_table(&mut self, conn: ConnId, db: String, schema: Option<String>) {
        let Some(c) = self.conn(conn).filter(|c| c.status == ConnStatus::Connected) else { return };
        let driver = c.cfg.driver;
        let schemas: Vec<String> = c.schemas(Some(&db)).unwrap_or_default().iter().map(|s| s.name.clone()).collect();
        let default = create_dialog::default_schema(driver).to_string();
        let schema = schema.or_else(|| schemas.contains(&default).then_some(default.clone())).or_else(|| schemas.first().cloned()).unwrap_or(default);
        self.ensure_db_loaded(conn, Some(&db));
        self.create = Some(CreateDialog::table(conn, driver, db, schema));
    }

    /// "Modify Table…": opens the dialog and asks the worker for the table's full structure.
    fn open_modify_table(&mut self, t: TableRef) {
        if self.table_has_pending_edits(&t) { self.error(tr("Save or discard this table's pending edits before changing it.")); return; }
        if self.refuse_read_only(t.conn) {
            return;
        }
        let Some(driver) = self.conn(t.conn).filter(|c| c.status == ConnStatus::Connected).map(|c| c.cfg.driver) else { return };
        let d = ModifyTableDialog::new(t.conn, driver, t.db.clone(), t.schema.clone(), t.table.clone());
        let request_id = d.request_id;
        self.modify = Some(d);
        self.worker.send(Request::TableDetails { conn: t.conn, db: t.db, schema: t.schema, table: t.table, request_id });
    }

    fn modify_loaded(&mut self, request_id: u64, result: Result<crate::db::alter::TableDetails, String>) {
        let Some(d) = self.modify.as_mut().filter(|d| d.request_id == request_id) else { return };
        d.loaded(result);
        let demo = std::env::var("DATACRAFT_DEMO").is_ok_and(|v| !v.is_empty());
        if demo && d.is_loaded() && std::env::var("DATACRAFT_DEMO_MODIFY").is_ok_and(|v| v == "edit") {
            d.demo_edits();
        }
    }

    /// Reply to "Modify Table…"'s statements: the table (maybe renamed) is reloaded.
    fn modify_done(&mut self, result: Result<(), String>) {
        let Some(d) = self.modify.as_mut() else { return };
        if let Err(e) = result {
            d.running = false;
            d.error = Some(e);
            // Engines without transactional DDL may have applied part of it: show the current state.
            let (conn, db) = (d.conn, d.db.clone());
            if let Some(c) = self.conn_mut(conn) {
                c.loading.remove(&db);
            }
            self.load_database(conn, db);
            return;
        }
        let Some(d) = self.modify.take() else { return };
        let conn = d.conn;
        let new_name = d.final_name();
        if new_name != d.table {
            // Data editors of the old name have nothing to show any more.
            let default_db = self.conn(conn).and_then(|c| c.default_db()).map(str::to_string);
            let gone = |c: &Console| {
                c.conn == Some(conn)
                    && c.db.as_deref().or(default_db.as_deref()) == Some(d.db.as_str())
                    && c.table.as_ref().is_some_and(|t| t.schema == d.schema && t.table == d.table)
            };
            while let Some(i) = self.consoles.iter().position(gone) {
                self.close_console_now(i);
            }
            for org in self.object_browser.organizations.iter_mut().filter(|o| o.conn == conn && o.db == d.db) {
                org.table_changed(&d.schema, &d.table, Some(&new_name));
            }
            let renamed = TableRef { conn, db: d.db.clone(), schema: d.schema.clone(), table: new_name.clone() };
            self.explorer.selected = Some(explorer::table_key(&renamed));
            self.explorer.selected_table = Some(renamed);
        } else {
            self.reload_table_editors(conn, &d.db, &d.schema, &d.table);
        }
        if let Some(c) = self.conn_mut(conn) {
            c.loading.remove(&d.db);
        }
        self.load_database(conn, d.db.clone());
        self.info(trf("Table “{name}” modified", &[("name", &new_name)]));
    }

    /// Loads (or reloads) the "Users" / "Roles" folder of a connection.
    fn load_users(&mut self, conn: ConnId) {
        let Some(c) = self.conn_mut(conn).filter(|c| c.status == ConnStatus::Connected) else { return };
        if crate::db::users::UserModel::of(c.cfg.driver).is_none() {
            return;
        }
        c.users.loading = true;
        self.worker.send(Request::Users { conn, request: crate::db::users::UsersRequest::List });
    }

    /// User editor: a new user (`account` None), an existing one, or the confirmation to drop it.
    /// Read-only connections can open the editor to look, but not create or drop.
    fn open_user_dialog(&mut self, conn: ConnId, account: Option<crate::db::users::Account>, drop: bool) {
        use crate::ui::user_dialog::UserDialog;
        if (drop || account.is_none()) && self.refuse_read_only(conn) {
            return;
        }
        let Some(c) = self.conn(conn).filter(|c| c.status == ConnStatus::Connected) else { return };
        let (driver, read_only) = (c.cfg.driver, c.cfg.read_only);
        if crate::db::users::UserModel::of(driver).is_none() {
            return;
        }
        if c.users.list.is_none() && !c.users.loading {
            self.load_users(conn);
        }
        self.user_dialog = Some(match account {
            None => UserDialog::new_user(conn, driver),
            Some(a) if drop => UserDialog::drop(conn, driver, a),
            Some(a) => {
                self.worker.send(Request::Users { conn, request: crate::db::users::UsersRequest::Detail(a.clone()) });
                UserDialog::edit(conn, driver, a, read_only)
            }
        });
    }

    /// Demo mode: `DATACRAFT_DEMO_USERS=1` expands the "Users" / "Roles" folder; `new`, `edit` or
    /// `drop` also opens that dialog (on the first account that is not root), once.
    fn demo_users(&mut self, conn: ConnId, first: Option<crate::db::users::Account>) {
        static OPENED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if std::env::var("DATACRAFT_DEMO").is_err() {
            return;
        }
        let Ok(mode) = std::env::var("DATACRAFT_DEMO_USERS") else { return };
        if OPENED.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        match mode.as_str() {
            "new" => self.open_user_dialog(conn, None, false),
            "edit" if first.is_some() => self.open_user_dialog(conn, first, false),
            "drop" if first.is_some() => self.open_user_dialog(conn, first, true),
            _ => {}
        }
    }

    fn users_response(&mut self, conn: ConnId, result: Result<crate::db::users::UsersReply, String>) {
        use crate::db::users::UsersReply;
        match result {
            Ok(UsersReply::List(list)) => {
                let first = list.accounts.iter().find(|a| a.account.name != "root" && !a.account.name.is_empty()).map(|a| a.account.clone());
                if let Some(c) = self.conn_mut(conn) {
                    c.users = crate::ui::user_dialog::UsersNode { list: Some(list), loading: false, error: None };
                }
                self.demo_users(conn, first);
            }
            Ok(UsersReply::Detail(account, user)) => {
                if let Some(d) = self.user_dialog.as_mut().filter(|d| d.conn == conn && d.pending.as_ref() == Some(&account)) {
                    d.pending = None;
                    let keep_edits = !d.loading;
                    d.loaded(*user, keep_edits);
                }
            }
            Err(e) => {
                // A failed detail belongs to the dialog waiting for it, if any; otherwise to the list.
                if let Some(d) = self.user_dialog.as_mut().filter(|d| d.conn == conn && d.pending.is_some()) {
                    d.pending = None;
                    if d.loading {
                        d.load_failed(e);
                    }
                } else if let Some(c) = self.conn_mut(conn) {
                    c.users.loading = false;
                    c.users.error = Some(e);
                }
            }
        }
    }

    /// Reply to the user dialog's statements.
    fn user_ddl_done(&mut self, result: Result<(), String>) {
        let Some(d) = self.user_dialog.as_mut() else { return };
        let conn = d.conn;
        if let Err(e) = result {
            d.set_error(&e);
            // What ran before the error stays (account statements commit on their own): read the
            // user again so the next Apply only sends what is still missing.
            if !d.dropping {
                let account = d.edited_account();
                d.pending = Some(account.clone());
                self.worker.send(Request::Users { conn, request: crate::db::users::UsersRequest::Detail(account) });
            }
            self.load_users(conn);
            return;
        }
        let Some(d) = self.user_dialog.take() else { return };
        let name = d.edited_account().label(d.driver);
        let args: &[(&str, &dyn std::fmt::Display)] = &[("name", &name)];
        let msg = match (d.driver == Driver::Postgres, d.dropping, d.original.is_some()) {
            (false, true, _) => trf("User “{name}” dropped", args),
            (false, false, true) => trf("User “{name}” saved", args),
            (false, false, false) => trf("User “{name}” created", args),
            (true, true, _) => trf("Role “{name}” dropped", args),
            (true, false, true) => trf("Role “{name}” saved", args),
            (true, false, false) => trf("Role “{name}” created", args),
        };
        self.info(msg);
        self.explorer.expanded.insert(format!("u:{conn}"));
        self.load_users(conn);
    }

    fn user_dialog_ui(&mut self, ctx: &egui::Context) {
        use crate::db::users::PrivTarget;
        use crate::ui::user_dialog::{UserContext, UserDialogResult};
        let Some(d) = self.user_dialog.as_ref() else { return };
        let conn = d.conn;
        let Some(c) = self.conns.iter().find(|c| c.cfg.id == conn) else {
            self.user_dialog = None;
            return;
        };
        let mut uc = UserContext { databases: c.databases.iter().map(|d| d.name.clone()).collect(), objects: Vec::new(), list: c.users.list.as_ref() };
        if c.cfg.driver == Driver::Postgres {
            for s in c.schemas(None).unwrap_or_default() {
                uc.objects.push(PrivTarget::Schema(s.name.clone()));
                uc.objects.extend(s.tables.iter().map(|t| PrivTarget::Table(s.name.clone(), t.name.clone())));
            }
        }
        let Some(d) = self.user_dialog.as_mut() else { return };
        match d.show(ctx, &uc) {
            Some(UserDialogResult::Apply(statements)) => {
                // Read-only connections never get here (no Apply button), but check anyway.
                if c.cfg.read_only {
                    d.error = Some(tr("Read-only connection").into());
                    return;
                }
                d.running = true;
                d.error = None;
                self.worker.send(Request::RunDdl { conn, db: None, statements, close_db: None, transaction: false });
            }
            Some(UserDialogResult::Cancel) => self.user_dialog = None,
            None => {}
        }
    }

    /// Reply to the "New Database" / "New Table" dialog's statements.
    fn ddl_done(&mut self, conn: ConnId, result: Result<(), String>) {
        if self.view_ddl_done(conn, &result) || self.routine_ddl_done(conn, &result) {
            return;
        }
        if self.modify.as_ref().is_some_and(|d| d.conn == conn && d.running) {
            self.modify_done(result);
            return;
        }
        if self.user_dialog.as_ref().is_some_and(|d| d.conn == conn && d.running) {
            self.user_ddl_done(result);
            return;
        }
        if self.drop.as_ref().is_some_and(|d| d.conn == conn && d.running) {
            self.drop_done(result);
            return;
        }
        if self.import.as_ref().is_some_and(|d| d.conn == conn && d.running) {
            self.import_done(result);
            return;
        }
        let Some(d) = self.create.as_mut().filter(|d| d.conn == conn && d.running) else {
            if let Err(e) = result {
                self.error(e);
            }
            return;
        };
        if let Err(e) = result {
            d.running = false;
            d.error = Some(e);
            return;
        }
        let Some(d) = self.create.take() else { return };
        let name = d.name.trim().to_string();
        match d.target {
            CreateTarget::Database => {
                self.explorer.expanded.insert(format!("c:{conn}"));
                self.explorer.selected = Some(explorer::db_key(conn, &name));
                self.worker.send(Request::Refresh(conn));
            }
            CreateTarget::Table { db, schema } => {
                let schema = match d.driver {
                    Driver::Sqlite => "main".to_string(),
                    Driver::MySql | Driver::MariaDb => db.clone(),
                    _ => schema.trim().to_string(),
                };
                let tref = TableRef { conn, db: db.clone(), schema: schema.clone(), table: name.clone() };
                self.explorer.expanded.insert(format!("c:{conn}"));
                self.explorer.expanded.insert(explorer::db_key(conn, &db));
                self.explorer.expanded.insert(format!("s:{conn}:{db}:{schema}"));
                self.explorer.selected = Some(explorer::table_key(&tref));
                self.explorer.selected_table = Some(tref);
                // The reload brings the new table; if a load is already in progress it may
                // miss it, so wait for it and ask again.
                if let Some(c) = self.conn_mut(conn) {
                    c.loading.remove(&db);
                }
                self.load_database(conn, db);
                self.info(trf("Table “{name}” created", &[("name", &name)]));
            }
        }
    }

    fn drop_done(&mut self, result: Result<(), String>) {
        let Some(d) = self.drop.as_mut() else { return };
        if let Err(e) = result {
            d.running = false;
            d.error = Some(e);
            return;
        }
        let Some(d) = self.drop.take() else { return };
        let conn = d.conn;
        if let Some(r) = d.routine_ref() {
            self.routine_dropped(&r);
            return;
        }
        match d.kind {
            DropKind::Procedure | DropKind::Function => {}
            DropKind::Database => {
                if let Some(c) = self.conn_mut(conn) {
                    c.databases.retain(|x| x.name != d.db);
                    c.loading.remove(&d.db);
                    c.db_errors.remove(&d.db);
                }
                self.explorer.selected = None;
                if self.explorer.selected_table.as_ref().is_some_and(|t| t.conn == conn && t.db == d.db) {
                    self.explorer.selected_table = None;
                }
                self.worker.send(Request::Refresh(conn));
            }
            DropKind::Truncate | DropKind::Empty => {
                // Open data editors of that table show the (now empty) data again.
                self.reload_table_editors(conn, &d.db, &d.schema, &d.name);
                self.info(trf(if d.kind == DropKind::Empty { "Table “{name}” emptied" } else { "Table “{name}” truncated" }, &[("name", &d.name)]));
            }
            DropKind::Table | DropKind::View | DropKind::Rename => {
                // Its data editors no longer have anything to show.
                let default_db = self.conn(conn).and_then(|c| c.default_db()).map(str::to_string);
                let gone = |c: &Console| {
                    c.conn == Some(conn)
                        && c.db.as_deref().or(default_db.as_deref()) == Some(d.db.as_str())
                        && c.table.as_ref().is_some_and(|t| t.schema == d.schema && t.table == d.name)
                };
                while let Some(i) = self.consoles.iter().position(gone) {
                    self.close_console_now(i);
                }
                let tref = TableRef { conn, db: d.db.clone(), schema: d.schema.clone(), table: d.name.clone() };
                if self.explorer.selected_table.as_ref() == Some(&tref) {
                    self.explorer.selected_table = None;
                    self.explorer.selected = None;
                }
                if let Some(c) = self.conn_mut(conn) {
                    c.loading.remove(&d.db);
                }
                self.load_database(conn, d.db.clone());
                let new_name = (d.kind == DropKind::Rename).then(|| crate::ui::drop_dialog::effective_name(d.driver, &d.new_name));
                for org in self.object_browser.organizations.iter_mut().filter(|o| o.conn == conn && o.db == d.db) {
                    org.table_changed(&d.schema, &d.name, new_name.as_deref());
                }
                if d.kind == DropKind::Rename {
                    let new_name = crate::ui::drop_dialog::effective_name(d.driver, &d.new_name);
                    let renamed = TableRef { table: new_name.clone(), ..tref };
                    self.explorer.selected = Some(explorer::table_key(&renamed));
                    self.explorer.selected_table = Some(renamed);
                    self.info(trf("Table “{name}” renamed to “{new_name}”", &[("name", &d.name), ("new_name", &new_name)]));
                } else {
                    let msg = if d.kind == DropKind::Table { "Table “{name}” dropped" } else { "View “{name}” dropped" };
                    self.info(trf(msg, &[("name", &d.name)]));
                }
            }
        }
    }

    /// Monotonic ids: a deleted connection's id is never reused (its keychain password or a
    /// late worker response could be associated with the new one).
    fn next_conn_id(&mut self) -> u64 {
        let max = self.conns.iter().map(|c| c.cfg.id).max().unwrap_or(0).max(self.last_conn_id);
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let id = (max + 1).max(millis);
        self.last_conn_id = id;
        id
    }

    fn open_new_connection_dialog(&mut self) {
        let id = self.next_conn_id();
        self.dialog = Some(ConnectionDialog::new_connection(id, &self.conn_configs()));
    }

    fn conn_configs(&self) -> Vec<ConnectionConfig> {
        self.conns.iter().map(|c| c.cfg.clone()).collect()
    }

    fn save_connection(&mut self, cfg: ConnectionConfig) {
        config::store_secrets(&cfg);
        let id = cfg.id;
        if cfg.read_only {
            self.revoke_editing(id);
        }
        if let Some(existing) = self.conn_mut(id) {
            existing.cfg = cfg;
            if existing.status != ConnStatus::Disconnected {
                self.disconnect(id);
            }
        } else {
            self.conns.push(ConnState::new(cfg));
            if let Some(c) = self.consoles.get_mut(self.active)
                && c.conn.is_none()
            {
                c.conn = Some(id);
            }
        }
        self.persist_connections();
        self.info(tr("Connection saved"));
    }

    /// A connection became read-only: its grids stop being editable (pending changes are dropped).
    fn revoke_editing(&mut self, conn: ConnId) {
        for c in self.consoles.iter_mut().filter(|c| c.conn == Some(conn)) {
            for v in c.results.iter_mut() {
                v.edit = None;
                v.readonly_reason = Some(tr("Read-only connection").to_string());
                v.edit_checked = true;
            }
        }
        if self.preview.as_ref().is_some_and(|p| self.consoles.iter().any(|c| c.id == p.console && c.conn == Some(conn))) {
            self.preview = None;
        }
    }

    fn delete_connection(&mut self, id: ConnId) {
        self.worker.send(Request::Disconnect(id));
        self.conns.retain(|c| c.cfg.id != id);
        self.object_browser.organizations.retain(|o| o.conn != id);
        config::delete_secrets(id);
        for c in self.consoles.iter_mut().filter(|c| c.conn == Some(id)) {
            c.conn = None;
            c.db = None;
            c.pending = None;
            c.run_started = None;
        }
        // That connection's data editors no longer make sense.
        let active_id = self.consoles.get(self.active).map(|c| c.id);
        self.consoles.retain(|c| !(c.table.is_some() && c.conn.is_none()));
        self.active = active_id
            .and_then(|id| self.consoles.iter().position(|c| c.id == id))
            .unwrap_or(0)
            .min(self.consoles.len().saturating_sub(1));
        self.persist_connections();
    }

    // ---------------------------------------------------------------- files

    fn open_sql_file(&mut self) {
        let Some(path) = rfd::FileDialog::new().set_title(tr("Open SQL script")).add_filter("SQL", &["sql"]).pick_file() else {
            return;
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let title = path.file_stem().map(|s| s.to_string_lossy().into_owned());
                let (conn, db) = self.consoles.get(self.active).map(|c| (c.conn, c.db.clone())).unwrap_or_default();
                let i = self.new_console(conn, title, text);
                self.consoles[i].db = db;
            }
            Err(e) => self.error(trf("Could not open {path}: {error}", &[("path", &path.display()), ("error", &e)])),
        }
    }

    fn save_console_as(&mut self) {
        let Some(console) = self.consoles.get(self.active).filter(|c| c.table.is_none()) else { return };
        let Some(path) = rfd::FileDialog::new()
            .set_title(tr("Save console as"))
            .add_filter("SQL", &["sql"])
            .set_file_name(format!("{}.sql", console.title))
            .save_file()
        else {
            return;
        };
        match std::fs::write(&path, &console.text) {
            Ok(()) => self.info(trf("Saved to {path}", &[("path", &path.display())])),
            Err(e) => self.error(trf("Could not save: {error}", &[("error", &e)])),
        }
    }

    fn close_console(&mut self, idx: usize) {
        if let Some(id) = self.consoles.get(idx).map(|c| c.id)
            && self.guard(Discard::Close(id))
        {
            self.close_console_now(idx);
        }
    }

    fn tab_close_indices(&self, id: u64, which: CloseResults) -> Vec<usize> {
        // Console IDs start at 1; 0 identifies the leading Objects tab.
        if id == 0 { return (0..self.consoles.len()).collect(); }
        let Some(i) = self.consoles.iter().position(|c| c.id == id) else { return Vec::new() };
        match which {
            CloseResults::One(_) => CloseResults::One(i),
            CloseResults::Others(_) => CloseResults::Others(i),
            CloseResults::Left(_) => CloseResults::Left(i),
            CloseResults::Right(_) => CloseResults::Right(i),
            CloseResults::All => CloseResults::All,
        }.indices(self.consoles.len())
    }

    fn close_tabs_now(&mut self, id: u64, which: CloseResults) {
        for i in self.tab_close_indices(id, which).into_iter().rev() { self.close_console_now(i); }
        if id == 0 { self.object_browser.active = true; }
        else if matches!(which, CloseResults::Others(_) | CloseResults::Left(_) | CloseResults::All) {
            self.object_browser.scope = None;
            self.object_browser.active = false;
        }
    }

    fn close_console_now(&mut self, idx: usize) {
        if idx < self.consoles.len() {
            self.consoles.remove(idx);
            if self.active > idx || self.active >= self.consoles.len() {
                self.active = self.active.saturating_sub(1);
            }
        }
    }

    fn new_console_here(&mut self) {
        if self.object_browser.active && let Some(scope) = self.object_browser.scope.clone() {
            let i = self.new_console(Some(scope.conn), None, String::new());
            self.consoles[i].db = Some(scope.db);
            return;
        }
        let (conn, db) = match self.consoles.get(self.active).filter(|c| c.conn.is_some()) {
            Some(c) => (c.conn, c.db.clone()),
            None => (self.conns.first().map(|c| c.cfg.id), None),
        };
        let i = self.new_console(conn, None, String::new());
        self.consoles[i].db = db;
    }

    // ---------------------------------------------------------------- UI

    fn shortcuts(&mut self, ctx: &egui::Context) {
        // IDE zoom (⌘= / ⌘+ / ⌘- / ⌘0) and editor font (⌥⌘= / ⌥⌘-). The ⌥ variants go
        // first: `consume_key` ignores an extra ⌥.
        let (font_up, font_down, zoom_up, zoom_down, zoom_reset) = ctx.input_mut(|i| {
            let alt = Modifiers::COMMAND | Modifiers::ALT;
            let font_up = i.consume_key(alt, Key::Equals) || i.consume_key(alt, Key::Plus);
            let font_down = i.consume_key(alt, Key::Minus);
            let zoom_up = i.consume_key(Modifiers::COMMAND, Key::Equals) || i.consume_key(Modifiers::COMMAND, Key::Plus);
            let zoom_down = i.consume_key(Modifiers::COMMAND, Key::Minus);
            let zoom_reset = i.consume_key(Modifiers::COMMAND, Key::Num0);
            (font_up, font_down, zoom_up, zoom_down, zoom_reset)
        });
        if font_up {
            self.font_size = (self.font_size + 1.0).min(32.0);
        }
        if font_down {
            self.font_size = (self.font_size - 1.0).max(8.0);
        }
        if zoom_up {
            self.zoom = zoom_in(self.zoom);
        }
        if zoom_down {
            self.zoom = zoom_out(self.zoom);
        }
        if zoom_reset {
            self.zoom = 1.0;
        }
        if self.modal_open() {
            return;
        }
        let in_objects = self.object_browser.active;
        let in_table = !in_objects && self.consoles.get(self.active).is_some_and(|c| c.table.is_some());
        let (run_all, run, new_console, reload, quit, save) = ctx.input_mut(|i| {
            let run_all = !in_objects && !in_table && i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Enter);
            let run = !in_objects && !in_table && i.consume_key(Modifiers::COMMAND, Key::Enter);
            let new_console = i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::N);
            // In a console ⌘R is "Replace" (handled by the editor).
            let reload = in_table && i.consume_key(Modifiers::COMMAND, Key::R);
            let quit = i.consume_key(Modifiers::COMMAND, Key::Q);
            let save = !in_objects && i.consume_key(Modifiers::COMMAND, Key::S);
            (run_all, run, new_console, reload, quit, save)
        });
        if run_all {
            self.run_console(self.active, true);
        } else if run {
            self.run_console(self.active, false);
        }
        if reload {
            self.reload_active();
        }
        if save {
            self.save_active_changes();
        }
        if new_console {
            self.new_console_here();
        }
        if quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// A modal dialog is open: shortcuts and run commands are ignored.
    fn modal_open(&self) -> bool {
        self.backup.is_some() || self.scheduler.is_some() || self.script.is_some() || self.object_browser.schema_search.open || self.object_browser.has_dialog() || self.dialog.is_some()
            || self.confirm_delete.is_some()
            || self.confirm_analyze.is_some()
            || self.create.is_some()
            || self.views.dialog.is_some()
            || self.routines.is_open()
            || self.drop.is_some()
            || self.import.is_some()
            || self.copy.is_some()
            || self.mask_export.is_some()
            || self.user_dialog.is_some()
            || self.show_shortcuts
            || self.show_about
            || self.show_settings
            || self.preview.as_ref().is_some_and(|p| p.visible)
            || self.confirm_discard.is_some()
    }

    fn reload_active(&mut self) {
        if let Some(c) = self.consoles.get(self.active).filter(|c| c.table.is_some() && !c.is_busy())
            && self.guard(Discard::Reload(c.id))
        {
            self.reload_table(self.active);
        }
    }

    fn save_active_changes(&mut self) {
        if self.consoles.get(self.active).is_some_and(|c| c.routine.is_some()) { self.save_routine_tab(self.active); return; }
        if let Some(c) = self.consoles.get(self.active)
            && let Some(vi) = c.edit_view_index()
            && !c.is_busy()
        {
            self.submit_now(self.active, vi);
        }
    }

    /// Native macOS menu commands. The menu takes their accelerators (⌘↩, ⌘S, ⌘=...), so the
    /// equivalent shortcuts in `shortcuts` only act if the menu was not installed.
    fn menu_commands(&mut self, ctx: &egui::Context) {
        use crate::native_menu::MenuCommand as M;
        let Some(menu) = self.native_menu.as_ref() else { return };
        let cmds = menu.poll();
        for cmd in cmds {
            let modal = self.modal_open();
            match cmd {
                M::About => self.show_about = true,
                M::CheckUpdates => self.check_for_updates(ctx),
                M::Settings => self.show_settings = true,
                M::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                M::NewConnection if !modal => self.open_new_connection_dialog(),
                M::NewConsole if !modal => self.new_console_here(),
                M::OpenSql if !modal => self.open_sql_file(),
                M::SaveSqlAs if !modal && !self.object_browser.active => self.save_console_as(),
                M::CloseTab if !modal && self.object_browser.active => { self.object_browser.active = false; self.object_browser.scope = None; }
                M::CloseTab if !modal && !self.consoles.is_empty() => self.close_console(self.active),
                // In a console: find in the editor; elsewhere: filter the explorer.
                M::Find if !modal => {
                    if self.object_browser.active {
                        ctx.memory_mut(|m| m.request_focus(egui::Id::new("object_search")));
                    } else if let Some(c) = self.consoles.get_mut(self.active) {
                        c.open_find(false);
                    } else {
                        self.show_explorer = true;
                        self.explorer.show_filter = true;
                    }
                }
                M::ToggleExplorer => self.show_explorer = !self.show_explorer,
                M::ToggleResults => self.show_results = !self.show_results,
                M::ThemeLight => self.dark = false,
                M::ThemeDark => self.dark = true,
                M::ZoomIn => self.zoom = zoom_in(self.zoom),
                M::ZoomOut => self.zoom = zoom_out(self.zoom),
                M::ZoomReset => self.zoom = 1.0,
                M::FontBigger => self.font_size = (self.font_size + 1.0).min(32.0),
                M::FontSmaller => self.font_size = (self.font_size - 1.0).max(8.0),
                M::FontReset => self.font_size = DEFAULT_FONT_SIZE,
                M::Run if !modal && !self.object_browser.active => self.run_console(self.active, false),
                M::RunAll if !modal && !self.object_browser.active => self.run_console(self.active, true),
                M::SaveChanges if !modal && !self.object_browser.active => self.save_active_changes(),
                // ⌘R: reload in the data editor, replace in a console (like DataGrip).
                M::Reload if !modal => {
                    if self.object_browser.active {
                        if let Some(scope) = &self.object_browser.scope { self.load_database(scope.conn, scope.db.clone()); }
                    } else if let Some(c) = self.consoles.get_mut(self.active).filter(|c| c.table.is_none()) {
                        c.open_find(true);
                    } else {
                        self.reload_active();
                    }
                }
                // No help website: shows the app info window.
                M::Help => self.show_about = true,
                M::Shortcuts => self.show_shortcuts = true,
                _ => {}
            }
        }
        if let Some(menu) = self.native_menu.as_ref() {
            menu.sync(self.dark, self.show_explorer, self.show_results);
        }
    }

    /// Main application menu (under the "DataCraft ⌄" project widget).
    fn app_menu(&mut self, ui: &mut Ui) {
        ui.set_min_width(220.0);
        ui.menu_button(tr("File"), |ui| {
            if ui.button(tr("New data source…")).clicked() {
                self.open_new_connection_dialog();
                ui.close();
            }
            if ui.add(egui::Button::new(tr("New console")).shortcut_text(crate::shortcuts::format("⇧⌘N"))).clicked() {
                self.new_console_here();
                ui.close();
            }
            ui.separator();
            if ui.button(tr("Open .sql…")).clicked() {
                ui.close();
                self.open_sql_file();
            }
            let demo = std::env::var("DATACRAFT_DEMO").is_ok_and(|v| !v.is_empty());
            if ui.add_enabled(!demo, egui::Button::new(tr("Task scheduler…"))).clicked() {
                let configs: Vec<_> = self.conns.iter().map(|connection| connection.cfg.clone()).collect();
                self.scheduler = Some(crate::scheduler::dialog::Dialog::new(ui.ctx(), &configs));
                ui.close();
            }
            let can_save = !self.object_browser.active && self.consoles.get(self.active).is_some_and(|c| c.table.is_none());
            if ui.add_enabled(can_save, egui::Button::new(tr("Save console as .sql…"))).clicked() {
                ui.close();
                self.save_console_as();
            }
            ui.separator();
            if ui.add(egui::Button::new(tr("Quit")).shortcut_text(crate::shortcuts::format("⌘Q"))).clicked() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
        ui.menu_button(tr("View"), |ui| self.view_menu(ui));
        ui.menu_button(tr("Help"), |ui| {
            if ui.button(tr("Keyboard shortcuts…")).clicked() {
                self.show_shortcuts = true;
                ui.close();
            }
            if ui.button(tr("Check for Updates…")).clicked() {
                self.check_for_updates(ui.ctx());
                ui.close();
            }
            if ui.button(tr("About DataCraft")).clicked() {
                self.show_about = true;
                ui.close();
            }
        });
    }

    fn view_menu(&mut self, ui: &mut Ui) {
        let p = palette(self.dark);
        ui.label(RichText::new(tr("Theme")).small().color(p.muted));
        if ui.radio(!self.dark, "Islands Light").clicked() {
            self.dark = false;
            ui.close();
        }
        if ui.radio(self.dark, "Islands Dark").clicked() {
            self.dark = true;
            ui.close();
        }
        ui.separator();
        ui.checkbox(&mut self.show_explorer, tr("Database Explorer"));
        ui.checkbox(&mut self.show_results, tr("Services (results)"));
        ui.separator();
        ui.menu_button(trf("IDE zoom ({zoom})", &[("zoom", &zoom_label(self.zoom))]), |ui| self.zoom_menu(ui));
        // Also in English, so it can be found from a language the user can't read.
        let language = if i18n::lang() == Lang::En { "Language".to_string() } else { format!("{} / Language", tr("Language")) };
        ui.menu_button(language, |ui| {
            for lang in Lang::ALL {
                if ui.radio(i18n::lang() == lang, lang.native_name()).clicked() {
                    self.set_language(lang);
                    ui.close();
                }
            }
        });
        ui.separator();
        ui.label(RichText::new(trf("Editor font: {size} pt", &[("size", &format!("{:.0}", self.font_size))])).small().color(p.muted));
        if ui.add(egui::Button::new(tr("Increase")).shortcut_text(crate::shortcuts::format("⌥⌘+"))).clicked() {
            self.font_size = (self.font_size + 1.0).min(32.0);
        }
        if ui.add(egui::Button::new(tr("Decrease")).shortcut_text(crate::shortcuts::format("⌥⌘-"))).clicked() {
            self.font_size = (self.font_size - 1.0).max(8.0);
        }
        if ui.button(tr("Reset")).clicked() {
            self.font_size = DEFAULT_FONT_SIZE;
        }
    }

    /// Changes the interface language, remembers it and relabels the native menu.
    fn set_language(&mut self, lang: Lang) {
        i18n::set_lang(lang);
        self.lang = Some(lang);
        if let Some(menu) = self.native_menu.as_ref() {
            menu.relabel();
        }
    }

    /// "IDE zoom" submenu: zoom in, zoom out, reset and fixed steps.
    fn zoom_menu(&mut self, ui: &mut Ui) {
        ui.set_min_width(180.0);
        if ui.add_enabled(self.zoom < ZOOM_STEPS[ZOOM_STEPS.len() - 1], egui::Button::new(tr("Zoom In")).shortcut_text(crate::shortcuts::format("⌘+"))).clicked() {
            self.zoom = zoom_in(self.zoom);
        }
        if ui.add_enabled(self.zoom > ZOOM_STEPS[0], egui::Button::new(tr("Zoom Out")).shortcut_text(crate::shortcuts::format("⌘-"))).clicked() {
            self.zoom = zoom_out(self.zoom);
        }
        if ui.add(egui::Button::new(tr("Reset (100%)")).shortcut_text(crate::shortcuts::format("⌘0"))).clicked() {
            self.zoom = 1.0;
            ui.close();
        }
        ui.separator();
        for z in ZOOM_STEPS {
            if ui.radio((self.zoom - z).abs() < 0.001, zoom_label(z)).clicked() {
                self.zoom = z;
                ui.close();
            }
        }
    }

    /// Top bar unified with the title (main toolbar).
    fn title_bar(&mut self, ui: &mut Ui) {
        let p = palette(self.dark);
        let rect = ui.max_rect();
        let mut drag_left = rect.left();
        let mut drag_right = rect.right();
        let fullscreen = ui.input(|i| i.viewport().fullscreen.unwrap_or(false));
        ui.horizontal_centered(|ui| {
            theme::split_row(ui, |ui, side| match side {
                theme::Side::Left => {
            ui.spacing_mut().item_spacing.x = 4.0;
            // Reserve traffic-light space only on macOS, independent of IDE zoom.
            let zoom = ui.ctx().zoom_factor().max(0.1);
            ui.add_space(crate::ui::window_chrome::leading_space(cfg!(target_os = "macos"), fullscreen, zoom));

            // Project widget uses the same mark as the native window and executable.
            let font = theme::bold(13.0);
            let galley = ui.painter().layout_no_wrap("DataCraft".into(), font, p.text);
            let w = 8.0 + 20.0 + 8.0 + galley.size().x + 4.0 + 14.0 + 8.0;
            let (r, resp) = ui.allocate_exact_size(vec2(w, 28.0), Sense::click());
            if resp.hovered() || resp.is_pointer_button_down_on() {
                ui.painter().rect_filled(r, 6.0, if resp.is_pointer_button_down_on() { p.pressed } else { p.hover });
            }
            let sq = Rect::from_min_size(pos2(r.left() + 8.0, r.center().y - 10.0), vec2(20.0, 20.0));
            crate::ui::branding::paint(ui, sq);
            ui.painter().galley(pos2(sq.right() + 8.0, r.center().y - galley.size().y / 2.0), galley.clone(), p.text);
            icons::paint(
                ui.painter(),
                Rect::from_min_size(pos2(sq.right() + 8.0 + galley.size().x + 4.0, r.center().y - 7.0), vec2(14.0, 14.0)),
                Ico::ChevronDown,
                p.muted,
                &p,
                true,
            );
            egui::Popup::menu(&resp).show(|ui| self.app_menu(ui));

            // Active tab's connection selector.
            let object_scope = self.object_browser.scope.as_ref().filter(|_| self.object_browser.active);
            let current = object_scope.map(|s| s.conn).or_else(|| self.consoles.get(self.active).and_then(|c| c.conn)).and_then(|id| self.conn(id));
            let active_db = object_scope.map(|s| s.db.clone()).or_else(|| self.consoles.get(self.active).and_then(|c| c.db.clone()));
            let (ico, label) = match current {
                Some(c) => (Ico::Driver(c.cfg.driver), c.summary().label(active_db.as_deref())),
                None => (Ico::Database, tr("No connection").into()),
            };
            let busy = self.object_browser.active || self.consoles.get(self.active).is_some_and(|c| c.is_busy() || c.table.is_some() || c.routine.is_some());
            let resp = theme::flat_button(ui, Some(ico), &label, FontId::proportional(13.0), true, true)
                .on_hover_text(tr("Data source of the active tab"));
            let mut chosen = None;
            let mut new_ds = false;
            egui::Popup::menu(&resp).show(|ui| {
                ui.set_min_width(220.0);
                for c in &self.conns {
                    let sel = current.is_some_and(|x| x.cfg.id == c.cfg.id);
                    if ui
                        .add_enabled(!busy || sel, egui::Button::selectable(sel, format!("{}  ·  {}", c.cfg.name, c.cfg.driver.label())))
                        .clicked()
                    {
                        chosen = Some(c.cfg.id);
                        ui.close();
                    }
                }
                if !self.conns.is_empty() {
                    ui.separator();
                }
                if ui.button(tr("New data source…")).clicked() {
                    new_ds = true;
                    ui.close();
                }
            });
            if new_ds {
                self.open_new_connection_dialog();
            }
            if let Some(id) = chosen {
                self.set_active_connection(id);
            }
            drag_left = ui.min_rect().right();

                }
                theme::Side::Right => {
                if cfg!(target_os = "windows") {
                    if let Some(command) = crate::ui::window_chrome::controls(ui) { ui.ctx().send_viewport_cmd(command); }
                    ui.add_space(8.0);
                }
                ui.add_space(5.0);
                ui.spacing_mut().item_spacing.x = 2.0;
                let gear = theme::icon_button_sized(ui, Ico::Gear, true, tr("Settings"), 30.0);
                egui::Popup::menu(&gear).show(|ui| {
                    self.view_menu(ui);
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label(tr("Max rows:"));
                        ui.add(egui::DragValue::new(&mut self.max_rows).range(1..=1_000_000).speed(10));
                    });
                });
                if theme::icon_button_sized(ui, Ico::Search, true, tr("Search tables"), 30.0).clicked() {
                    self.show_explorer = true;
                    self.explorer.show_filter = !self.explorer.show_filter;
                }
                drag_right = ui.min_rect().left();
                }
            });
        });
        if drag_right > drag_left {
            let drag_rect = Rect::from_min_max(pos2(drag_left, rect.top()), pos2(drag_right, rect.bottom()));
            let drag = ui.interact(drag_rect, egui::Id::new("title_bar_drag"), Sense::click_and_drag());
            if drag.drag_started_by(egui::PointerButton::Primary) { ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag); }
            if drag.double_clicked() {
                let maximized = ui.input(|i| i.viewport().maximized.unwrap_or(false));
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
            }
        }
    }

    /// Changes the active console's connection (if it is not busy).
    fn set_active_connection(&mut self, id: ConnId) {
        let needs_connect = self.conn(id).is_some_and(|c| matches!(c.status, ConnStatus::Disconnected | ConnStatus::Error(_)));
        if let Some(c) = self.consoles.get_mut(self.active) {
            if c.is_busy() || c.table.is_some() || c.routine.is_some() || c.conn == Some(id) {
                return;
            }
            c.conn = Some(id);
            c.db = None;
            c.pending = None;
            c.run_started = None;
        } else {
            self.new_console(Some(id), None, String::new());
        }
        if needs_connect {
            self.connect(id);
        }
    }

    /// Left tool window stripe.
    fn left_stripe(&mut self, ui: &mut Ui) {
        let rect = ui.max_rect();
        let mut top = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::top_down(egui::Align::Center)));
        top.spacing_mut().item_spacing.y = 4.0;
        top.add_space(2.0);
        if stripe_button(&mut top, Ico::Database, self.show_explorer, tr("Database Explorer")).clicked() {
            self.show_explorer = !self.show_explorer;
        }
        let more = stripe_button(&mut top, Ico::More, false, tr("More tool windows"));
        egui::Popup::menu(&more).show(|ui| {
            ui.checkbox(&mut self.show_explorer, tr("Database Explorer"));
            ui.checkbox(&mut self.show_results, tr("Services"));
        });

        let mut bottom = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::bottom_up(egui::Align::Center)));
        bottom.spacing_mut().item_spacing.y = 4.0;
        bottom.add_space(4.0);
        let has_error = self.status_msg.as_ref().is_some_and(|(_, e)| *e);
        let problems = stripe_button(&mut bottom, Ico::Problems, false, tr("Problems"));
        if problems.clicked()
            && let Some((msg, _)) = &self.status_msg
        {
            ui.ctx().copy_text(msg.clone());
        }
        if has_error {
            let c = problems.rect.right_top() + vec2(-9.0, 9.0);
            bottom.painter().circle_filled(c, 3.0, palette(self.dark).error);
        }
        if stripe_button(&mut bottom, Ico::Terminal, false, &crate::shortcuts::format(tr("New console (⇧⌘N)"))).clicked() {
            self.new_console_here();
        }
        if stripe_button(&mut bottom, Ico::Services, self.show_results, tr("Services (results)")).clicked() {
            self.show_results = !self.show_results;
        }
    }

    /// Right stripe (notifications and structure).
    fn right_stripe(&mut self, ui: &mut Ui) {
        let rect = ui.max_rect();
        let mut top = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::top_down(egui::Align::Center)));
        top.spacing_mut().item_spacing.y = 4.0;
        top.add_space(2.0);
        let msg = self.status_msg.clone();
        let bell = stripe_button(&mut top, Ico::Bell, false, tr("Notifications"));
        egui::Popup::menu(&bell).show(|ui| {
            ui.set_min_width(260.0);
            match &msg {
                Some((m, err)) => {
                    let color = if *err { palette(self.dark).error } else { palette(self.dark).text };
                    ui.add(egui::Label::new(RichText::new(m).color(color)).wrap());
                    if ui.button(tr("Dismiss")).clicked() {
                        self.status_msg = None;
                        ui.close();
                    }
                }
                None => {
                    ui.label(RichText::new(tr("No notifications")).color(palette(self.dark).muted));
                }
            }
        });
        if msg.is_some() {
            let c = bell.rect.right_top() + vec2(-9.0, 9.0);
            top.painter().circle_filled(c, 3.0, palette(self.dark).accent);
        }
        let structure = stripe_button(&mut top, Ico::Structure, false, tr("Tab structure"));
        egui::Popup::menu(&structure).show(|ui| {
            let cols = self.consoles.get(self.active).and_then(|c| {
                let t = c.table.as_ref()?;
                self.table_columns(c.conn?, c.db.as_deref(), &t.schema, &t.table)
            });
            match cols {
                Some(cols) => {
                    for c in cols {
                        ui.label(format!("{}  {}", c.name, c.data_type.to_lowercase()));
                    }
                }
                None => {
                    ui.label(RichText::new(tr("No structure")).color(palette(self.dark).muted));
                }
            }
        });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let p = palette(self.dark);
        ui.horizontal_centered(|ui| {
            theme::split_row(ui, |ui, side| match side {
                theme::Side::Left => {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.add_space(12.0);
            // Active tab's breadcrumbs.
            let console = self.consoles.get(self.active);
            let conn = console.and_then(|c| c.conn).and_then(|id| self.conn(id));
            let mut crumbs: Vec<(Option<Ico>, String)> = vec![(None, tr("Database").into())];
            if let Some(c) = conn {
                crumbs.push((Some(Ico::Driver(c.cfg.driver)), c.cfg.name.clone()));
                if let Some(db) = c.resolve_db(console.and_then(|x| x.db.as_deref())) {
                    crumbs.push((Some(Ico::Database), db.to_string()));
                }
            }
            if let Some(c) = console {
                match &c.table {
                    Some(t) => {
                        // SQLite / MySQL: the schema is the same as the database (the explorer omits it too).
                        if conn.is_none_or(|x| !x.cfg.driver.flat_schema()) {
                            crumbs.push((Some(Ico::Schema), t.schema.clone()));
                        }
                        let is_view = c
                            .conn
                            .and_then(|id| self.table_info(id, c.db.as_deref(), &t.schema, &t.table))
                            .is_some_and(|i| i.kind == crate::db::ObjectKind::View);
                        crumbs.push((Some(Ico::Folder), if is_view { tr("views") } else { tr("tables") }.into()));
                        crumbs.push((Some(if is_view { Ico::View } else { Ico::Table }), t.table.clone()));
                    }
                    None => crumbs.push((Some(Ico::Console), c.title.clone())),
                }
            }
            let n = crumbs.len();
            for (i, (ico, text)) in crumbs.into_iter().enumerate() {
                breadcrumb(ui, ico, &text);
                if i + 1 < n {
                    let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
                    icons::paint(ui.painter(), r.shrink(1.0), Ico::ChevronRight, p.muted, &p, true);
                }
            }
            if let Some(c) = conn
                && !matches!(c.status, ConnStatus::Connected)
            {
                ui.add_space(8.0);
                let color = match c.status {
                    ConnStatus::Error(_) => p.error,
                    ConnStatus::Connecting => p.warn,
                    _ => p.muted,
                };
                ui.label(RichText::new(c.status.label()).color(color).size(12.0));
            }

                }
                theme::Side::Right => {
                ui.add_space(12.0);
                ui.spacing_mut().item_spacing.x = 4.0;
                let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                icons::paint(ui.painter(), r, Ico::Lock, p.text_muted, &p, true);
                ui.add_space(6.0);
                if (self.zoom - 1.0).abs() > 0.001 {
                    let resp = theme::flat_button(ui, None, &zoom_label(self.zoom), FontId::proportional(12.5), false, true)
                        .on_hover_text(crate::shortcuts::format(tr("IDE zoom (⌘0 to reset)")));
                    egui::Popup::menu(&resp).show(|ui| self.zoom_menu(ui));
                    ui.add_space(4.0);
                }
                ui.style_mut().visuals.widgets.inactive.bg_stroke = egui::Stroke::NONE;
                ui.style_mut().visuals.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
                ui.style_mut().visuals.widgets.inactive.bg_fill = egui::Color32::TRANSPARENT;
                ui.add(
                    egui::DragValue::new(&mut self.max_rows)
                        .range(1..=1_000_000)
                        .speed(10)
                        .prefix(tr("max rows: ")),
                )
                .on_hover_text(tr("Maximum rows per query (drag or double-click to edit)"));
                if self.object_browser.active {
                    let connected = self.object_browser.scope.as_ref().and_then(|scope| self.conn(scope.conn)).is_some_and(|connection| connection.status == ConnStatus::Connected);
                    if let Some(action) = self.object_browser.status_controls(ui, connected) { self.handle_explorer(ui.ctx(), vec![action]); }
                    theme::toolbar_separator(ui);
                }
                if let Some(c) = self.consoles.get(self.active) {
                    if c.table.is_none() {
                        let (l, col) = c.line_col();
                        ui.add_space(8.0);
                        ui.label(RichText::new(format!("{l}:{col}")).color(p.text_muted).size(12.5));
                    }
                    if let Some(r) = c.results.last() {
                        ui.add_space(8.0);
                        let text = if r.rs.is_command() {
                            results::affected_message(&r.rs)
                        } else {
                            trf("{n} rows · {time}", &[("n", &r.row_count()), ("time", &results::format_duration(r.rs.elapsed))])
                        };
                        ui.label(RichText::new(text).color(p.text_muted).size(12.5));
                    }
                }
                if let Some((msg, is_err)) = &self.status_msg {
                    ui.add_space(8.0);
                    let color = if *is_err { p.error } else { p.text_muted };
                    // The notice cannot take the whole bar: a third at most.
                    let max_w = (ui.available_width() / 3.0).max(80.0);
                    let resp = ui
                        .scope(|ui| {
                            ui.set_max_width(max_w);
                            ui.add(egui::Label::new(RichText::new(msg).color(color).size(12.5)).truncate().sense(Sense::click()))
                        })
                        .inner;
                    if resp.on_hover_text(tr("Click to dismiss")).clicked() {
                        self.status_msg = None;
                    }
                }
                }
            });
        });
    }

    /// Editor tabs (at the top of the editor island).
    fn editor_tabs(&mut self, ui: &mut Ui) {
        let mut close = None;
        let mut activate = None;
        let mut start_rename = None;
        let mut new = false;
        let mut close_all = false;
        let mut close_tabs = None;
        let mut close_objects = false;
        let w = ui.available_width();
        ui.allocate_ui_with_layout(vec2(w, 40.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.set_min_size(vec2(w, 40.0));
            ui.add_space(6.0);
            let scroll_w = (w - 46.0).max(60.0);
            ui.allocate_ui_with_layout(vec2(scroll_w, 40.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                egui::ScrollArea::horizontal().id_salt("editor_tabs").max_width(scroll_w).show(ui, |ui| {
                    ui.horizontal_centered(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        if self.object_browser.active || self.object_browser.scope.is_some() {
                            let (response, _) = theme::tab(ui, self.object_browser.active, Some(Ico::Folder), tr("Objects"), false);
                            if response.clicked() { self.object_browser.active = true; }
                            response.context_menu(|ui| {
                                if ui.button(tr("Close")).clicked() { close_objects = true; ui.close(); }
                                for label in [tr("Close others"), tr("Close to the right")] {
                                    if ui.add_enabled(!self.consoles.is_empty(), egui::Button::new(label)).clicked() { close_tabs = Some((0, CloseResults::All)); ui.close(); }
                                }
                                ui.add_enabled(false, egui::Button::new(tr("Close to the left")));
                                if ui.button(tr("Close all")).clicked() { close_all = true; ui.close(); }
                            });
                        }
                        for (i, c) in self.consoles.iter().enumerate() {
                            if let Some((id, name)) = self.renaming.as_mut()
                                && *id == c.id
                            {
                                let resp = ui.add(egui::TextEdit::singleline(name).desired_width(140.0).margin(vec2(6.0, 4.0)));
                                resp.request_focus();
                                let (enter, esc) = ui.input(|i| (i.key_pressed(Key::Enter), i.key_pressed(Key::Escape)));
                                if esc {
                                    self.renaming = None;
                                } else if enter || resp.lost_focus() {
                                    let name = name.trim().to_string();
                                    let id = *id;
                                    self.renaming = None;
                                    if !name.is_empty() {
                                        ui.memory_mut(|m| m.data.insert_temp(egui::Id::new("rename_result"), (id, name)));
                                    }
                                }
                                continue;
                            }
                            let ico = if let Some(editor) = &c.routine {
                                if editor.reference.kind == crate::db::routines::RoutineKind::Function { Ico::Function } else { Ico::Procedure }
                            } else if c.table.is_some() { Ico::Table } else { Ico::Console };
                            let busy = c.is_busy();
                            let title = if busy {
                                format!("{} …", c.title)
                            } else if c.has_pending_edits() {
                                format!("{} *", c.title)
                            } else {
                                c.title.clone()
                            };
                            let (resp, closed) = theme::tab(ui, !self.object_browser.active && i == self.active, Some(ico), &title, true);
                            if closed {
                                close = Some(i);
                            } else if resp.clicked() {
                                activate = Some(i);
                            }
                            if resp.double_clicked() && c.table.is_none() {
                                start_rename = Some((c.id, c.title.clone()));
                            }
                            if resp.middle_clicked() {
                                close = Some(i);
                            }
                            resp.context_menu(|ui| {
                                if ui.add_enabled(c.table.is_none(), egui::Button::new(tr("Rename"))).clicked() {
                                    start_rename = Some((c.id, c.title.clone()));
                                    ui.close();
                                }
                                if ui.button(tr("Close")).clicked() {
                                    close = Some(i);
                                    ui.close();
                                }
                                for (label, which, enabled) in [
                                    (tr("Close others"), CloseResults::Others(i), self.consoles.len() > 1 || self.object_browser.scope.is_some()),
                                    (tr("Close to the left"), CloseResults::Left(i), i > 0 || self.object_browser.scope.is_some()),
                                    (tr("Close to the right"), CloseResults::Right(i), i + 1 < self.consoles.len()),
                                    (tr("Close all"), CloseResults::All, true),
                                ] {
                                    if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                                        close_tabs = Some((c.id, which));
                                        ui.close();
                                    }
                                }
                            });
                        }
                    });
                });
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(4.0);
                let more = theme::icon_button(ui, Ico::MoreVert, true, tr("Tabs"));
                egui::Popup::menu(&more).show(|ui| {
                    if ui.add(egui::Button::new(tr("New console")).shortcut_text(crate::shortcuts::format("⇧⌘N"))).clicked() {
                        new = true;
                        ui.close();
                    }
                    if ui.add_enabled(!self.consoles.is_empty(), egui::Button::new(tr("Close all"))).clicked() {
                        close_all = true;
                        ui.close();
                    }
                    if !self.consoles.is_empty() {
                        ui.separator();
                        for (i, c) in self.consoles.iter().enumerate() {
                            if ui.selectable_label(i == self.active, &c.title).clicked() {
                                activate = Some(i);
                                ui.close();
                            }
                        }
                    }
                });
            });
        });
        if let Some((id, name)) = ui.memory_mut(|m| m.data.remove_temp::<(u64, String)>(egui::Id::new("rename_result")))
            && let Some(c) = self.console_by_id(id)
        {
            c.title = name;
        }
        if let Some(i) = activate {
            self.active = i;
            self.object_browser.active = false;
        }
        if start_rename.is_some() {
            self.renaming = start_rename;
        }
        if let Some(i) = close {
            self.close_console(i);
        }
        if close_all && self.guard(Discard::CloseAll) {
            self.discard_and_do(Discard::CloseAll);
        }
        if let Some((id, which)) = close_tabs && self.guard(Discard::CloseTabs(id, which)) { self.close_tabs_now(id, which); }
        if close_objects { self.object_browser.scope = None; self.object_browser.active = false; }
        if new {
            self.new_console_here();
        }
    }

    fn central(&mut self, ui: &mut Ui) {
        let p = palette(self.dark);
        if self.object_browser.active {
            let mut actions = Vec::new();
            theme::island_frame(&p).show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                actions.extend(self.object_browser.show_inspector(ui, &self.conns));
                self.editor_tabs(ui);
                separator(ui, p.island_border);
                actions.extend(self.object_browser.show(ui, &self.conns));
            });
            for action in actions {
                match action {
                    crate::ui::object_browser::BrowserAction::Search(request) => self.worker.send(*request),
                    crate::ui::object_browser::BrowserAction::Explorer(action) => self.handle_explorer(ui.ctx(), vec![action]),
                    crate::ui::object_browser::BrowserAction::Inspect { table, count_rows } => self.inspect_object(table, count_rows),
                }
            }
            return;
        }
        // Deferred loading of restored data editors.
        if let Some(c) = self.consoles.get(self.active)
            && c.table.as_ref().is_some_and(|t| !t.loaded)
            && !c.is_busy()
        {
            self.reload_table(self.active);
        }
        if self.consoles.get(self.active).and_then(|c| c.table.as_ref()).is_some_and(|t| !t.info_requested) { self.request_table_details(self.active, false, false); }
        // Schema of the tab's database (autocomplete, keys and editing).
        if let Some(c) = self.consoles.get(self.active)
            && let Some(conn) = c.conn
        {
            let db = c.db.clone();
            self.ensure_db_loaded(conn, db.as_deref());
        }
        let is_console = self.consoles.get(self.active).is_some_and(|c| c.table.is_none() && c.routine.is_none());
        let mut actions = Vec::new();
        let conn_list = self.conn_list();

        if is_console && self.show_results {
            // The editor (tabs + toolbar + a few lines) always keeps its room: the results
            // panel cannot grow past what is left, even with short windows.
            let avail = ui.available_height();
            let max_results = (avail - 170.0).max(avail * 0.45).max(60.0);
            let min_results = 120.0_f32.min(max_results);
            egui::Panel::bottom(egui::Id::new("results_island"))
                .resizable(true)
                .default_size(300.0_f32.min(max_results))
                .size_range(min_results..=max_results)
                .show_separator_line(false)
                .frame(egui::Frame::NONE.inner_margin(Margin { top: 2 * theme::ISLAND_GAP, ..Margin::ZERO }))
                .show(ui, |ui| {
                    theme::fixed_island(ui, &p, |ui| {
                        let idx = self.active;
                        let console = &self.consoles[idx];
                        let conn = console.conn.and_then(|id| self.conns.iter().find(|c| c.cfg.id == id));
                        let cx = ConsoleCtx {
                            connections: &conn_list,
                            schemas: conn.and_then(|c| c.schemas(console.db.as_deref())),
                            driver: conn.map(|c| c.cfg.driver).unwrap_or(Driver::Postgres),
                            dark: self.dark,
                            font_size: self.font_size,
                            columns: None,
                            auto_complete: self.auto_complete,
                        };
                        actions.extend(self.consoles[idx].show_results(ui, &cx));
                    });
                });
        }

        theme::island_frame(&p).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            self.editor_tabs(ui);
            separator(ui, p.island_border);
            if self.consoles.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() / 3.0);
                    ui.label(RichText::new(tr("No open tabs")).color(p.muted));
                    ui.add_space(4.0);
                    ui.label(RichText::new(crate::shortcuts::format(tr("New console  ⇧⌘N"))).color(p.muted));
                    ui.add_space(8.0);
                    if theme::primary_button(ui, tr("New console")).clicked() {
                        self.new_console_here();
                    }
                });
                return;
            }
            let idx = self.active.min(self.consoles.len() - 1);
            self.active = idx;
            let console = &self.consoles[idx];
            let conn = console.conn.and_then(|id| self.conns.iter().find(|c| c.cfg.id == id));
            let schemas = conn.and_then(|c| c.schemas(console.db.as_deref()));
            let columns = console.table.as_ref().and_then(|t| {
                schemas?
                    .iter()
                    .find(|s| s.name == t.schema)?
                    .tables
                    .iter()
                    .find(|x| x.name == t.table)
                    .map(|t| t.columns.as_slice())
            });
            let cx = ConsoleCtx {
                connections: &conn_list,
                schemas,
                driver: conn.map(|c| c.cfg.driver).unwrap_or(Driver::Postgres),
                dark: self.dark,
                font_size: self.font_size,
                columns,
                auto_complete: self.auto_complete,
            };
            let console = &mut self.consoles[idx];
            let out = if console.routine.is_some() { console.show_routine(ui, &cx) } else if console.table.is_some() { console.show_table(ui, &cx) } else { console.show_console(ui, &cx) };
            actions.extend(out);
        });

        let idx = self.active;
        self.sync_save_on_row_change();
        for a in actions {
            match a {
                ConsoleAction::SaveRoutine => self.save_routine_tab(idx),
                ConsoleAction::ApplyFilter => {
                    if let Some(id) = self.consoles.get(idx).map(|c| c.id) && self.guard(Discard::ApplyFilter(id)) { self.apply_filter_now(idx); }
                }
                ConsoleAction::Page(page) => {
                    if let Some(id) = self.consoles.get(idx).map(|c| c.id) && self.guard(Discard::Page(id, page)) { self.navigate_page(idx, page); }
                }
                ConsoleAction::Inspect(count_rows) => self.request_table_details(idx, count_rows, false),
                ConsoleAction::Run => self.run_console(idx, false),
                ConsoleAction::RunAll => self.run_console(idx, true),
                ConsoleAction::Cancel => {
                    let console = self.consoles[idx].id;
                    self.worker.send(Request::Cancel { console });
                }
                ConsoleAction::ExplainAnalyze { statement, explain } => {
                    let id = self.consoles[idx].id;
                    if self.consoles[idx].table.is_none() && !self.consoles[idx].is_busy() && self.guard(Discard::Run(id, false)) {
                        if is_query(&statement) {
                            self.run_statements(idx, vec![explain]);
                        } else {
                            self.confirm_analyze = Some((idx, explain));
                        }
                    }
                }
                ConsoleAction::RunStatements(statements) => {
                    let id = self.consoles[idx].id;
                    if self.consoles[idx].table.is_none() && !self.consoles[idx].is_busy() && self.guard(Discard::Run(id, false)) {
                        self.run_statements(idx, statements);
                    }
                }
                ConsoleAction::Reload => {
                    if let Some(id) = self.consoles.get(idx).map(|c| c.id)
                        && self.guard(Discard::Reload(id))
                    {
                        self.reload_table(idx);
                    }
                }
                ConsoleAction::Ddl => {
                    if let Some(c) = self.consoles.get(idx)
                        && let (Some(conn), Some(t)) = (c.conn, c.table.as_ref())
                        && let Some(db) = self.conn(conn).and_then(|x| x.resolve_db(c.db.as_deref()))
                    {
                        let tref = TableRef { conn, db: db.to_string(), schema: t.schema.clone(), table: t.table.clone() };
                        self.generate(ui.ctx(), &tref, Generate::Ddl);
                    }
                }
                ConsoleAction::Connect(id) => self.connect(id),
                ConsoleAction::Error(e) => self.error(e),
                ConsoleAction::Preview(view) => self.open_preview(idx, view),
                ConsoleAction::Submit(view) => self.submit_now(idx, view),
                ConsoleAction::CloseResults(which) => self.close_results(idx, which),
                ConsoleAction::ExportMasked => self.open_mask_export(idx),
            }
        }
    }

    fn conn_list(&self) -> Vec<ConnSummary> {
        self.conns.iter().map(ConnState::summary).collect()
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        self.show_table_actions(ctx);
        if let Some(dialog) = self.scheduler.as_mut() {
            let configs: Vec<_> = self.conns.iter().map(|connection| connection.cfg.clone()).collect();
            if dialog.show(ctx, &configs) { self.scheduler = None; }
        }
        if let Some(d) = self.script.as_mut()
            && let Some(ran) = d.show(ctx)
        {
            let conn = d.conn;
            self.script = None;
            if ran {
                self.worker.send(Request::Refresh(conn));
            }
        }
        self.update_notice(ctx);
        if let Some(dialog) = self.backup.as_mut() {
            match dialog.show(ctx, &self.explorer.backups) {
                Some(crate::ui::backup_dialog::BackupAction::Start(request)) => self.worker.send(Request::Backup { conn: dialog.conn, id: dialog.id, request: Box::new(request), cancel: dialog.cancel.clone() }),
                Some(crate::ui::backup_dialog::BackupAction::Close) => self.backup = None,
                None => {}
            }
        }

        if let Some(dialog) = self.dialog.as_mut() {
            match dialog.show(ctx) {
                Some(DialogResult::Save { save, delete, close }) => {
                    if close {
                        self.dialog = None;
                    }
                    for id in delete {
                        self.delete_connection(id);
                    }
                    for cfg in save {
                        self.save_connection(cfg);
                    }
                }
                Some(DialogResult::Test(cfg)) => self.worker.send(Request::Test(*cfg)),
                Some(DialogResult::Cancel) => self.dialog = None,
                None => {}
            }
        }

        if let Some(d) = self.create.as_ref() {
            let schemas: Vec<String> = match &d.target {
                CreateTarget::Table { db, .. } => {
                    self.conn(d.conn).and_then(|c| c.schemas(Some(db))).unwrap_or_default().iter().map(|s| s.name.clone()).collect()
                }
                CreateTarget::Database => vec![],
            };
            if let Some(d) = self.create.as_mut() {
                match d.show(ctx, &schemas) {
                    Some(CreateResult::Create(statements)) => {
                        d.running = true;
                        d.error = None;
                        let db = match &d.target {
                            CreateTarget::Table { db, .. } => Some(db.clone()),
                            CreateTarget::Database => None,
                        };
                        let conn = d.conn;
                        self.worker.send(Request::RunDdl { conn, db, statements, close_db: None, transaction: false });
                    }
                    Some(CreateResult::Cancel) => self.create = None,
                    None => {}
                }
            }
        }

        self.view_dialog(ctx);
        self.routine_dialogs(ctx);
        self.user_dialog_ui(ctx);

        if let Some(d) = self.import.as_mut() {
            match d.show(ctx) {
                Some(ImportResult::Import(statements)) => {
                    let (conn, db) = (d.conn, Some(d.db.clone()));
                    self.worker.send(Request::RunDdl { conn, db, statements, close_db: None, transaction: true });
                }
                Some(ImportResult::Cancel) => self.import = None,
                None => {}
            }
        }

        if let Some(d) = self.mask_export.as_mut() {
            match d.show(ctx) {
                Some(MaskExportAction::Save) => self.run_mask_export(ctx, true),
                Some(MaskExportAction::Copy) => self.run_mask_export(ctx, false),
                Some(MaskExportAction::Close) => self.mask_export = None,
                None => {}
            }
        }

        if let Some(d) = self.copy.as_mut() {
            match d.show(ctx, &self.conns) {
                Some(CopyAction::Start(request)) => self.start_copy(*request),
                Some(CopyAction::Load(conn, db)) => self.ensure_db_loaded(conn, Some(&db)),
                Some(CopyAction::Close) => self.copy = None,
                None => {}
            }
        }

        if let Some(d) = self.modify.as_mut() {
            match d.show(ctx) {
                Some(ModifyResult::Apply(plan)) => {
                    d.running = true;
                    d.error = None;
                    let (conn, db) = (d.conn, Some(d.db.clone()));
                    self.worker.send(Request::RunDdl { conn, db, statements: plan.statements, close_db: None, transaction: plan.transaction });
                }
                Some(ModifyResult::Cancel) => self.modify = None,
                None => {}
            }
        }

        if let Some(d) = self.drop.as_mut() {
            match d.show(ctx) {
                Some(DropResult::Drop(statements)) => {
                    d.running = true;
                    d.error = None;
                    let (conn, db, close_db) = match d.kind {
                        // Run from the connection's default database, never from the one being dropped.
                        DropKind::Database => (d.conn, None, Some(d.db.clone())),
                        _ => (d.conn, Some(d.db.clone()), None),
                    };
                    self.worker.send(Request::RunDdl { conn, db, statements, close_db, transaction: false });
                }
                Some(DropResult::Cancel) => self.drop = None,
                None => {}
            }
        }

        let p = palette(self.dark);
        let frame = egui::Frame::popup(&ctx.global_style())
            .fill(p.popup_bg)
            .corner_radius(12)
            .inner_margin(Margin::same(20))
            .stroke(egui::Stroke::new(1.0, p.popup_border));

        if let Some(id) = self.confirm_delete {
            let name = self.conn(id).map(|c| c.cfg.name.clone()).unwrap_or_default();
            let mut decision = None;
            egui::Modal::new(egui::Id::new("confirm_delete")).frame(frame).show(ctx, |ui| {
                ui.set_width(360.0);
                ui.label(RichText::new(tr("Delete data source")).font(theme::bold(15.0)));
                ui.add_space(8.0);
                ui.label(trf("Delete “{name}”? This action cannot be undone.", &[("name", &name)]));
                ui.add_space(16.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if theme::primary_button(ui, tr("Delete")).clicked() {
                        decision = Some(true);
                    }
                    if theme::secondary_button(ui, tr("Cancel")).clicked() {
                        decision = Some(false);
                    }
                });
            });
            if ctx.input(|i| i.key_pressed(Key::Escape)) {
                decision = Some(false);
            }
            match decision {
                Some(true) => {
                    self.confirm_delete = None;
                    self.delete_connection(id);
                }
                Some(false) => self.confirm_delete = None,
                None => {}
            }
        }

        if let Some((id, password)) = self.ask_password.as_mut() {
            let id = *id;
            let name = self.conns.iter().find(|c| c.cfg.id == id).map(|c| c.cfg.name.clone()).unwrap_or_default();
            let mut decision = None;
            egui::Modal::new(egui::Id::new("ask_password")).frame(frame).show(ctx, |ui| {
                ui.set_width(380.0);
                ui.label(RichText::new(tr("Password")).font(theme::bold(15.0)));
                ui.add_space(8.0);
                ui.label(trf("“{name}” was imported without its password. It is kept in the system keychain.", &[("name", &name)]));
                ui.add_space(10.0);
                let edit = ui.add(egui::TextEdit::singleline(password).password(true).margin(egui::vec2(8.0, 5.0)).desired_width(f32::INFINITY));
                if !edit.has_focus() && !edit.lost_focus() {
                    edit.request_focus();
                }
                if edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    decision = Some(true);
                }
                ui.add_space(16.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if theme::primary_button(ui, tr("Connect")).clicked() {
                        decision = Some(true);
                    }
                    if theme::secondary_button(ui, tr("Cancel")).clicked() {
                        decision = Some(false);
                    }
                });
            });
            if ctx.input(|i| i.key_pressed(Key::Escape)) {
                decision = Some(false);
            }
            match decision {
                Some(true) => {
                    let password = self.ask_password.take().map(|(_, p)| p).unwrap_or_default();
                    if let Some(c) = self.conn_mut(id) {
                        c.cfg.password = password;
                        c.cfg.ask_password = false;
                        config::store_secrets(&c.cfg);
                    }
                    self.persist_connections();
                    self.connect(id);
                }
                Some(false) => self.ask_password = None,
                None => {}
            }
        }

        if let Some((idx, explain)) = self.confirm_analyze.clone() {
            let mut decision = None;
            egui::Modal::new(egui::Id::new("confirm_analyze")).frame(frame).show(ctx, |ui| {
                ui.set_width(400.0);
                ui.label(RichText::new(tr("Explain Analyze")).font(theme::bold(15.0)));
                ui.add_space(8.0);
                ui.label(tr("Explain Analyze runs the statement to measure it, and this one changes data or structure. Run it anyway?"));
                ui.add_space(16.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if theme::primary_button(ui, tr("Run")).clicked() {
                        decision = Some(true);
                    }
                    if theme::secondary_button(ui, tr("Cancel")).clicked() {
                        decision = Some(false);
                    }
                });
            });
            if ctx.input(|i| i.key_pressed(Key::Escape)) {
                decision = Some(false);
            }
            match decision {
                Some(true) => {
                    self.confirm_analyze = None;
                    if idx < self.consoles.len() && !self.consoles[idx].is_busy() {
                        self.run_statements(idx, vec![explain]);
                    }
                }
                Some(false) => self.confirm_analyze = None,
                None => {}
            }
        }

        if let Some(action) = self.confirm_discard {
            let mut decision = None;
            egui::Modal::new(egui::Id::new("confirm_discard")).frame(frame).show(ctx, |ui| {
                ui.set_width(380.0);
                ui.label(RichText::new(tr("Unsaved changes")).font(theme::bold(15.0)));
                ui.add_space(8.0);
                ui.label(match action {
                    Discard::Close(_) | Discard::CloseAll | Discard::CloseTabs(..) => if self.consoles.iter().any(|c| c.routine.is_some() && c.has_pending_edits()) { tr("Unsaved changes") } else { tr("Closing the tab will discard the pending grid changes.") },
                    Discard::Reload(_) | Discard::ApplyFilter(_) | Discard::Page(..) => tr("Reloading the data will discard the pending grid changes."),
                    Discard::CloseResults(..) => tr("Closing the result tab will discard the pending grid changes."),
                    Discard::Run(..) => tr("Running again will discard the pending grid changes."),
                });
                ui.add_space(16.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if theme::primary_button(ui, tr("Discard")).clicked() {
                        decision = Some(true);
                    }
                    if theme::secondary_button(ui, tr("Cancel")).clicked() {
                        decision = Some(false);
                    }
                });
            });
            if ctx.input(|i| i.key_pressed(Key::Escape)) {
                decision = Some(false);
            }
            match decision {
                Some(true) => {
                    self.confirm_discard = None;
                    self.discard_and_do(action);
                }
                Some(false) => self.confirm_discard = None,
                None => {}
            }
        }

        self.preview_dialog(ctx, frame);

        if self.show_shortcuts {
            let resp = egui::Modal::new(egui::Id::new("keyboard_shortcuts")).frame(frame).show(ctx, |ui| {
                ui.set_width(460.0);
                ui.label(RichText::new(tr("Keyboard shortcuts")).font(theme::bold(17.0)));
                ui.add_space(8.0);
                egui::ScrollArea::vertical().max_height(430.0).show(ui, |ui| {
                    ui.set_width(430.0);
                    for (section, rows) in [
                        (tr("General"), &[
                            (tr("New console"), "⇧⌘N"), (tr("Quit"), "⌘Q"),
                            (tr("Zoom In"), "⌘+"), (tr("Zoom Out"), "⌘-"),
                            (tr("Reset (100%)"), "⌘0"), (tr("Increase"), "⌥⌘+"),
                            (tr("Decrease"), "⌥⌘-"),
                        ][..]),
                        (tr("Edit"), &[
                            (tr("Undo"), "⌘Z"), (tr("Redo"), "⇧⌘Z"),
                            (tr("Cut"), "⌘X"), (tr("Copy"), "⌘C"),
                            (tr("Paste"), "⌘V"), (tr("Select all"), "⌘A"),
                        ][..]),
                        (tr("SQL editor"), &[
                            (tr("Run statement"), "⌘↩"), (tr("Run all"), "⇧⌘↩"),
                            (tr("Explain Plan"), "⇧⌘E"), (tr("Reformat Code"), "⌥⌘L"),
                            (tr("Comment with Line Comment"), "⌘/"), (tr("Duplicate Line or Selection"), "⌘D"),
                            (tr("Delete Line"), if cfg!(target_os = "macos") { "⌘⌫" } else { "Ctrl+Y" }),
                            (tr("Find…"), "⌘F"), (tr("Replace…"), "⌘R"),
                            (tr("Query History"), "⌥⌘E"), (tr("Show completions"), "Ctrl+Space"),
                            (tr("Next match"), "⌘G / F3"),
                            (tr("Previous match"), "⇧⌘G / ⇧F3"),
                        ][..]),
                        (tr("Results panel"), &[
                            (tr("Find…"), "⌘F"), (tr("Reload data"), "⌘R"),
                            (tr("Submit changes"), "⌘S"), (tr("Undo"), "⌘Z"),
                            (tr("Clone Row"), "⌘D"), (tr("Add row"), "⌘N / Insert"),
                            (tr("Delete row"), "⌘Delete"), (tr("Record View"), "⇧↩"),
                            (tr("Edit cell"), "Enter / F2 / ⌘↩"), (tr("View Value"), "⌥Click"),
                            (tr("Select all"), "⌘A"), (tr("Set NULL"), "Delete"),
                            (tr("Move between cells"), "Arrows / Tab / ⇧Tab"),
                            (tr("Extend selection"), "⇧Arrows"),
                            (tr("First or last column"), "⌘Left / ⌘Right / Home / End"),
                            (tr("First or last row"), "⌘Up / ⌘Down / ⌘Home / ⌘End"),
                            (tr("First or last row"), "⌘Page Up / ⌘Page Down"),
                            (tr("Move by page"), "Page Up / Page Down"),
                            (tr("Discard current record changes"), "Esc"),
                        ][..]),
                    ] {
                        ui.add_space(8.0);
                        ui.label(RichText::new(section).strong());
                        ui.separator();
                        for (label, keys) in rows {
                            ui.horizontal(|ui| {
                                ui.label(*label);
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    ui.label(RichText::new(crate::shortcuts::format(keys)).monospace().color(p.muted));
                                });
                            });
                        }
                    }
                    if cfg!(target_os = "macos") {
                        ui.add_space(8.0);
                        ui.label(RichText::new("macOS").strong());
                        ui.separator();
                        for (label, keys) in [
                            (tr("Settings…"), "⌘,"), (tr("Open SQL script…"), "⌘O"),
                            (tr("Save console as…"), "⇧⌘S"), (tr("Close tab"), "⌘W"),
                        ] {
                            ui.horizontal(|ui| {
                                ui.label(label);
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    ui.label(RichText::new(keys).monospace().color(p.muted));
                                });
                            });
                        }
                    }
                });
                ui.add_space(10.0);
                theme::primary_button(ui, tr("Close")).clicked()
            });
            if resp.inner || resp.should_close() { self.show_shortcuts = false; }
        }

        if self.show_about {
            let resp = egui::Modal::new(egui::Id::new("about")).frame(frame).show(ctx, |ui| {
                ui.set_width(320.0);
                ui.vertical_centered(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(44.0, 44.0), Sense::hover());
                    crate::ui::branding::paint(ui, r);
                    ui.add_space(8.0);
                    ui.label(RichText::new("DataCraft").font(theme::bold(17.0)));
                    ui.label(RichText::new(trf("Version {version}", &[("version", &env!("CARGO_PKG_VERSION"))])).color(p.muted));
                    ui.add_space(8.0);
                    ui.label(tr("Lightweight database client for PostgreSQL, MySQL, MariaDB, SQL Server, Oracle, SQLite, Snowflake, ClickHouse, Redis, MongoDB, InfluxDB and Kusto."));
                    ui.label(RichText::new("macOS · Windows · Linux").small().color(p.muted));
                    ui.label(RichText::new(tr("Made with Rust, egui and tokio")).small().color(p.muted));
                    ui.add_space(14.0);
                    theme::primary_button(ui, tr("Close")).clicked()
                })
                .inner
            });
            if resp.inner || resp.should_close() {
                self.show_about = false;
            }
        }
        if self.show_settings {
            let resp = egui::Modal::new(egui::Id::new("settings")).frame(frame).show(ctx, |ui| {
                ui.set_width(300.0);
                ui.label(RichText::new(tr("Settings")).font(theme::bold(15.0)));
                ui.add_space(8.0);
                self.view_menu(ui);
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(tr("Max rows per query:"));
                    ui.add(egui::DragValue::new(&mut self.max_rows).range(1..=1_000_000).speed(10));
                });
                ui.checkbox(&mut self.auto_complete, tr("Show completions while typing"));
                ui.checkbox(&mut self.check_updates, tr("Check for updates at startup"));
                ui.separator();
                self.perf.settings_ui(ui);
                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| theme::primary_button(ui, tr("Close")).clicked())
                    .inner
            });
            if resp.inner || resp.should_close() {
                self.show_settings = false;
            }
        }
    }
}

impl DataCraftApp {
    /// "Check for Updates…": the result is shown even for a skipped version, and when up to date.
    fn check_for_updates(&mut self, ctx: &egui::Context) {
        self.info(tr("Checking for updates…"));
        self.update_check = Some((crate::update::check(ctx.clone(), std::time::Duration::ZERO), true));
    }

    /// The update check's result, and the notice of a newer release in the bottom-right corner.
    fn update_notice(&mut self, ctx: &egui::Context) {
        if let Some((rx, asked)) = &self.update_check
            && let Ok(result) = rx.try_recv()
        {
            let asked = *asked;
            self.update_check = None;
            match result {
                Ok(r) if crate::update::is_newer(&r.version) => {
                    if asked || self.skipped_version.as_deref() != Some(r.version.as_str()) {
                        self.update = Some(r);
                    }
                    if asked {
                        self.status_msg = None;
                    }
                }
                Ok(_) if asked => self.info(trf("DataCraft {version} is up to date", &[("version", &env!("CARGO_PKG_VERSION"))])),
                Err(e) if asked => self.error(trf("Could not check for updates: {error}", &[("error", &e)])),
                _ => {}
            }
        }
        // Not over a dialog: it comes back when the dialog closes.
        let Some(release) = self.update.clone().filter(|_| !self.modal_open()) else { return };
        let p = palette(self.dark);
        let mut close = false;
        egui::Area::new(egui::Id::new("update_notice")).anchor(egui::Align2::RIGHT_BOTTOM, vec2(-16.0, -40.0)).order(egui::Order::Foreground).show(ctx, |ui| {
            egui::Frame::popup(ui.style()).fill(p.popup_bg).corner_radius(10).inner_margin(Margin::same(14)).stroke(egui::Stroke::new(1.0, p.popup_border)).show(ui, |ui| {
                ui.set_width(300.0);
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::hover());
                    crate::ui::branding::paint(ui, r);
                    ui.vertical(|ui| {
                        ui.label(RichText::new(trf("DataCraft {version} is available", &[("version", &release.version)])).strong());
                        ui.label(RichText::new(trf("You have {version}. Download it from the release page.", &[("version", &env!("CARGO_PKG_VERSION"))])).size(12.0).color(p.muted));
                    });
                });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if theme::primary_button(ui, tr("View Release")).clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(&release.url));
                        close = true;
                    }
                    if theme::secondary_button(ui, tr("Skip This Version")).clicked() {
                        self.skipped_version = Some(release.version.clone());
                        close = true;
                    }
                    if theme::secondary_button(ui, tr("Later")).clicked() {
                        close = true;
                    }
                });
            });
        });
        if close {
            self.update = None;
        }
    }

    /// "Preview pending changes" modal window.
    fn preview_dialog(&mut self, ctx: &egui::Context, frame: egui::Frame) {
        let Some(pv) = self.preview.as_ref().filter(|p| p.visible) else { return };
        let p = palette(self.dark);
        let n = pv.statements.len();
        let applying = pv.applying;
        let error = pv.error.clone();
        let title = self
            .consoles
            .iter()
            .find(|c| c.id == pv.console)
            .and_then(|c| c.results.get(pv.view))
            .and_then(|v| v.edit.as_ref())
            .map(|e| format!("{}.{}", e.target.schema, e.target.table))
            .unwrap_or_default();
        let driver = self
            .consoles
            .iter()
            .find(|c| c.id == pv.console)
            .and_then(|c| self.conn(c.conn?))
            .map_or(Driver::Postgres, |c| c.cfg.driver);
        let sql_text = preview_text(driver, &pv.statements);
        let dark = self.dark;
        let mut submit = false;
        let mut cancel = false;
        let resp = egui::Modal::new(egui::Id::new("preview_changes")).frame(frame).show(ctx, |ui| {
            let screen = ctx.content_rect();
            let w = (screen.width() * 0.6).clamp(360.0, 860.0);
            ui.set_width(w);
            ui.horizontal(|ui| {
                ui.label(RichText::new(tr("Preview pending changes")).font(theme::bold(15.0)));
                ui.add_space(8.0);
                let count = if n == 1 { tr("1 change").to_string() } else { trf("{n} changes", &[("n", &n)]) };
                ui.label(RichText::new(format!("{count} · {title}")).color(p.muted));
            });
            ui.add_space(10.0);
            egui::Frame::NONE
                .fill(p.editor_bg)
                .stroke(egui::Stroke::new(1.0, p.control_border))
                .corner_radius(6)
                .inner_margin(Margin::same(8))
                .show(ui, |ui| {
                    let max_h = (screen.height() * 0.5).clamp(120.0, 520.0);
                    egui::ScrollArea::both().max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
                        let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, _w: f32| {
                            let mut job = sql::highlight_for(buf.as_str(), driver, dark, FontId::monospace(12.5));
                            job.wrap.max_width = f32::INFINITY;
                            ui.fonts_mut(|f| f.layout_job(job))
                        };
                        let mut text = sql_text.as_str();
                        ui.add(
                            egui::TextEdit::multiline(&mut text)
                                .id(egui::Id::new("preview_sql"))
                                .code_editor()
                                .frame(egui::Frame::NONE)
                                .desired_width(f32::INFINITY)
                                .desired_rows(4)
                                .layouter(&mut layouter),
                        );
                    });
                });
            if let Some(e) = &error {
                ui.add_space(8.0);
                ui.add(egui::Label::new(RichText::new(e).monospace().color(p.error)).wrap());
            }
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if theme::secondary_button(ui, tr("Copy SQL")).clicked() {
                    ui.ctx().copy_text(sql_text.clone());
                }
                if applying {
                    ui.add_space(8.0);
                    ui.add(egui::Spinner::new().size(14.0).color(p.muted));
                    ui.label(RichText::new(tr("Saving…")).color(p.muted));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.add_enabled_ui(!applying && n > 0, |ui| {
                        if theme::primary_button(ui, tr("Submit")).clicked() {
                            submit = true;
                        }
                    });
                    ui.add_enabled_ui(!applying, |ui| {
                        if theme::secondary_button(ui, tr("Cancel")).clicked() {
                            cancel = true;
                        }
                    });
                });
            });
        });
        if !applying && (resp.should_close() || ctx.input(|i| i.key_pressed(Key::Escape))) {
            cancel = true;
        }
        if cancel {
            self.preview = None;
        } else if submit {
            self.submit_preview();
        }
    }
}

/// The preview window's text: one statement per line, ending in `;` except for Redis, whose
/// command lines have no terminator.
fn preview_text(driver: Driver, statements: &[String]) -> String {
    let end = if driver == Driver::Redis { "" } else { ";" };
    statements.iter().map(|s| format!("{s}{end}")).collect::<Vec<_>>().join("\n")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Generate {
    Select,
    Insert,
    Ddl,
}

/// Demo mode against a server: `DATACRAFT_DEMO=<scheme>://user:password@host:port/database`
/// with scheme `postgres`, `mysql`, `mariadb`, `sqlserver` or `oracle` (database = service).
fn demo_server_config(id: u64, url: &str) -> Option<ConnectionConfig> {
    let (scheme, rest) = url.split_once("://")?;
    let driver = match scheme {
        "postgres" | "postgresql" => Driver::Postgres,
        "mysql" => Driver::MySql,
        "mariadb" => Driver::MariaDb,
        "sqlserver" | "mssql" => Driver::MsSql,
        "oracle" => Driver::Oracle,
        "redis" | "valkey" => Driver::Redis,
        "mongodb" => Driver::MongoDb,
        _ => return None,
    };
    let mut cfg = ConnectionConfig::new(id, driver);
    let (auth, rest) = rest.rsplit_once('@')?;
    let (user, password) = auth.split_once(':').unwrap_or((auth, ""));
    let (hostport, db) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = hostport.rsplit_once(':').unwrap_or((hostport, ""));
    cfg.user = user.to_string();
    cfg.password = password.to_string();
    cfg.host = host.to_string();
    if let Ok(p) = port.parse() {
        cfg.port = p;
    }
    if !db.is_empty() {
        cfg.database = db.to_string();
    }
    cfg.name = format!("{}@{}", driver.label(), host);
    Some(cfg)
}

/// true if a statement only reads, in the engine's own language (SQL, Redis, `mongosh`, Flux,
/// InfluxQL or KQL).
pub(crate) fn is_read_statement(driver: Driver, statement: &str) -> bool {
    match driver {
        Driver::Redis => crate::db::redis_db::is_read_command(statement),
        Driver::MongoDb => crate::db::mongo::is_read_command(statement),
        Driver::InfluxDb => crate::db::influx::is_read_command(statement),
        Driver::Kusto => crate::db::kusto::is_read_command(statement),
        _ => is_query(statement),
    }
}

/// true if the statement only reads (allowed on read-only connections). A `WITH` that
/// contains a data-modifying statement (PostgreSQL) is not.
fn is_query(statement: &str) -> bool {
    let words: Vec<String> = statement
        .lines()
        .map(|l| l.split("--").next().unwrap_or(""))
        .flat_map(|l| l.split(|c: char| !c.is_alphanumeric() && c != '_'))
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    // `SELECT … INTO` creates tables (PG, SQL Server) or writes files (MySQL); `EXPLAIN ANALYZE`
    // runs the statement. `WITH … PROCEDURE … CALL` (Snowflake) runs an anonymous procedure, so
    // DDL, grants and calls count as writes anywhere in the statement.
    let writes = |w: &String| {
        matches!(
            w.as_str(),
            "insert" | "update" | "delete" | "merge" | "into" | "analyze" | "analyse" | "call" | "procedure" | "function"
                | "drop" | "create" | "alter" | "grant" | "revoke" | "execute" | "exec" | "truncate" | "copy" | "put"
                | "remove" | "undrop"
        )
    };
    // `$$…$$` bodies (Snowflake, PostgreSQL) hold code that runs as a block.
    if statement.contains("$$") {
        return false;
    }
    match words.first().map(String::as_str) {
        // SHOW / DESCRIBE never write, whatever they name (`SHOW CREATE TABLE`).
        Some("show" | "describe" | "desc") => true,
        Some("select" | "explain" | "values" | "table" | "with") => !words.iter().any(writes),
        // SQL Server's explain plan.
        Some("set") => words.len() == 3 && words[1] == "showplan_text" && matches!(words[2].as_str(), "on" | "off"),
        _ => false,
    }
}

/// Data editor query with filters and the row limit in the SQL itself.
pub fn table_query(driver: Driver, schema: &str, table: &str, where_clause: &str, order_by: &str, max_rows: usize) -> String {
    let w = where_clause.trim().trim_end_matches(';').trim();
    let o = order_by.trim().trim_end_matches(';').trim();
    // Braces are optional in the filter row: `age: {$gt: 30}` or `{age: {$gt: 30}}`.
    let braces = |s: &str| if s.is_empty() { "{}".to_string() } else if s.starts_with('{') { s.to_string() } else { format!("{{{s}}}") };
    match driver {
        // A key group is its pattern; the filter row can narrow it (`user:1*`).
        Driver::Redis => return format!("SCAN 0 MATCH {} COUNT 1000", if w.is_empty() { table } else { w }),
        Driver::MongoDb => {
            let name = serde_json::to_string(table).unwrap_or_default();
            let sort = if o.is_empty() { String::new() } else { format!(".sort({})", braces(o)) };
            return format!("db.getCollection({name}).find({}){sort}.limit({max_rows})", braces(w));
        }
        // The filter row holds a Flux predicate on `r` and the time range.
        Driver::InfluxDb => return crate::db::influx::table_query(schema, table, w, o, max_rows),
        // A KQL predicate and sort (`State == "TEXAS"`, `StartTime desc`).
        Driver::Kusto => return crate::db::kusto::table_query(table, w, o, max_rows),
        _ => {}
    }
    let t = Session::quote_table(driver, schema, table);
    let mut sql = match driver {
        Driver::MsSql => format!("SELECT TOP ({max_rows}) * FROM {t}"),
        _ => format!("SELECT * FROM {t}"),
    };
    let w = where_clause.trim().trim_end_matches(';');
    if !w.is_empty() {
        sql.push_str(&format!(" WHERE {w}"));
    }
    let o = order_by.trim().trim_end_matches(';');
    if !o.is_empty() {
        sql.push_str(&format!(" ORDER BY {o}"));
    }
    match driver {
        Driver::MsSql => sql,
        // ROWNUM instead of FETCH FIRST so it also works before 12c.
        Driver::Oracle => format!("SELECT * FROM ({sql}) WHERE ROWNUM <= {max_rows}"),
        _ => format!("{sql} LIMIT {max_rows}"),
    }
}

/// Subsequent SQL pages keep the same projection so editing column indices stay valid.
fn table_query_page(driver: Driver, schema: &str, table: &str, w: &str, o: &str, limit: usize, offset: usize) -> String {
    if offset == 0 || driver.is_nosql() { return table_query(driver, schema, table, w, o, limit); }
    if !matches!(driver, Driver::MsSql | Driver::Oracle) { return format!("{} OFFSET {offset}", table_query(driver, schema, table, w, o, limit)); }
    let mut sql = format!("SELECT * FROM {}", Session::quote_table(driver, schema, table));
    if !w.trim().trim_end_matches(';').is_empty() { sql.push_str(&format!(" WHERE {}", w.trim().trim_end_matches(';'))); }
    if !o.trim().trim_end_matches(';').is_empty() { sql.push_str(&format!(" ORDER BY {}", o.trim().trim_end_matches(';'))); }
    else if driver == Driver::MsSql { sql.push_str(" ORDER BY (SELECT NULL)"); }
    format!("{sql} OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY")
}

/// Approximate DDL from the known column information.
fn ddl(driver: Driver, table: &str, columns: &[ColumnInfo]) -> String {
    let mut lines: Vec<String> = columns
        .iter()
        .map(|c| {
            let mut l = format!("    {} {}", quote_ident_if_needed(driver, &c.name), c.data_type.to_lowercase());
            if !c.nullable {
                l.push_str(" not null");
            }
            l
        })
        .collect();
    let pk: Vec<String> =
        columns.iter().filter(|c| c.primary_key).map(|c| quote_ident_if_needed(driver, &c.name)).collect();
    if !pk.is_empty() {
        lines.push(format!("    primary key ({})", pk.join(", ")));
    }
    format!("create table {table}\n(\n{}\n);", lines.join(",\n"))
}

/// Tool window stripe button (37x40 in DataGrip; here 30x30 within a 40 wide stripe).
fn stripe_button(ui: &mut Ui, ico: Ico, active: bool, tooltip: &str) -> egui::Response {
    let p = theme::pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::click());
    if active {
        ui.painter().rect_filled(rect, 7.0, p.stripe_selected);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 7.0, p.hover);
    }
    let color = if active { p.text } else { p.icon };
    icons::paint(ui.painter(), Rect::from_center_size(rect.center(), vec2(18.0, 18.0)), ico, color, &p, true);
    resp.on_hover_text(tooltip)
}

fn breadcrumb(ui: &mut Ui, ico: Option<Ico>, text: &str) {
    let p = theme::pal(ui);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), FontId::proportional(12.5), p.text_muted);
    let w = 6.0 + if ico.is_some() { 20.0 } else { 0.0 } + galley.size().x + 6.0;
    let (r, resp) = ui.allocate_exact_size(vec2(w, 22.0), Sense::hover());
    if resp.hovered() {
        ui.painter().rect_filled(r, 5.0, p.hover);
    }
    let mut x = r.left() + 6.0;
    if let Some(ico) = ico {
        icons::paint(ui.painter(), Rect::from_min_size(pos2(x, r.center().y - 7.0), vec2(14.0, 14.0)), ico, p.icon, &p, true);
        x += 20.0;
    }
    let color = if resp.hovered() { p.text } else { p.text_muted };
    ui.painter().galley(pos2(x, r.center().y - galley.size().y / 2.0), galley, color);
}

fn separator(ui: &mut Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.center().y, egui::Stroke::new(1.0, color));
}

fn now() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// Puts a run's result into the console (tabs + log).
fn apply_execution(
    c: &mut Console,
    results_in: Vec<crate::db::ResultSet>,
    error: Option<String>,
    elapsed: std::time::Duration,
    salt: u64,
) {
    c.running = false;
    c.run_started = None;
    let time = now();
    for rs in &results_in {
        let message = if rs.is_command() {
            trf("{message} in {time}", &[("message", &results::affected_message(rs)), ("time", &results::format_duration(rs.elapsed))])
        } else {
            let mut m = trf("{n} rows retrieved in {time}", &[("n", &rs.rows.len()), ("time", &results::format_duration(rs.elapsed))]);
            if rs.truncated {
                m.push_str(tr(" (limited to max_rows)"));
            }
            m
        };
        c.log.push(OutputEntry { time: time.clone(), statement: rs.statement.clone(), message, is_error: false });
    }
    if let Some(err) = &error {
        let failed = c
            .last_statements
            .iter()
            .find(|s| !results_in.iter().any(|r| r.statement.trim() == s.trim()))
            .cloned()
            .unwrap_or_default();
        c.log.push(OutputEntry { time: time.clone(), statement: failed, message: err.clone(), is_error: true });
    } else if c.last_statements.len() > 1 {
        c.log.push(OutputEntry {
            time,
            statement: String::new(),
            message: trf("{n} statements executed in {time}", &[("n", &c.last_statements.len()), ("time", &results::format_duration(elapsed))]),
            is_error: false,
        });
    }
    if c.log.len() > 1000 {
        let extra = c.log.len() - 1000;
        c.log.drain(..extra);
    }

    // Reload after saving changes: replaces only the edited result.
    let replace = c.replace_view.take();
    if let Some(i) = replace
        && error.is_none()
        && i < c.results.len()
        && results_in.len() == 1
        && let Some(rs) = results_in.first().cloned()
    {
        c.results[i].refresh(rs);
        c.result_tab = ResultTab::Result(i);
        return;
    }
    let has_queries = results_in.iter().any(|r| !r.is_command());
    let mut views: Vec<ResultView> = Vec::new();
    let last_command = results_in.iter().rev().find(|r| r.is_command()).cloned();
    for (i, rs) in results_in.into_iter().enumerate() {
        if !rs.is_command() {
            views.push(ResultView::new(rs, salt * 1000 + i as u64));
        }
    }
    if !has_queries && let Some(rs) = last_command {
        views.push(ResultView::new(rs, salt * 1000));
    }
    if !views.is_empty() || error.is_none() {
        c.results = views;
    }
    c.result_tab = if error.is_some() || c.results.is_empty() {
        ResultTab::Output
    } else {
        ResultTab::Result(c.results.len() - 1)
    };
}

impl eframe::App for DataCraftApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.perf.begin_frame();
        // First of all: the menu's edit commands are injected as events of this frame.
        self.menu_commands(ctx);
        self.handle_responses();
        if self.applied_zoom != Some(self.zoom) {
            ctx.set_zoom_factor(self.zoom);
            self.applied_zoom = Some(self.zoom);
        }
        if self.applied_dark != Some(self.dark) || self.perf.style_outdated() {
            theme::apply(ctx, self.dark);
            self.applied_dark = Some(self.dark);
            self.perf.apply_style(ctx);
        }
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        self.perf.instant_wheel(ctx, raw_input);
    }

    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if let Some(size) = self.demo_window.take() {
            ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(vec2(400.0, 300.0)));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        }
        self.shortcuts(&ctx);
        let p = palette(self.dark);
        let bar = egui::Frame::NONE.fill(p.main_bg);

        // With zoom < 100% the bar doesn't go below 32 screen pt (macOS traffic lights).
        let title_h = 40.0_f32.max(32.0 / ctx.zoom_factor().max(0.1));
        egui::Panel::top("title_bar")
            .exact_size(title_h)
            .resizable(false)
            .show_separator_line(false)
            .frame(bar)
            .show(ui, |ui| self.title_bar(ui));
        egui::Panel::bottom("status_bar")
            .exact_size(30.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(bar)
            .show(ui, |ui| self.status_bar(ui));
        egui::Panel::left("stripe_left")
            .exact_size(40.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(bar)
            .show(ui, |ui| self.left_stripe(ui));
        if self.object_browser.active || self.consoles.get(self.active).is_none_or(|c| c.table.is_none()) {
        egui::Panel::right("stripe_right")
            .exact_size(40.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(bar)
            .show(ui, |ui| self.right_stripe(ui));
        }
        if self.show_explorer {
            let mut panel = egui::Panel::left("explorer")
                .resizable(true)
                .default_size(300.0)
                .size_range(150.0..=700.0)
                .show_separator_line(false)
                .frame(bar.inner_margin(Margin { right: theme::ISLAND_GAP, ..Margin::ZERO }));
            if let Some(w) = self.demo_explorer_w {
                panel = panel.exact_size(w);
            }
            let explorer_actions = panel
                .show(ui, |ui| theme::fixed_island(ui, &p, |ui| explorer::show(ui, &mut self.explorer, &self.conns)))
                .inner;
            self.handle_explorer(&ctx, explorer_actions);
        }
        let left = if self.show_explorer { theme::ISLAND_GAP } else { 0 };
        egui::CentralPanel::default()
            .frame(bar.inner_margin(Margin { left, ..Margin::ZERO }))
            .show(ui, |ui| self.central(ui));
        self.dialogs(&ctx);
        if cfg!(target_os = "windows") { crate::ui::window_chrome::resize_edges(&ctx); }
        self.perf.stats_overlay(&ctx, frame.info().cpu_usage);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let state = Persisted {
            backups: self.explorer.backups.clone(),
            object_organizations: self.object_browser.organizations.iter().filter(|o| !o.is_empty()).cloned().collect(),
            object_layout: self.object_browser.layout,
            save_on_row_change: self.save_on_row_change,
            islands_dark: self.dark,
            font_size: self.font_size,
            max_rows: self.max_rows,
            auto_complete: self.auto_complete,
            consoles: self
                .consoles
                .iter()
                .map(|c| PersistedConsole {
                    routine_comment: c.routine.as_ref().map(|r| r.comment.clone()),
                    routine: c.routine.as_ref().map(|r| (r.reference.clone(), r.saved.clone(), r.transaction)),
                    title: c.title.clone(),
                    text: c.text.clone(),
                    conn: c.conn,
                    table: c.table.as_ref().map(|t| (t.schema.clone(), t.table.clone())),
                    table_view: c.table.as_ref().map(|t| t.view).unwrap_or_default(),
                    db: c.db.clone(),
                })
                .collect(),
            active: self.active,
            console_counter: self.console_counter,
            show_explorer: self.show_explorer,
            show_results: self.show_results,
            zoom: self.zoom,
            lang: self.lang,
            render_mode: self.perf.mode,
            show_frame_stats: self.perf.show_stats,
            check_updates: self.check_updates,
            skipped_version: self.skipped_version.clone(),
        };
        eframe::set_value(storage, STORAGE_KEY, &state);
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn tabs_app() -> DataCraftApp {
        DataCraftApp {
            worker: Worker::start(egui::Context::default()), conns: Vec::new(),
            consoles: (1..=4).map(|id| Console::new(id, format!("Query {id}"), String::new(), None)).collect(),
            active: 1, next_console_id: 5, console_counter: 4, result_salt: 0,
            explorer: ExplorerState::default(), backup: None, scheduler: None, object_browser: crate::ui::object_browser::ObjectBrowser::default(),
            save_on_row_change: false,
            dialog: None, confirm_delete: None, confirm_analyze: None, ask_password: None, create: None, drop: None, views: Default::default(), routines: Default::default(), table_actions: Default::default(), import: None, copy: None, mask_export: None, user_dialog: None, renaming: None,
            show_about: false, show_shortcuts: false, show_settings: false, script: None, check_updates: false, skipped_version: None, update_check: None, update: None, native_menu: None, show_explorer: true,
            show_results: true, dark: true, applied_dark: None, font_size: DEFAULT_FONT_SIZE,
            max_rows: 1000, auto_complete: true, status_msg: None, last_conn_id: 0, demo_pending: false, demo_edit: None,
            zoom: 1.0, applied_zoom: None, preview: None, confirm_discard: None,
            demo_explorer_w: None, demo_window: None, lang: None,
            perf: crate::ui::performance::Performance::new(None, Default::default(), false),
            modify: None,
        }
    }

    #[test]
    fn startup_opens_objects_without_creating_or_losing_console_tabs() {
        use egui_kittest::kittest::Queryable;
        let mut app = tabs_app();
        app.consoles.clear();
        app.restore_tabs(Vec::new(), 9);
        assert!(app.consoles.is_empty());
        assert_eq!(app.active, 0);
        assert!(app.object_browser.active);
        let mut first = true;
        let mut h = egui_kittest::Harness::builder().build_ui_state(move |ui, app: &mut DataCraftApp| {
            if std::mem::take(&mut first) { theme::install_fonts(ui.ctx()); return; }
            app.central(ui);
        }, app);
        h.run_steps(3);
        assert!(h.query_by_label("Objects").is_some());
        assert!(h.query_by_label("Select a database in the explorer to browse its objects.").is_some());
        let app = h.state_mut();
        app.restore_tabs(vec![PersistedConsole {
            routine_comment: None,
            routine: None,
            title: "Saved query".into(), text: "SELECT 42;".into(), conn: None,
            table: None, table_view: Default::default(), db: None,
        }], 7);
        assert!(app.object_browser.active);
        assert_eq!(app.consoles.len(), 1);
        assert_eq!(app.consoles[0].text, "SELECT 42;");
        h.run_steps(2);
        h.get_by_label("Saved query").click();
        h.run_steps(2);
        assert!(!h.state().object_browser.active);
        assert_eq!(h.state().consoles[0].text, "SELECT 42;");
    }

    #[test]
    fn pages_keep_the_projection_and_filters_do_not_apply_before_discard_confirmation() {
        let sql = table_query_page(Driver::Sqlite, "main", "notes", "id > 0", "id DESC", 10, 20);
        assert_eq!(sql, "SELECT * FROM \"notes\" WHERE id > 0 ORDER BY id DESC LIMIT 10 OFFSET 20");
        let sql = table_query_page(Driver::MsSql, "dbo", "notes", "", "", 10, 20);
        assert_eq!(sql, "SELECT * FROM [dbo].[notes] ORDER BY (SELECT NULL) OFFSET 20 ROWS FETCH NEXT 10 ROWS ONLY");
        let mut app = tabs_app();
        app.open_table(&TableRef { conn: 7, db: "shop".into(), schema: "public".into(), table: "notes".into() });
        let index = app.active;
        let id = app.consoles[index].id;
        let mut result = ResultView::new(crate::db::ResultSet { columns: vec!["id".into()], rows: vec![vec![Some("1".into())]], ..Default::default() }, 991);
        result.set_edit_target(Ok(crate::ui::editing::EditTarget { schema: "public".into(), table: "notes".into(), col_names: vec!["id".into()], key_cols: vec![0], binary_cols: vec![false], has_pk: true, insert_only: false }));
        result.edit.as_mut().unwrap().state.set_value(&result.rs, crate::ui::editing::RowRef::Existing(0), 0, Some("2".into()));
        app.consoles[index].results.push(result);
        app.consoles[index].table.as_mut().unwrap().filter_sort.pending = Some(("id > 10".into(), "id DESC".into()));
        assert!(!app.guard(Discard::ApplyFilter(id)));
        assert!(app.consoles[index].table.as_ref().unwrap().where_clause.is_empty());
        assert!(app.consoles[index].has_pending_edits());
        app.discard_and_do(Discard::ApplyFilter(id));
        assert_eq!(app.consoles[index].table.as_ref().unwrap().where_clause, "id > 10");
    }

    #[test]
    fn view_and_auto_save_preferences_roundtrip_and_apply_to_new_tables() {
        use crate::ui::object_browser::ObjectLayout;
        let defaults: Persisted = serde_json::from_str("{}").unwrap();
        assert!(!defaults.save_on_row_change);
        assert_eq!(defaults.object_layout, ObjectLayout::List);
        #[derive(Default)]
        struct MemoryStorage(std::collections::BTreeMap<String, String>);
        impl eframe::Storage for MemoryStorage {
            fn get_string(&self, key: &str) -> Option<String> { self.0.get(key).cloned() }
            fn set_string(&mut self, key: &str, value: String) { self.0.insert(key.into(), value); }
            fn remove_string(&mut self, key: &str) { self.0.remove(key); }
            fn flush(&mut self) {}
        }
        let mut app = tabs_app();
        app.save_on_row_change = true;
        app.object_browser.layout = ObjectLayout::Grid;
        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);
        let restored: Persisted = eframe::get_value(&storage, STORAGE_KEY).unwrap();
        assert!(restored.save_on_row_change);
        assert_eq!(restored.object_layout, ObjectLayout::Grid);
        let mut app = tabs_app();
        app.save_on_row_change = restored.save_on_row_change;
        app.object_browser.layout = restored.object_layout;
        let table = |name: &str| TableRef { conn: 7, db: "shop".into(), schema: "public".into(), table: name.into() };
        app.open_table(&table("customers"));
        app.open_table(&table("orders"));
        assert!(app.consoles[app.active].table.as_ref().unwrap().save_on_row_change);
        app.consoles[app.active].table.as_mut().unwrap().save_on_row_change = false;
        app.sync_save_on_row_change();
        assert!(!app.save_on_row_change);
        assert!(app.consoles.iter().filter_map(|c| c.table.as_ref()).all(|t| !t.save_on_row_change));
        app.open_table(&table("invoices"));
        assert!(!app.consoles[app.active].table.as_ref().unwrap().save_on_row_change);
    }

    #[test]
    fn bulk_tab_closing_preserves_pending_edits_until_confirmed_and_keeps_active_tab() {
        use crate::db::ResultSet;
        use crate::ui::editing::{EditTarget, RowRef};
        let mut app = tabs_app();
        let mut view = ResultView::new(ResultSet { columns: vec!["note".into()], rows: vec![vec![Some("before".into())]], ..Default::default() }, 1);
        view.set_edit_target(Ok(EditTarget { schema: "main".into(), table: "notes".into(), col_names: vec!["note".into()], key_cols: vec![0], binary_cols: vec![false], has_pk: false, insert_only: false }));
        view.edit.as_mut().unwrap().state.set_value(&view.rs, RowRef::Existing(0), 0, Some("after".into()));
        app.consoles[3].results.push(view);
        let action = Discard::CloseTabs(2, CloseResults::Right(1));
        assert!(!app.guard(action));
        assert_eq!(app.consoles.len(), 4);
        assert_eq!(app.confirm_discard, Some(action));
        app.confirm_discard = None;
        assert!(app.consoles[3].has_pending_edits());
        app.discard_and_do(action);
        assert_eq!(app.consoles.iter().map(|c| c.id).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(app.consoles[app.active].id, 2);
        app.close_tabs_now(2, CloseResults::Left(99));
        assert_eq!(app.consoles[0].id, 2);
        assert_eq!(app.active, 0);
    }

    #[test]
    #[ignore = "Requires a GPU; renders the real editor tab context menu"]
    fn render_tab_menu() {
        use egui_kittest::kittest::Queryable;
        let mut first = true;
        let mut h = egui_kittest::Harness::builder().with_size(vec2(820.0, 300.0)).build_ui_state(
            move |ui, app: &mut DataCraftApp| {
                if std::mem::take(&mut first) { theme::install_fonts(ui.ctx()); theme::apply(ui.ctx(), true); return; }
                app.editor_tabs(ui);
            }, tabs_app());
        h.run_steps(3);
        h.get_by_label("Query 2").click_secondary();
        h.run_steps(3);
        assert!(h.query_by_label("Close to the right").is_some());
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/navigation-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        h.render().unwrap().save(dir.join("tab-menu.png")).unwrap();
    }

    #[test]
    #[ignore = "Requires a GPU; renders the notice of a newer release"]
    fn render_update_notice() {
        use egui_kittest::kittest::Queryable;
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/branding-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = tabs_app();
        app.update = Some(crate::update::Release { version: "9.0.0".into(), url: "https://github.com/javy-fernandez/datacraft/releases/tag/v9.0.0".into() });
        let mut first = true;
        let mut h = egui_kittest::Harness::builder().with_size(vec2(900.0, 620.0)).build_ui_state(move |ui, app: &mut DataCraftApp| {
            if std::mem::take(&mut first) { theme::install_fonts(ui.ctx()); theme::apply(ui.ctx(), true); return; }
            egui::CentralPanel::default().show(ui, |ui| { app.editor_tabs(ui); });
            app.dialogs(ui.ctx());
        }, app);
        h.run_steps(4);
        h.render().unwrap().save(dir.join("update-notice.png")).unwrap();
        // "Skip This Version" remembers it and hides the notice.
        h.get_by_label(tr("Skip This Version")).click();
        h.run_steps(2);
        assert!(h.state().update.is_none());
        assert_eq!(h.state().skipped_version.as_deref(), Some("9.0.0"));
    }

    #[test]
    #[ignore = "Requires a GPU; renders the project widget and About with the same application mark"]
    fn render_application_branding() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/branding-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        for dark in [false, true] {
            let mut app = tabs_app(); app.dark = dark; app.show_about = true;
            let mut first = true;
            let mut h = egui_kittest::Harness::builder().with_size(vec2(900.0, 620.0)).build_ui_state(move |ui, app: &mut DataCraftApp| {
                if std::mem::take(&mut first) { theme::install_fonts(ui.ctx()); theme::apply(ui.ctx(), dark); return; }
                egui::Panel::top("branding_title").exact_size(40.0).show(ui, |ui| app.title_bar(ui));
                egui::CentralPanel::default().show(ui, |ui| { app.editor_tabs(ui); });
                app.dialogs(ui.ctx());
            }, app);
            h.run_steps(4);
            h.render().unwrap().save(dir.join(if dark { "application-brand-dark.png" } else { "application-brand-light.png" })).unwrap();
        }
    }

    #[test]
    #[ignore = "Requires a GPU; renders Objects with the real title, tabs, explorer and status bar"]
    fn render_objects_with_application_chrome() {
        use egui_kittest::kittest::Queryable;
        let mut app = tabs_app();
        let mut cfg = ConnectionConfig::new(7, Driver::MariaDb); cfg.name = "Development".into();
        let mut connection = ConnState::new(cfg); connection.status = ConnStatus::Connected;
        let columns = vec![ColumnInfo { name: "id".into(), data_type: "int".into(), nullable: false, primary_key: true }];
        let tables = ["customers", "orders", "products", "invoices", "payments", "shipments"].into_iter().map(|name| TableInfo { name: name.into(), kind: crate::db::ObjectKind::Table, columns: columns.clone() }).collect();
        connection.databases.push(DatabaseInfo { name: "shop".into(), current: true, schemas: Some(vec![SchemaInfo { name: "shop".into(), tables }]) });
        app.conns.push(connection);
        app.consoles[app.active].conn = Some(7); app.consoles[app.active].db = Some("shop".into());
        app.explorer.expanded.extend(["c:7".into(), "d:7:shop".into()]);
        app.object_browser.open(crate::ui::object_browser::ObjectScope { conn: 7, db: "shop".into(), schema: None, kind: None });
        let mut first = true;
        let mut h = egui_kittest::Harness::builder().with_size(vec2(1600.0, 1000.0)).build_ui_state(move |ui, app: &mut DataCraftApp| {
            if std::mem::take(&mut first) { theme::install_fonts(ui.ctx()); theme::apply(ui.ctx(), true); return; }
            egui::Panel::top("fixture_title").exact_size(40.0).show(ui, |ui| app.title_bar(ui));
            egui::Panel::bottom("fixture_status").exact_size(28.0).show(ui, |ui| app.status_bar(ui));
            egui::Panel::left("fixture_explorer").exact_size(250.0).show(ui, |ui| { explorer::show(ui, &mut app.explorer, &app.conns); });
            egui::CentralPanel::default().show(ui, |ui| app.central(ui));
        }, app);
        h.run_steps(4);
        let tabs_bottom = h.get_all_by_label("Objects").map(|node| node.rect().bottom()).min_by(f32::total_cmp).unwrap();
        assert!(h.get_by_label("Information").rect().top() < tabs_bottom);
        assert!(h.get_by_label("Search objects…").rect().top() > 940.0);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/branding-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        for (layout, name) in [(crate::ui::object_browser::ObjectLayout::Grid, "objects-grid"), (crate::ui::object_browser::ObjectLayout::List, "objects-list")] {
            h.state_mut().object_browser.layout = layout; h.run_steps(3);
            if h.state().object_browser.selected.is_none() { h.get_by_label("orders").click(); h.run_steps(2); }
            let app = h.state_mut();
            let table = app.object_browser.selected.clone().unwrap();
            let request = app.object_browser.start_request(table);
            app.object_browser.receive(7, request, Ok(crate::db::object_info::ObjectInfo { rows: Some(42), engine: Some("InnoDB".into()), row_format: Some("Dynamic".into()), auto_increment: Some(43), created: Some("2026-10-01 09:30:00".into()), data_bytes: Some(16384), ..Default::default() }));
            h.run_steps(3);
            h.render().unwrap().save(dir.join(format!("{name}.png"))).unwrap();
        }
    }

    #[test]
    fn zoom_steps() {
        assert_eq!(zoom_in(1.0), 1.1);
        assert_eq!(zoom_out(1.0), 0.9);
        assert_eq!(zoom_in(2.0), 2.0);
        assert_eq!(zoom_out(0.5), 0.5);
        // From an intermediate value it goes to the nearest step in that direction.
        assert_eq!(zoom_in(1.2), 1.25);
        assert_eq!(zoom_out(1.2), 1.1);
        assert_eq!(clamp_zoom(5.0), 2.0);
        assert_eq!(clamp_zoom(f32::NAN), 1.0);
        assert_eq!(zoom_label(1.25), "125%");
        let mut z = 0.5;
        for _ in 0..ZOOM_STEPS.len() {
            z = zoom_in(z);
        }
        assert_eq!(z, 2.0);
    }

    #[test]
    fn read_only_statements() {
        assert!(is_query("select 1"));
        assert!(is_query("  -- note\n SELECT * FROM t"));
        assert!(is_query("with x as (select 1) select * from x"));
        // Snowflake anonymous procedures run anything (found in review of the Snowflake PR).
        assert!(!is_query("WITH p AS PROCEDURE() RETURNS INT LANGUAGE SQL AS $$ BEGIN DROP TABLE t; RETURN 1; END $$ CALL p()"));
        assert!(!is_query("with p as procedure() returns int language sql as 'begin return 1; end' call p()"));
        assert!(!is_query("with x as (select 1) select * from x; drop table t"));
        assert!(!is_query("select $$ text $$"));
        assert!(is_query("SHOW CREATE TABLE t"));
        assert!(is_query("describe procedure p(int)"));
        assert!(is_query("explain select 1"));
        assert!(!is_query("explain analyze delete from t"));
        assert!(!is_query("explain analyse delete from t"));
        assert!(!is_query("select * into copy from t"));
        assert!(is_query("SET SHOWPLAN_TEXT ON"));
        assert!(!is_query("set ansi_nulls off"));
        assert!(!is_query("with d as (delete from t returning *) select * from d"));
        assert!(!is_query("update t set a = 1"));
        assert!(!is_query("drop table t"));
        assert!(!is_query(""));
    }

    #[test]
    fn table_query_limits_rows_per_dialect() {
        assert_eq!(table_query(Driver::Sqlite, "main", "users", "", "", 50), "SELECT * FROM \"users\" LIMIT 50");
        assert_eq!(
            table_query(Driver::Postgres, "public", "t", "id > 3;", " name desc ", 10),
            "SELECT * FROM \"public\".\"t\" WHERE id > 3 ORDER BY name desc LIMIT 10"
        );
        assert_eq!(
            table_query(Driver::MsSql, "dbo", "t", "x = 1", "", 5),
            "SELECT TOP (5) * FROM [dbo].[t] WHERE x = 1"
        );
        assert_eq!(
            table_query(Driver::Oracle, "HR", "T", "", "ID", 5),
            "SELECT * FROM (SELECT * FROM \"HR\".\"T\" ORDER BY ID) WHERE ROWNUM <= 5"
        );
        assert_eq!(table_query(Driver::MySql, "shop", "t", "", "", 5), "SELECT * FROM `shop`.`t` LIMIT 5");
    }

    #[test]
    fn nosql_changes_preview_as_their_commands() {
        use crate::edit::Change;
        let kv = |c: &str, v: Option<&str>| (c.to_string(), v.map(str::to_string));
        let changes = vec![
            Change::Update { key: vec![kv("_id", Some("1"))], set: vec![kv("name", Some("O'Brien \"x\""))] },
            Change::Insert { values: vec![kv("name", Some("Ana")), kv("tags", Some("[1]"))] },
            Change::Delete { key: vec![kv("_id", Some("2"))] },
        ];
        let types: HashMap<String, String> = [("_id", "int"), ("tags", "array")].map(|(a, b)| (a.to_string(), b.to_string())).into();
        let mongo = edit::change_statements(Driver::MongoDb, "shop", "users", &changes, &types).unwrap();
        assert_eq!(mongo.len(), 3);
        assert!(mongo[0].starts_with("db.getCollection(\"users\").updateOne({\"_id\": 1}"));
        let text = preview_text(Driver::MongoDb, &mongo);
        assert!(text.ends_with(';'));
        let job = sql::highlight_for(&text, Driver::MongoDb, true, FontId::monospace(12.5));
        assert_eq!(job.text, text);

        let changes = vec![
            Change::Update { key: vec![kv("key", Some("s"))], set: vec![kv("ttl", None), kv("value", Some("it's \"v\""))] },
            Change::Delete { key: vec![kv("key", Some("h"))] },
        ];
        let types: HashMap<String, String> = [("s", "string"), ("h", "hash")].map(|(a, b)| (a.to_string(), b.to_string())).into();
        let redis = edit::change_statements(Driver::Redis, "db0", "*", &changes, &types).unwrap();
        assert_eq!(redis, vec!["PERSIST s", r#"SET s "it's \"v\"" XX KEEPTTL"#, "DEL h"]);
        let text = preview_text(Driver::Redis, &redis);
        assert_eq!(text.lines().count(), 3);
        assert!(!text.contains(';'));
        let job = sql::highlight_for(&text, Driver::Redis, false, FontId::monospace(12.5));
        assert_eq!(job.text, text);
        assert_eq!(preview_text(Driver::Sqlite, &["DELETE FROM t".to_string()]), "DELETE FROM t;");
    }
}
