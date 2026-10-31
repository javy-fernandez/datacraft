//! Native macOS menu bar menu (`muda` crate).
//!
//! **Shortcut strategy:** the native menu is what triggers the actions. A menu accelerator
//! consumes the key press in `-[NSApplication sendEvent:]` before it reaches the winit view,
//! so egui never sees that key (no double execution) and egui's duplicate shortcuts are
//! dead: they must be removed from `DataCraftApp::shortcuts`.
//!
//! **Edit:** the winit view doesn't implement `copy:`/`paste:`/`undo:`…, and muda disables
//! menu autoenabling, so the predefined edit `PredefinedMenuItem`s would swallow ⌘C/⌘V with
//! nobody handling them and egui would stop copying/pasting. That's why Edit uses its own
//! items and [`NativeMenu::poll`] forwards the action to egui just as egui-winit would
//! generate it: `Event::Copy` / `Event::Cut`, `ViewportCommand::RequestPaste` (egui-winit
//! reads the clipboard and emits `Event::Paste` on the next frame) and `Event::Key` with ⌘
//! for undo, redo and select all. For those events to arrive in the same frame, call `poll`
//! from `App::logic` (before painting the UI).
//!
//! **Quit:** own item (not `terminate:`) so the app closes with `ViewportCommand::Close`
//! and can save state / warn about pending changes.
//!
//! **Language:** every label goes through [`tr`]; after changing the language call
//! [`NativeMenu::relabel`].

use crate::i18n::{tr, tr_ctx};
use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use std::cell::{Cell, RefCell};
use std::sync::mpsc::{Receiver, channel};

/// Action chosen in the native menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuCommand {
    // DataCraft
    About,
    CheckUpdates,
    Settings,
    Quit,
    // File
    NewConnection,
    NewConsole,
    OpenSql,
    SaveSqlAs,
    CloseTab,
    // Edit (handled by `poll`; only `Find` reaches the app)
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
    Find,
    // View
    ToggleExplorer,
    ToggleResults,
    ThemeLight,
    ThemeDark,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    FontBigger,
    FontSmaller,
    FontReset,
    // Run
    Run,
    RunAll,
    SaveChanges,
    Reload,
    // Help
    Help,
    Shortcuts,
}

impl MenuCommand {
    const ALL: [MenuCommand; 32] = [
        Self::About,
        Self::CheckUpdates,
        Self::Settings,
        Self::Quit,
        Self::NewConnection,
        Self::NewConsole,
        Self::OpenSql,
        Self::SaveSqlAs,
        Self::CloseTab,
        Self::Undo,
        Self::Redo,
        Self::Cut,
        Self::Copy,
        Self::Paste,
        Self::SelectAll,
        Self::Find,
        Self::ToggleExplorer,
        Self::ToggleResults,
        Self::ThemeLight,
        Self::ThemeDark,
        Self::ZoomIn,
        Self::ZoomOut,
        Self::ZoomReset,
        Self::FontBigger,
        Self::FontSmaller,
        Self::FontReset,
        Self::Run,
        Self::RunAll,
        Self::SaveChanges,
        Self::Reload,
        Self::Help,
        Self::Shortcuts,
    ];

    /// muda item id (the variant's `Debug`, unique by construction).
    fn id(self) -> String {
        format!("datacraft.{self:?}")
    }

    fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.id() == id)
    }

    /// Edit commands that `poll` forwards to egui instead of returning them.
    fn is_text_edit(self) -> bool {
        matches!(self, Self::Undo | Self::Redo | Self::Cut | Self::Copy | Self::Paste | Self::SelectAll)
    }
}

/// Native menu installed in NSApp. Keep it alive for the lifetime of the app.
pub struct NativeMenu {
    _menu: Menu,
    ctx: egui::Context,
    rx: Receiver<MenuCommand>,
    theme_light: CheckMenuItem,
    theme_dark: CheckMenuItem,
    explorer: CheckMenuItem,
    results: CheckMenuItem,
    /// Every translatable entry (menus, submenus and items) with the function that
    /// returns its text in the active language; used by [`NativeMenu::relabel`].
    labels: Vec<Labeled>,
    /// Last synced state (dark, explorer, results) so AppKit isn't touched every frame.
    synced: Cell<Option<(bool, bool, bool)>>,
}

/// Menu entry whose text can be changed after creation.
enum Entry {
    Item(MenuItem),
    Check(CheckMenuItem),
    Submenu(Submenu),
    Predefined(PredefinedMenuItem),
}

/// An entry and its label (a `tr("…")` call, re-evaluated on each relabel).
struct Labeled {
    entry: Entry,
    text: fn() -> &'static str,
}

impl Labeled {
    fn apply(&self) {
        let text = (self.text)();
        match &self.entry {
            Entry::Item(i) => i.set_text(text),
            Entry::Check(c) => c.set_text(text),
            Entry::Submenu(s) => s.set_text(text),
            Entry::Predefined(p) => p.set_text(text),
        }
    }
}

impl NativeMenu {
    /// Creates the menu and installs it in NSApp. Call it once, from the main thread and with
    /// the app already launched (e.g. in `DataCraftApp::new` with `&cc.egui_ctx`): it replaces
    /// winit's default menu. Returns `None` when not on the main thread or if it fails.
    pub fn install(ctx: &egui::Context) -> Option<Self> {
        // Only macOS has the NSApp menu bar; elsewhere the egui shortcuts act instead.
        if !cfg!(target_os = "macos") || std::thread::current().name() != Some("main") {
            return None;
        }
        let built = match build() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("native menu: {e}");
                return None;
            }
        };
        #[cfg(target_os = "macos")]
        built.menu.init_for_nsapp();

        let (tx, rx) = channel();
        let repaint = ctx.clone();
        // Can only be set once per process (OnceLock in muda).
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            if let Some(cmd) = MenuCommand::from_id(e.id().as_ref()) {
                let _ = tx.send(cmd);
                repaint.request_repaint();
            }
        }));

        Some(Self {
            _menu: built.menu,
            ctx: ctx.clone(),
            rx,
            theme_light: built.theme_light,
            theme_dark: built.theme_dark,
            explorer: built.explorer,
            results: built.results,
            labels: built.labels,
            synced: Cell::new(None),
        })
    }

    /// Commands clicked since the last frame (non-blocking). Edit commands (undo, redo,
    /// cut, copy, paste, select all) are forwarded to egui here and not returned.
    pub fn poll(&self) -> Vec<MenuCommand> {
        let mut out = Vec::new();
        for cmd in self.rx.try_iter() {
            // A click on a CheckMenuItem toggles its check: resync on the next `sync`.
            self.synced.set(None);
            if cmd.is_text_edit() {
                forward_edit(&self.ctx, cmd);
            } else {
                out.push(cmd);
            }
        }
        out
    }

    /// Syncs the checks with the app state (cheap: only touches AppKit when it changes).
    pub fn sync(&self, dark: bool, explorer_visible: bool, results_visible: bool) {
        let state = (dark, explorer_visible, results_visible);
        if self.synced.get() == Some(state) {
            return;
        }
        self.theme_light.set_checked(!dark);
        self.theme_dark.set_checked(dark);
        self.explorer.set_checked(explorer_visible);
        self.results.set_checked(results_visible);
        self.synced.set(Some(state));
    }

    /// Sets the text of every menu, submenu and item again in the active language.
    ///
    /// Call it from the main thread right after `crate::i18n::set_lang` so the menu bar
    /// follows the language chosen in the app (the labels are only translated when built).
    /// Accelerators, checks and ids don't change.
    pub fn relabel(&self) {
        for l in &self.labels {
            l.apply();
        }
    }
}

/// Forwards an edit action to egui as if it came from the keyboard.
fn forward_edit(ctx: &egui::Context, cmd: MenuCommand) {
    use egui::{Event, Key, Modifiers as M};
    let cmd_mods = M::MAC_CMD | M::COMMAND;
    let key = |key: Key, modifiers: M| Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers };
    let event = match cmd {
        MenuCommand::Copy => Event::Copy,
        MenuCommand::Cut => Event::Cut,
        MenuCommand::Paste => {
            // egui-winit reads the clipboard and emits `Event::Paste` on the next frame.
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaste);
            ctx.request_repaint();
            return;
        }
        MenuCommand::Undo => key(Key::Z, cmd_mods),
        MenuCommand::Redo => key(Key::Z, cmd_mods | M::SHIFT),
        MenuCommand::SelectAll => key(Key::A, cmd_mods),
        _ => return,
    };
    ctx.input_mut(|i| i.events.push(event));
}

struct Built {
    menu: Menu,
    theme_light: CheckMenuItem,
    theme_dark: CheckMenuItem,
    explorer: CheckMenuItem,
    results: CheckMenuItem,
    labels: Vec<Labeled>,
}

type Label = fn() -> &'static str;

fn build() -> muda::Result<Built> {
    let labels = RefCell::new(Vec::new());
    let keep = |entry: Entry, text: Label| labels.borrow_mut().push(Labeled { entry, text });

    let cmd = Modifiers::META;
    let acc = |mods: Modifiers, code: Code| Some(Accelerator::new(mods, code));
    let item = |c: MenuCommand, text: Label, a: Option<Accelerator>| {
        let i = MenuItem::with_id(c.id(), text(), true, a);
        keep(Entry::Item(i.clone()), text);
        i
    };
    let check = |c: MenuCommand, text: &str| CheckMenuItem::with_id(c.id(), text, true, false, None);
    let predefined = |p: PredefinedMenuItem, text: Label| {
        keep(Entry::Predefined(p.clone()), text);
        p
    };
    let submenu = |text: Label, items: &[&dyn muda::IsMenuItem]| -> muda::Result<Submenu> {
        let s = Submenu::with_items(text(), true, items)?;
        keep(Entry::Submenu(s.clone()), text);
        Ok(s)
    };
    let sep = PredefinedMenuItem::separator;

    let app_menu = Submenu::with_items(
        "DataCraft",
        true,
        &[
            &item(MenuCommand::About, || tr("About DataCraft"), None),
            &item(MenuCommand::CheckUpdates, || tr("Check for Updates…"), None),
            &sep(),
            &item(MenuCommand::Settings, || tr("Settings…"), acc(cmd, Code::Comma)),
            &sep(),
            &predefined(PredefinedMenuItem::services(Some(tr("Services"))), || tr("Services")),
            &sep(),
            &predefined(PredefinedMenuItem::hide(Some(tr("Hide DataCraft"))), || tr("Hide DataCraft")),
            &predefined(PredefinedMenuItem::hide_others(Some(tr("Hide Others"))), || tr("Hide Others")),
            &predefined(PredefinedMenuItem::show_all(Some(tr("Show All"))), || tr("Show All")),
            &sep(),
            &item(MenuCommand::Quit, || tr("Quit DataCraft"), acc(cmd, Code::KeyQ)),
        ],
    )?;

    let file_menu = submenu(
        || tr("File"),
        &[
            &item(MenuCommand::NewConnection, || tr("New connection…"), None),
            &item(MenuCommand::NewConsole, || tr("New console"), acc(cmd | Modifiers::SHIFT, Code::KeyN)),
            &item(MenuCommand::OpenSql, || tr("Open SQL script…"), acc(cmd, Code::KeyO)),
            &item(MenuCommand::SaveSqlAs, || tr("Save console as…"), acc(cmd | Modifiers::SHIFT, Code::KeyS)),
            &sep(),
            &item(MenuCommand::CloseTab, || tr("Close tab"), acc(cmd, Code::KeyW)),
        ],
    )?;

    let edit_menu = submenu(
        || tr_ctx("menu", "Edit"),
        &[
            &item(MenuCommand::Undo, || tr("Undo"), acc(cmd, Code::KeyZ)),
            &item(MenuCommand::Redo, || tr("Redo"), acc(cmd | Modifiers::SHIFT, Code::KeyZ)),
            &sep(),
            &item(MenuCommand::Cut, || tr("Cut"), acc(cmd, Code::KeyX)),
            &item(MenuCommand::Copy, || tr("Copy"), acc(cmd, Code::KeyC)),
            &item(MenuCommand::Paste, || tr("Paste"), acc(cmd, Code::KeyV)),
            &item(MenuCommand::SelectAll, || tr("Select all"), acc(cmd, Code::KeyA)),
            &sep(),
            &item(MenuCommand::Find, || tr("Find…"), acc(cmd, Code::KeyF)),
        ],
    )?;

    // Product names: the same in every language.
    let theme_light = check(MenuCommand::ThemeLight, "Islands Light");
    let theme_dark = check(MenuCommand::ThemeDark, "Islands Dark");
    let explorer = check(MenuCommand::ToggleExplorer, tr("Database Explorer"));
    let results = check(MenuCommand::ToggleResults, tr("Results panel"));
    keep(Entry::Check(results.clone()), || tr("Results panel"));
    let theme_menu = submenu(|| tr("Theme"), &[&theme_light, &theme_dark])?;
    let zoom_menu = submenu(
        || tr("IDE zoom"),
        &[
            &item(MenuCommand::ZoomIn, || tr("Zoom In"), acc(cmd, Code::Equal)),
            &item(MenuCommand::ZoomOut, || tr("Zoom Out"), acc(cmd, Code::Minus)),
            &item(MenuCommand::ZoomReset, || tr("Reset"), acc(cmd, Code::Digit0)),
        ],
    )?;
    let font_menu = submenu(
        || tr("Editor font"),
        &[
            &item(MenuCommand::FontBigger, || tr("Increase"), acc(cmd | Modifiers::ALT, Code::Equal)),
            &item(MenuCommand::FontSmaller, || tr("Decrease"), acc(cmd | Modifiers::ALT, Code::Minus)),
            &item(MenuCommand::FontReset, || tr("Reset"), None),
        ],
    )?;
    let view_menu = submenu(|| tr("View"), &[&explorer, &results, &sep(), &theme_menu, &zoom_menu, &font_menu])?;

    let run_menu = submenu(
        || tr("Run"),
        &[
            &item(MenuCommand::Run, || tr("Run statement"), acc(cmd, Code::Enter)),
            &item(MenuCommand::RunAll, || tr("Run all"), acc(cmd | Modifiers::SHIFT, Code::Enter)),
            &sep(),
            &item(MenuCommand::SaveChanges, || tr("Save grid changes"), acc(cmd, Code::KeyS)),
            &item(MenuCommand::Reload, || tr("Reload data"), acc(cmd, Code::KeyR)),
        ],
    )?;

    let window_menu = submenu(
        || tr("Window"),
        &[
            &predefined(PredefinedMenuItem::minimize(Some(tr("Minimize"))), || tr("Minimize")),
            &PredefinedMenuItem::maximize(Some(tr("Zoom"))),
            &sep(),
            &predefined(PredefinedMenuItem::bring_all_to_front(Some(tr("Bring All to Front"))), || {
                tr("Bring All to Front")
            }),
        ],
    )?;
    #[cfg(target_os = "macos")]
    window_menu.set_as_windows_menu_for_nsapp();

    let help_menu = submenu(|| tr("Help"), &[&item(MenuCommand::Shortcuts, || tr("Keyboard shortcuts…"), None), &item(MenuCommand::Help, || tr("DataCraft Help"), None)])?;
    #[cfg(target_os = "macos")]
    help_menu.set_as_help_menu_for_nsapp();

    let menu = Menu::with_items(&[&app_menu, &file_menu, &edit_menu, &view_menu, &run_menu, &window_menu, &help_menu])?;
    Ok(Built { menu, theme_light, theme_dark, explorer, results, labels: labels.into_inner() })
}

#[cfg(test)]
mod tests {
    use super::MenuCommand;

    #[test]
    fn ids_round_trip() {
        for c in MenuCommand::ALL {
            assert_eq!(MenuCommand::from_id(&c.id()), Some(c));
        }
        assert_eq!(MenuCommand::from_id("other"), None);
    }
}
