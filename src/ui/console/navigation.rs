//! SQL editor navigation and execution previews. UI choices always retain source ranges.
use super::*;
use crate::sql::navigation as source;
use std::ops::Range;

mod cache;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Command {
    Back,
    Forward,
    LastEdit,
    Top,
    Bottom,
    FindWord,
    Context,
    MatchingBrace,
    WordForward,
    WordBackward,
    WordForwardSelect,
    WordBackwardSelect,
    ParagraphForward,
    ParagraphBackward,
    ParagraphForwardSelect,
    ParagraphBackwardSelect,
    GoToLine,
    Structure,
    ToggleBookmark,
    MnemonicBookmark,
    Bookmarks,
    NextIssue,
    PreviousIssue,
    Explorer,
    Declaration,
    Switcher,
    Actions,
    Settings,
}

pub const ACTIONS: &[(Command, &str, &str)] = &[
    (Command::Back, "Back", "⌥⌘Left"),
    (Command::Forward, "Forward", "⌥⌘Right"),
    (Command::LastEdit, "Last edit location", "⇧⌘Backspace"),
    (Command::Top, "Move caret to top", "⌘Page Up"),
    (Command::Bottom, "Move caret to bottom", "⌘Page Down"),
    (
        Command::FindWord,
        "Next occurrence of word at caret",
        if cfg!(target_os = "macos") {
            "⌥⌘G"
        } else {
            "Ctrl+F3"
        },
    ),
    (
        Command::Context,
        "Context information",
        if cfg!(target_os = "macos") {
            "⌥⌘I"
        } else {
            "Alt+Q"
        },
    ),
    (Command::MatchingBrace, "Matching brace", "⇧⌘M"),
    (Command::WordForward, "Move caret to next word", ""),
    (Command::WordBackward, "Move caret to previous word", ""),
    (
        Command::WordForwardSelect,
        "Move caret to next word with selection",
        "",
    ),
    (
        Command::WordBackwardSelect,
        "Move caret to previous word with selection",
        "",
    ),
    (
        Command::ParagraphForward,
        "Move caret forward a paragraph",
        "",
    ),
    (
        Command::ParagraphBackward,
        "Move caret backward a paragraph",
        "",
    ),
    (
        Command::ParagraphForwardSelect,
        "Move caret forward a paragraph with selection",
        "",
    ),
    (
        Command::ParagraphBackwardSelect,
        "Move caret backward a paragraph with selection",
        "",
    ),
    (Command::GoToLine, "Go to Line/Column…", "⌘L"),
    (
        Command::Structure,
        "File Structure…",
        if cfg!(target_os = "macos") {
            "⌘F12 / ⌥⌘7"
        } else {
            "Ctrl+F12 / Alt+7"
        },
    ),
    (
        Command::ToggleBookmark,
        "Toggle bookmark",
        if cfg!(target_os = "macos") {
            "⌥⇧⌘B"
        } else {
            "F11"
        },
    ),
    (
        Command::MnemonicBookmark,
        "Bookmark with mnemonic…",
        if cfg!(target_os = "macos") {
            "⌥⇧⌘M"
        } else {
            "⌘F11"
        },
    ),
    (
        Command::Bookmarks,
        "Bookmarks…",
        if cfg!(target_os = "macos") {
            "⌥⇧⌘K"
        } else {
            "⇧F11"
        },
    ),
    (Command::NextIssue, "Next highlighted issue", "F2"),
    (Command::PreviousIssue, "Previous highlighted issue", "⇧F2"),
    (
        Command::Explorer,
        "Select in Database Explorer",
        if cfg!(target_os = "macos") {
            "⌥⌘B"
        } else {
            "⌥⇧B"
        },
    ),
    (Command::Declaration, "Go to DDL / Declaration", "⌘B"),
    (Command::Switcher, "Recent tabs / Switcher", "Ctrl+Tab"),
    (Command::Actions, "Find navigation action…", "⇧⌘A"),
    (Command::Settings, "Editor navigation settings…", ""),
];

pub(super) fn menu(ui: &mut Ui, cmds: &mut Vec<EditCmd>) {
    for &(cmd, label, key) in ACTIONS {
        if ui
            .add(egui::Button::new(tr(label)).shortcut_text(crate::shortcuts::format(key)))
            .clicked()
        {
            cmds.push(EditCmd::Navigate(cmd));
            ui.close();
        }
    }
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Bookmark {
    pub position: usize,
    pub mnemonic: String,
    pub description: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Saved {
    pub bookmarks: Vec<Bookmark>,
    pub line_numbers: bool,
    pub word_end: bool,
    pub stop_at_line_break: bool,
    pub highest_priority: bool,
    #[serde(deserialize_with = "deserialize_bindings")]
    pub bindings: Vec<Binding>,
}

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    pub line_numbers: bool,
    pub word_end: bool,
    pub stop_at_line_break: bool,
    pub highest_priority: bool,
    #[serde(deserialize_with = "deserialize_bindings")]
    pub bindings: Vec<Binding>,
}

impl From<&Saved> for Settings {
    fn from(saved: &Saved) -> Self {
        Self {
            line_numbers: saved.line_numbers,
            word_end: saved.word_end,
            stop_at_line_break: saved.stop_at_line_break,
            highest_priority: saved.highest_priority,
            bindings: saved.bindings.clone(),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self::from(&Saved::default())
    }
}

impl Saved {
    pub fn apply_settings(&mut self, settings: &Settings) {
        self.line_numbers = settings.line_numbers;
        self.word_end = settings.word_end;
        self.stop_at_line_break = settings.stop_at_line_break;
        self.highest_priority = settings.highest_priority;
        self.bindings.clone_from(&settings.bindings);
    }
}

// Legacy console settings still deserialize through Saved. New state stores
// only bookmarks here, with preferences and bindings saved once for the app.
pub fn serialize_bookmarks<S: serde::Serializer>(
    saved: &Saved,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    #[derive(serde::Serialize)]
    struct Bookmarks<'a> {
        bookmarks: &'a [Bookmark],
    }
    serde::Serialize::serialize(
        &Bookmarks {
            bookmarks: &saved.bookmarks,
        },
        serializer,
    )
}
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Binding {
    pub command: Command,
    pub shortcut: egui::KeyboardShortcut,
}

// Read unit variants by name before decoding them, so a removed command or egui
// key only drops its binding. This also accepts the enum syntax in eframe's RON.
fn known_variant<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    struct Variant<T>(std::marker::PhantomData<T>);
    impl<'de, T: serde::de::DeserializeOwned> serde::de::DeserializeSeed<'de> for Variant<T> {
        type Value = Option<T>;
        fn deserialize<D: serde::Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> Result<Self::Value, D::Error> {
            deserializer.deserialize_identifier(self)
        }
    }
    impl<'de, T: serde::de::DeserializeOwned> serde::de::Visitor<'de> for Variant<T> {
        type Value = Option<T>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a shortcut command or key name")
        }
        fn visit_enum<A: serde::de::EnumAccess<'de>>(
            self,
            data: A,
        ) -> Result<Self::Value, A::Error> {
            let (value, variant) = data.variant_seed(self)?;
            serde::de::VariantAccess::unit_variant(variant)?;
            Ok(value)
        }
        fn visit_str<E: serde::de::Error>(self, name: &str) -> Result<Self::Value, E> {
            Ok(serde_json::from_value(serde_json::Value::String(name.to_owned())).ok())
        }
    }
    deserializer.deserialize_enum("ShortcutVariant", &[], Variant(std::marker::PhantomData))
}

fn deserialize_bindings<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<Binding>, D::Error> {
    #[derive(serde::Deserialize)]
    struct RawShortcut {
        modifiers: Modifiers,
        #[serde(deserialize_with = "known_variant")]
        logical_key: Option<Key>,
    }
    #[derive(serde::Deserialize)]
    struct RawBinding {
        #[serde(deserialize_with = "known_variant")]
        command: Option<Command>,
        shortcut: RawShortcut,
    }
    let raw = <Vec<RawBinding> as serde::Deserialize>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .filter_map(|b| {
            Some(Binding {
                command: b.command?,
                shortcut: egui::KeyboardShortcut::new(
                    b.shortcut.modifiers,
                    b.shortcut.logical_key?,
                ),
            })
        })
        .collect())
}

pub fn shortcut_allowed(shortcut: &egui::KeyboardShortcut, mac: bool) -> bool {
    let m = shortcut.modifiers;
    let key = shortcut.logical_key;
    if !(m.command || m.ctrl || m.mac_cmd) {
        return false;
    }
    if mac && (key == Key::F11 || (key == Key::F3 && m.ctrl)) {
        return false;
    }
    if key == Key::Tab || (mac && key == Key::Space) {
        return false;
    }
    let command = m.command || m.mac_cmd || (!mac && m.ctrl);
    if command
        && matches!(
            key,
            Key::A
                | Key::C
                | Key::X
                | Key::V
                | Key::Z
                | Key::Y
                | Key::F
                | Key::R
                | Key::L
                | Key::B
                | Key::D
                | Key::S
                | Key::Q
                | Key::W
                | Key::N
                | Key::Enter
                | Key::Equals
                | Key::Minus
                | Key::Plus
                | Key::Num0
                | Key::Slash
                | Key::Backspace
        )
    {
        return false;
    }
    true
}

pub fn take_binding(input: &mut egui::InputState, bindings: &[Binding]) -> Option<Command> {
    // Native layouts can emit Key and Text together (Option on macOS, AltGr
    // elsewhere). Preserve that input instead of firing a navigation action.
    if input.events.iter().any(|e| matches!(e, Event::Text(_))) {
        return None;
    }
    let mut command = None;
    input.events.retain(|e| {
        if let Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } = e
            && let Some(b) = bindings.iter().find(|b| {
                shortcut_allowed(&b.shortcut, cfg!(target_os = "macos"))
                    && b.shortcut.logical_key == *key
                    && modifiers.matches_exact(b.shortcut.modifiers)
            })
        {
            command = Some(b.command);
            return false;
        }
        true
    });
    command
}
impl Default for Saved {
    fn default() -> Self {
        Self {
            bookmarks: Vec::new(),
            line_numbers: true,
            word_end: true,
            stop_at_line_break: true,
            highest_priority: false,
            bindings: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Popup {
    Line,
    Structure,
    Mnemonic,
    Context,
    Actions,
    Settings,
}

pub(super) struct GutterRun {
    revision: u64,
    source: (Option<ConnId>, Option<String>, Driver),
    range: Range<usize>,
}

#[derive(Default)]
pub struct State {
    pub caret: Option<usize>,
    pub anchor: Option<usize>,
    pub pending_location: Option<usize>,
    pub saved: Saved,
    pub pending: Vec<Command>,
    popup: Option<Popup>,
    input: String,
    error: Option<&'static str>,
    selected: Vec<usize>,
    highlight: usize,
    focus: bool,
    snapshot: Option<u64>,
    source: Option<(Option<ConnId>, Option<String>, Driver)>,
    previous: Option<String>,
    revision: Option<u64>,
    index: Option<Arc<cache::TextIndex>>,
    scope_key: Option<(Option<(usize, usize)>, bool, Vec<usize>)>,
    scopes: Vec<Range<usize>>,
    scope_chars: Vec<Range<usize>>,
    scope_epoch: u64,
    outline_cache: Option<cache::OutlineCache>,
    filter_key: Option<(u64, Driver, String)>,
    visible: Arc<Vec<usize>>,
    bookmark_key: Option<(u64, u64)>,
    bookmark_lines: Arc<std::collections::HashSet<usize>>,
    pub jump: Option<(usize, usize)>,
    suppress_jump: bool,
    #[cfg(test)]
    pub(crate) index_builds: usize,
    #[cfg(test)]
    scope_builds: usize,
    #[cfg(test)]
    filter_builds: usize,
    #[cfg(test)]
    outline_builds: usize,
    pub(super) gutter_run: Option<GutterRun>,
    recording: Option<Command>,
}

impl State {
    pub fn is_open(&self) -> bool {
        self.popup.is_some()
    }
    fn open(&mut self, popup: Popup) {
        self.popup = Some(popup);
        self.input.clear();
        self.error = None;
        self.selected.clear();
        self.highlight = 0;
        self.focus = true;
        self.snapshot = None;
        self.source = None;
        self.recording = None;
    }
    pub fn problems(&mut self, text: &str, driver: Driver) -> &[source::Issue] {
        self.sync_text(text);
        self.ensure_index(text, driver);
        &self.index.as_ref().unwrap().issues
    }

    pub(super) fn execution_ranges(
        &self,
        text: &str,
        cursor: Option<(usize, usize)>,
        driver: Driver,
    ) -> Vec<Range<usize>> {
        let temporary;
        let index = if let Some(index) = self
            .index
            .as_ref()
            .filter(|i| i.revision == self.revision() && i.driver == driver)
        {
            index.as_ref()
        } else {
            temporary = cache::TextIndex::new(text, driver, self.revision());
            &temporary
        };
        if self.popup == Some(Popup::Structure) {
            return self
                .selected
                .iter()
                .filter_map(|&i| index.statements.get(i).map(|s| s.start..s.end))
                .collect();
        }
        if let Some((a, b)) = cursor.filter(|(a, b)| a != b)
            && !text[index.byte(a)..index.byte(b)].trim().is_empty()
        {
            return vec![index.byte(a)..index.byte(b)];
        }
        index
            .statement_at(text, cursor.map_or(index.characters.len() - 1, |(_, b)| b))
            .map(|i| vec![index.statements[i].start..index.statements[i].end])
            .unwrap_or_default()
    }

    pub(crate) fn sync_text(&mut self, text: &str) {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hash);
        self.sync_changed_text(text, hash.finish());
    }

    fn sync_changed_text(&mut self, text: &str, revision: u64) {
        if self.revision == Some(revision) {
            return;
        }
        if let Some(old) = self
            .previous
            .as_ref()
            .filter(|_| !self.saved.bookmarks.is_empty())
        {
            let before: Vec<_> = old.chars().collect();
            let after: Vec<_> = text.chars().collect();
            let prefix = before
                .iter()
                .zip(&after)
                .take_while(|(a, b)| a == b)
                .count();
            let suffix = before[prefix..]
                .iter()
                .rev()
                .zip(after[prefix..].iter().rev())
                .take_while(|(a, b)| a == b)
                .count();
            let end = before.len() - suffix;
            for bookmark in &mut self.saved.bookmarks {
                if bookmark.position >= end {
                    bookmark.position = bookmark
                        .position
                        .saturating_add(after.len())
                        .saturating_sub(before.len())
                        .min(after.len());
                } else if bookmark.position > prefix {
                    bookmark.position = prefix;
                }
            }
        }
        self.previous = Some(text.to_owned());
        self.revision = Some(revision);
        self.index = None;
        self.scope_key = None;
        self.filter_key = None;
        if self.snapshot.is_some_and(|s| s != revision) {
            self.popup = None;
            self.snapshot = None;
            self.source = None;
        }
    }
}

/// An unfilled, continuous box around the executed character range. On multiple visual
/// rows its first/last edges follow the endpoints, including soft-wrapped editor rows.
fn outline_chars(
    galley: &egui::Galley,
    ranges: &[Range<usize>],
    origin: egui::Pos2,
    color: egui::Color32,
) -> Vec<egui::Shape> {
    let mut offset = 0;
    let row_starts: Vec<_> = galley
        .rows
        .iter()
        .map(|row| {
            let start = offset;
            offset += row.char_count_including_newline().0;
            start
        })
        .collect();
    let cursor = |position: usize| {
        let mut row = row_starts
            .partition_point(|&s| s <= position)
            .saturating_sub(1);
        if row > 0 && position == row_starts[row] && !galley.rows[row - 1].ends_with_newline {
            row -= 1;
        }
        egui::epaint::text::cursor::LayoutCursor {
            row,
            column: egui::text::CharIndex(
                position
                    .saturating_sub(row_starts[row])
                    .min(galley.rows[row].char_count_excluding_newline().0),
            ),
        }
    };
    ranges
        .iter()
        .filter_map(|range| {
            if range.is_empty() || galley.rows.is_empty() || range.end > offset {
                return None;
            }
            let first = cursor(range.start);
            let mut last = cursor(range.end);
            // Selecting the newline does not select the next empty editor row.
            if last.column.0 == 0 && last.row > first.row {
                last.row -= 1;
                last.column = egui::text::CharIndex(galley.rows[last.row].glyphs.len());
            }
            let head = &galley.rows[first.row];
            let tail = &galley.rows[last.row];
            let start_x = head.pos.x + head.x_offset(first.column) - 0.5;
            let end_x = tail.pos.x + tail.x_offset(last.column) + 0.5;
            let top = head.min_y() - 0.5;
            let bottom = tail.max_y() + 0.5;
            let mut points = if first.row == last.row {
                vec![
                    pos2(start_x, top),
                    pos2(end_x, top),
                    pos2(end_x, bottom),
                    pos2(start_x, bottom),
                ]
            } else {
                let left = galley.rows[first.row + 1..=last.row]
                    .iter()
                    .map(|row| row.pos.x - 0.5)
                    .fold(start_x, f32::min);
                let right = galley.rows[first.row..last.row]
                    .iter()
                    .map(|row| row.rect().right() + 0.5)
                    .fold(end_x, f32::max);
                vec![
                    pos2(start_x, top),
                    pos2(right, top),
                    pos2(right, tail.min_y()),
                    pos2(end_x, tail.min_y()),
                    pos2(end_x, bottom),
                    pos2(left, bottom),
                    pos2(left, head.max_y()),
                    pos2(start_x, head.max_y()),
                ]
            };
            for point in &mut points {
                *point += origin.to_vec2();
            }
            Some(egui::Shape::closed_line(points, Stroke::new(1.0, color)))
        })
        .collect()
}

impl Console {
    pub fn navigate_to(&mut self, ctx: &egui::Context, position: usize, select_to: Option<usize>) {
        let from = self
            .navigation
            .caret
            .or(self.cursor.map(|(_, b)| b))
            .unwrap_or(0);
        let n = self.text.chars().count();
        let position = position.min(n);
        self.set_cursor(ctx, select_to.unwrap_or(position).min(n), position);
        if from != position && !self.navigation.suppress_jump {
            let origin = self.navigation.jump.map_or(from, |(origin, _)| origin);
            self.navigation.jump = Some((origin, position));
        }
        self.scroll_to = Some(position);
        ctx.memory_mut(|m| m.request_focus(self.editor_id()));
    }

    pub fn open_structure(&mut self, driver: Driver) {
        self.navigation.sync_text(&self.text);
        self.navigation.ensure_index(&self.text, driver);
        self.navigation.open(Popup::Structure);
        self.navigation.snapshot = Some(self.navigation.revision());
        self.navigation.source = Some((self.conn, self.db.clone(), driver));
        let index = self.navigation.index();
        self.navigation.highlight = index
            .statement_at(
                &self.text,
                self.cursor.map_or(index.characters.len() - 1, |(_, b)| b),
            )
            .unwrap_or(0);
        self.navigation.selected = vec![self.navigation.highlight];
        self.completion = None;
    }

    pub(super) fn navigation_keys(&mut self, ui: &Ui, cmds: &mut Vec<EditCmd>) {
        if self.navigation.is_open() {
            return;
        }
        if !ui.memory(|m| m.has_focus(self.editor_id())) {
            return;
        }
        if let Some(command) = ui.input_mut(|i| take_binding(i, &self.navigation.saved.bindings)) {
            cmds.push(EditCmd::Navigate(command));
            return;
        }
        let c = Modifiers::COMMAND;
        let s = Modifiers::SHIFT;
        let a = Modifiers::ALT;
        let bindings = [
            (c, Key::L, Command::GoToLine),
            (c, Key::F12, Command::Structure),
            (
                if cfg!(target_os = "macos") { c | a } else { a },
                Key::Num7,
                Command::Structure,
            ),
            (
                if cfg!(target_os = "macos") {
                    c | a | s
                } else {
                    s
                },
                if cfg!(target_os = "macos") {
                    Key::K
                } else {
                    Key::F11
                },
                Command::Bookmarks,
            ),
            (
                if cfg!(target_os = "macos") {
                    c | a | s
                } else {
                    c
                },
                if cfg!(target_os = "macos") {
                    Key::M
                } else {
                    Key::F11
                },
                Command::MnemonicBookmark,
            ),
            (
                if cfg!(target_os = "macos") {
                    c | a | s
                } else {
                    Modifiers::NONE
                },
                if cfg!(target_os = "macos") {
                    Key::B
                } else {
                    Key::F11
                },
                Command::ToggleBookmark,
            ),
            (s, Key::F2, Command::PreviousIssue),
            (Modifiers::NONE, Key::F2, Command::NextIssue),
            (
                if cfg!(target_os = "macos") { c | a } else { a },
                if cfg!(target_os = "macos") {
                    Key::I
                } else {
                    Key::Q
                },
                Command::Context,
            ),
            (c | s, Key::M, Command::MatchingBrace),
            (c, Key::PageUp, Command::Top),
            (c, Key::PageDown, Command::Bottom),
            (c | s, Key::A, Command::Actions),
            (
                if cfg!(target_os = "macos") {
                    c | a
                } else {
                    a | s
                },
                Key::B,
                Command::Explorer,
            ),
            (c, Key::B, Command::Declaration),
            (
                if cfg!(target_os = "macos") { a } else { c },
                Key::ArrowRight,
                Command::WordForward,
            ),
            (
                if cfg!(target_os = "macos") { a } else { c },
                Key::ArrowLeft,
                Command::WordBackward,
            ),
            (
                (if cfg!(target_os = "macos") { a } else { c }) | s,
                Key::ArrowRight,
                Command::WordForwardSelect,
            ),
            (
                (if cfg!(target_os = "macos") { a } else { c }) | s,
                Key::ArrowLeft,
                Command::WordBackwardSelect,
            ),
        ];
        ui.input_mut(|i| {
            for (mods, key, cmd) in bindings { if take_key(i, mods, key) { cmds.push(EditCmd::Navigate(cmd)); } }
            let mut find_word = if cfg!(target_os = "macos") { take_key(i, c | a, Key::G) } else { false };
            if !cfg!(target_os = "macos") {
                i.events.retain(|e| if matches!(e, Event::Key{key:Key::F3,pressed:true,modifiers:m,..} if m.ctrl && !m.alt && !m.shift && !m.mac_cmd){find_word=true;false}else{true});
            }
            if find_word {cmds.push(EditCmd::Navigate(Command::FindWord));}
            if !cfg!(target_os="macos") && self.find.is_none() && take_key(i, c, Key::G) { cmds.push(EditCmd::Navigate(Command::GoToLine)); }
        });
    }

    pub fn navigation_command(
        &mut self,
        ctx: &egui::Context,
        driver: Driver,
        cmd: Command,
        actions: &mut Vec<ConsoleAction>,
    ) {
        use Command::*;
        if let Some(editor) = &mut self.routine
            && editor.page != crate::ui::routine_dialog::RoutinePage::Definition
            && !matches!(cmd, Back | Forward | LastEdit | Switcher | Bookmarks)
        {
            editor.page = crate::ui::routine_dialog::RoutinePage::Definition;
            self.navigation.pending.push(cmd);
            ctx.request_repaint();
            return;
        }
        self.navigation.sync_text(&self.text);
        self.navigation.ensure_index(&self.text, driver);
        let suppressed = self.navigation.suppress_jump;
        self.navigation.suppress_jump |= matches!(
            cmd,
            WordForward
                | WordBackward
                | WordForwardSelect
                | WordBackwardSelect
                | ParagraphForward
                | ParagraphBackward
                | ParagraphForwardSelect
                | ParagraphBackwardSelect
        );
        let pos = self
            .navigation
            .caret
            .or(self.cursor.map(|(_, b)| b))
            .unwrap_or(0);
        match cmd {
            Back | Forward | LastEdit | Explorer | Declaration | Switcher => {
                actions.push(ConsoleAction::Navigate(cmd))
            }
            Top => self.navigate_to(ctx, 0, None),
            Bottom => self.navigate_to(ctx, self.text.chars().count(), None),
            ParagraphForward
            | ParagraphBackward
            | ParagraphForwardSelect
            | ParagraphBackwardSelect => {
                let p = source::paragraph(
                    &self.text,
                    pos,
                    matches!(cmd, ParagraphForward | ParagraphForwardSelect),
                );
                let anchor = matches!(cmd, ParagraphForwardSelect | ParagraphBackwardSelect)
                    .then_some(self.navigation.anchor.unwrap_or(pos));
                self.navigate_to(ctx, p, anchor);
            }
            WordForward | WordBackward | WordForwardSelect | WordBackwardSelect => {
                let chars: Vec<_> = self.text.chars().collect();
                let ident = |ch: char| ch.is_alphanumeric() || ch == '_';
                let mut p = pos.min(chars.len());
                let mut line_stop = false;
                if matches!(cmd, WordForward | WordForwardSelect) {
                    if self.navigation.saved.word_end {
                        while p < chars.len() && !ident(chars[p]) {
                            if chars[p] == '\n' && self.navigation.saved.stop_at_line_break {
                                p += 1;
                                line_stop = true;
                                break;
                            }
                            p += 1;
                        }
                    }
                    while !line_stop && p < chars.len() && ident(chars[p]) {
                        p += 1;
                    }
                    if !self.navigation.saved.word_end {
                        while p < chars.len() && !ident(chars[p]) {
                            if chars[p] == '\n' && self.navigation.saved.stop_at_line_break {
                                p += 1;
                                break;
                            }
                            p += 1;
                        }
                    }
                } else {
                    while p > 0 && !ident(chars[p - 1]) {
                        p -= 1;
                        if chars[p] == '\n' && self.navigation.saved.stop_at_line_break {
                            line_stop = true;
                            break;
                        }
                    }
                    while !line_stop && p > 0 && ident(chars[p - 1]) {
                        p -= 1;
                    }
                }
                let anchor = matches!(cmd, WordForwardSelect | WordBackwardSelect)
                    .then_some(self.navigation.anchor.unwrap_or(pos));
                self.navigate_to(ctx, p, anchor);
            }
            MatchingBrace => {
                if let Some(p) = source::matching_brace(&self.text, pos, driver) {
                    self.navigate_to(ctx, p, None);
                }
            }
            FindWord => {
                if let Some(r) = source::reference_at(&self.text, pos, driver) {
                    self.open_find(false);
                    if let Some(f) = &mut self.find {
                        f.query = r.parts[r.component].name.clone();
                        f.focus = false;
                        f.whole_word = true;
                    }
                    if let Some((a, b)) = self
                        .find_matches()
                        .into_iter()
                        .find(|&(a, b)| a <= pos && pos <= b)
                    {
                        self.set_cursor(ctx, a, b);
                    }
                    self.find_step(ctx, true);
                }
            }
            GoToLine => self.navigation.open(Popup::Line),
            Structure => self.open_structure(driver),
            ToggleBookmark | MnemonicBookmark => {
                let line = source::line_column(&self.text, pos).0;
                let existing = self
                    .navigation
                    .saved
                    .bookmarks
                    .iter()
                    .position(|b| source::line_column(&self.text, b.position).0 == line);
                if cmd == ToggleBookmark {
                    if let Some(i) = existing {
                        self.navigation.saved.bookmarks.remove(i);
                    } else {
                        self.navigation.saved.bookmarks.push(Bookmark {
                            position: pos,
                            ..Default::default()
                        });
                    }
                } else {
                    self.navigation.open(Popup::Mnemonic);
                }
            }
            Bookmarks => actions.push(ConsoleAction::Navigate(cmd)),
            NextIssue | PreviousIssue => {
                let highest = self.navigation.saved.highest_priority;
                let problems = self.navigation.problems(&self.text, driver);
                let errors_only = highest && problems.iter().any(|p| p.error);
                let problems: Vec<_> = problems
                    .iter()
                    .filter(|p| !errors_only || p.error)
                    .collect();
                let next = if cmd == NextIssue {
                    problems
                        .iter()
                        .find(|p| p.position > pos)
                        .or(problems.first())
                } else {
                    problems
                        .iter()
                        .rev()
                        .find(|p| p.position < pos)
                        .or(problems.last())
                };
                let next = next.map(|p| p.position);
                if let Some(position) = next {
                    self.navigate_to(ctx, position, None);
                }
            }
            Context => self.navigation.open(Popup::Context),
            Actions => self.navigation.open(Popup::Actions),
            Settings => self.navigation.open(Popup::Settings),
        }
        if cmd == FindWord {
            let to = self.cursor.map_or(pos, |(_, b)| b);
            if to != pos {
                self.navigation.jump = Some((pos, to));
            }
        }
        self.navigation.suppress_jump = suppressed;
    }

    pub(super) fn navigation_ui(
        &mut self,
        ui: &Ui,
        cx: &ConsoleCtx<'_>,
        actions: &mut Vec<ConsoleAction>,
    ) {
        let ctx = ui.ctx();
        for cmd in std::mem::take(&mut self.navigation.pending) {
            self.navigation_command(ctx, cx.driver, cmd, actions);
        }
        if let Some(target) = self.navigation.gutter_run.take()
            && target.revision == self.navigation.revision()
            && target.source == (self.conn, self.db.clone(), cx.driver)
        {
            actions.push(ConsoleAction::RunStatements(
                self.prepare_ranges(cx.driver, vec![target.range]),
            ));
        }
        if self
            .navigation
            .source
            .as_ref()
            .is_some_and(|s| s != &(self.conn, self.db.clone(), cx.driver))
        {
            self.navigation.popup = None;
            self.navigation.source = None;
        }
        let Some(popup) = self.navigation.popup else {
            return;
        };
        // Editor commands can change the text after the editor was drawn this frame.
        self.navigation.ensure_index(&self.text, cx.driver);
        self.completion = None;
        let title = match popup {
            Popup::Line => "Go to Line/Column",
            Popup::Structure => "File Structure",
            Popup::Mnemonic => "Bookmark with mnemonic",
            Popup::Context => "Context information",
            Popup::Actions => "Find navigation action",
            Popup::Settings => "Editor navigation settings",
        };
        let mut open = true;
        let mut close = false;
        let mut command = None;
        let mut navigate = None;
        let mut execute = false;
        let mut all = false;
        let mut submit = false;
        // Consume before the editor can interpret popup navigation as SQL edits.
        let recording = self.navigation.recording.take();
        if let Some(command) = recording {
            let key = ctx.input_mut(|i| {
                let mut key = None;
                i.events.retain(|e| {
                    if let Event::Key {
                        key: k,
                        pressed: true,
                        modifiers,
                        ..
                    } = e
                    {
                        key = Some((*k, *modifiers));
                        false
                    } else {
                        true
                    }
                });
                key
            });
            if let Some((key, modifiers)) = key {
                if key == Key::Escape {
                    self.navigation.error = None;
                }
                if key != Key::Escape {
                    let shortcut = egui::KeyboardShortcut::new(modifiers, key);
                    if shortcut_allowed(&shortcut, cfg!(target_os = "macos"))
                        && !ctx.input(|i| i.events.iter().any(|e| matches!(e, Event::Text(_))))
                    {
                        self.navigation
                            .saved
                            .bindings
                            .retain(|b| b.command != command && b.shortcut != shortcut);
                        self.navigation
                            .saved
                            .bindings
                            .push(Binding { command, shortcut });
                        self.navigation.error = None;
                    } else {
                        self.navigation.recording = Some(command);
                        self.navigation.error =
                            Some("Use Ctrl or Command; this shortcut is reserved or types text.");
                    }
                }
            } else {
                self.navigation.recording = Some(command);
            }
        }
        let (up, down, enter, esc) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        egui::Window::new(tr(title)).id(egui::Id::new(("editor_navigation", self.id))).open(&mut open).collapsible(false).resizable(true).default_width(540.0).anchor(egui::Align2::CENTER_TOP, vec2(0.0, 85.0)).show(ctx, |ui| {
            if matches!(popup, Popup::Line | Popup::Structure | Popup::Actions | Popup::Mnemonic) {
                let hint = match popup { Popup::Line => "line:column", Popup::Mnemonic => "One letter or digit", _ => "Type to filter" };
                let r = ui.add(egui::TextEdit::singleline(&mut self.navigation.input).hint_text(tr(hint)).desired_width(f32::INFINITY));
                if self.navigation.focus { r.request_focus(); self.navigation.focus = false; }
                if r.changed() && popup != Popup::Mnemonic { self.navigation.highlight = usize::MAX; self.navigation.selected.clear(); }
            }
            match popup {
                Popup::Line => { submit = enter || ui.button(tr("Go")).clicked(); }
                Popup::Mnemonic => { submit = enter || ui.button(tr("Save bookmark")).clicked(); }
                Popup::Structure => {
                    let index = self.navigation.index();
                    let ss = &index.statements;
                    let visible = self.navigation.filtered_statements();
                    if (up || down) && let Some(index) = move_choice(&visible, self.navigation.highlight, up, down) { self.navigation.highlight = index; self.navigation.selected = vec![index]; }
                    if !visible.contains(&self.navigation.highlight) && let Some(&index)=visible.first(){self.navigation.highlight=index;self.navigation.selected=vec![index];}
                    ui.label(tr("Selected statements are outlined in the editor. Ctrl/Command-click selects several."));
                    egui::ScrollArea::vertical().max_height(300.0).show_rows(ui, ui.spacing().interact_size.y, visible.len(), |ui, rows| {
                        for &i in &visible[rows] {
                            let s = &ss[i];
                            let r = ui.add(egui::Button::selectable(self.navigation.selected.contains(&i), &index.structure[i].label).wrap_mode(egui::TextWrapMode::Truncate)).on_hover_text(&s.text);
                            if r.clicked() {
                                let multi = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
                                let shift = ui.input(|i| i.modifiers.shift);
                                if shift { self.navigation.selected = (i.min(self.navigation.highlight)..=i.max(self.navigation.highlight)).collect(); }
                                else if multi { if self.navigation.selected.contains(&i) { self.navigation.selected.retain(|j| *j != i); } else { self.navigation.selected.push(i); } }
                                else { self.navigation.selected = vec![i]; }
                                self.navigation.highlight = i;
                            }
                            if r.double_clicked() { navigate = Some(index.statement_chars[i].start); }
                            if r.secondary_clicked() && !self.navigation.selected.contains(&i) {self.navigation.selected=vec![i];self.navigation.highlight=i;}
                            r.context_menu(|ui| { if ui.add_enabled(!self.is_busy(), egui::Button::new(tr("Execute selected statements"))).clicked() { execute = true; ui.close(); } });
                        }
                    });
                    ui.horizontal(|ui| {
                        if !visible.is_empty() && (enter || ui.button(tr("Go to statement")).clicked()) {
                            navigate = index.statement_chars.get(self.navigation.highlight).map(|r| r.start);
                        }
                        execute |= ui.add_enabled(!self.is_busy() && !self.navigation.selected.is_empty(), egui::Button::new(tr("Execute selected statements"))).clicked();
                        all = ui.add_enabled(!self.is_busy() && !ss.is_empty(), egui::Button::new(tr("Run all statements"))).clicked();
                    });
                    if ss.is_empty() { ui.label(tr("No executable statements")); }
                }
                Popup::Actions => {
                    let rows: Vec<_> = ACTIONS.iter().enumerate().filter(|(_,(_,label,_))| label.to_lowercase().contains(&self.navigation.input.to_lowercase())).map(|(i,_)| i).collect();
                    if let Some(i) = move_choice(&rows, self.navigation.highlight, up, down) { self.navigation.highlight = i; }
                    egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                        for &i in &rows { let (cmd,label,key) = ACTIONS[i]; if ui.selectable_label(self.navigation.highlight == i, format!("{}    {}", tr(label), crate::shortcuts::format(key))).clicked() { command = Some(cmd); } }
                    });
                    if enter { command = rows.iter().find(|&&i| i == self.navigation.highlight).or(rows.first()).map(|&i| ACTIONS[i].0); }
                }
                Popup::Context => {
                    let index = self.navigation.index();
                    if let Some(i) = index.statement_at(&self.text, self.cursor.map_or(0, |(_,b)| b)) { ui.monospace(&index.statements[i].text); }
                    else { ui.label(tr("No statement at the caret")); }
                }
                Popup::Settings => {
                    ui.checkbox(&mut self.navigation.saved.line_numbers, tr("Show line numbers"));
                    ui.checkbox(&mut self.navigation.saved.word_end, tr("Next word stops at the end of the word"));
                    ui.checkbox(&mut self.navigation.saved.stop_at_line_break, tr("Stop word movement at line breaks"));
                    ui.checkbox(&mut self.navigation.saved.highest_priority, tr("Problems with highest priority only"));
                    ui.separator();ui.label(tr("Keyboard shortcuts"));
                    if self.navigation.recording.is_some(){ui.label(tr("Press a key combination. Esc cancels recording."));}
                    egui::ScrollArea::vertical().max_height(250.0).show(ui,|ui|{
                        for &(command,label,default) in ACTIONS{
                            ui.push_id(format!("{command:?}"),|ui|{ui.horizontal(|ui|{
                                ui.label(tr(label));
                                let binding=self.navigation.saved.bindings.iter().find(|b|b.command==command);
                                ui.monospace(binding.map(|b|ui.ctx().format_shortcut(&b.shortcut)).unwrap_or_else(||crate::shortcuts::format(default)));
                                if ui.button(tr("Record shortcut")).clicked(){self.navigation.recording=Some(command);}
                                if ui.button(tr("Reset")).clicked(){self.navigation.saved.bindings.retain(|b|b.command!=command);}
                            });});
                        }
                    });
                }
            }
            if let Some(error) = &self.navigation.error { ui.colored_label(pal(ui).error, tr(*error)); }
            close |= ui.button(tr("Cancel / Close")).clicked();
        });
        if submit {
            if popup == Popup::Line {
                match source::position(&self.text, &self.navigation.input) {
                    Ok(p) => navigate = Some(p),
                    Err(e) => self.navigation.error = Some(e),
                }
            } else {
                let s = self.navigation.input.trim();
                if s.chars().count() == 1 && s.chars().all(|c| c.is_ascii_alphanumeric()) {
                    let position = self.cursor.map_or(0, |(_, b)| b);
                    let line = source::line_column(&self.text, position).0;
                    let description = self
                        .navigation
                        .saved
                        .bookmarks
                        .iter()
                        .find(|b| source::line_column(&self.text, b.position).0 == line)
                        .map(|b| b.description.clone())
                        .unwrap_or_default();
                    self.navigation.saved.bookmarks.retain(|b| {
                        !b.mnemonic.eq_ignore_ascii_case(s)
                            && source::line_column(&self.text, b.position).0 != line
                    });
                    self.navigation.saved.bookmarks.push(Bookmark {
                        position,
                        mnemonic: s.to_ascii_uppercase(),
                        description,
                    });
                    actions.push(ConsoleAction::BookmarkAssigned(s.to_ascii_uppercase()));
                    close = true;
                } else {
                    self.navigation.error = Some("Choose one letter or digit");
                }
            }
        }
        if (execute || all) && !self.is_busy() {
            let statements = self.prepare_run(cx.driver, all);
            actions.push(ConsoleAction::RunStatements(statements));
            close = true;
        }
        if let Some(p) = navigate {
            self.navigate_to(ctx, p, None);
            close = true;
        }
        if close || esc || !open || command.is_some() {
            self.navigation.popup = None;
            self.navigation.snapshot = None;
            self.navigation.source = None;
            ctx.memory_mut(|m| m.request_focus(self.editor_id()));
        }
        if let Some(cmd) = command {
            self.navigation_command(ctx, cx.driver, cmd, actions);
        }
    }
}

fn move_choice(rows: &[usize], current: usize, up: bool, down: bool) -> Option<usize> {
    if rows.is_empty() {
        return None;
    }
    let at = rows.iter().position(|&i| i == current).unwrap_or(0);
    Some(
        rows[if up {
            (at + rows.len() - 1) % rows.len()
        } else if down {
            (at + 1) % rows.len()
        } else {
            at
        }],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;

    #[test]
    fn context_popup_rebuilds_after_same_frame_edits() {
        for command in [
            Some(EditCmd::Format),
            Some(EditCmd::ToggleComment),
            Some(EditCmd::ReplaceAll),
            None,
        ] {
            let mut h = egui_kittest::Harness::builder().build_ui_state(
                move |ui, console: &mut Console| {
                    let cx = ConsoleCtx {
                        connections: &[],
                        schemas: None,
                        driver: Driver::Postgres,
                        dark: true,
                        font_size: 14.0,
                        columns: None,
                        auto_complete: false,
                    };
                    let mut actions = Vec::new();
                    console.editor(ui, &cx, &mut Vec::new());
                    console.navigation_command(ui.ctx(), cx.driver, Command::Context, &mut actions);
                    console.open_find(true);
                    let find = console.find.as_mut().unwrap();
                    find.query = "1".into();
                    find.replace = "42".into();
                    if let Some(command) = command {
                        console.apply_cmd(ui.ctx(), &cx, command, &mut actions);
                    } else {
                        console.insert_text(ui.ctx(), "42");
                    }
                    console.navigation_ui(ui, &cx, &mut actions);
                    assert_eq!(
                        console.navigation.index().revision,
                        console.navigation.revision()
                    );
                },
                Console::new(1, "test".into(), "select 1 from notes;".into(), None),
            );
            h.run_steps(1);
        }
    }

    #[test]
    fn word_selection_uses_the_same_endpoints_as_movement() {
        let text = "order_id, next\n\nسلام.value";
        for word_end in [false, true] {
            for stop_at_line_break in [false, true] {
                for (key, start) in [(Key::ArrowRight, 0), (Key::ArrowLeft, text.chars().count())] {
                    let mut h = harness(text, false);
                    let ctx = h.ctx.clone();
                    h.state_mut().console.navigation.saved.word_end = word_end;
                    h.state_mut().console.navigation.saved.stop_at_line_break = stop_at_line_break;
                    h.state_mut().console.navigate_to(&ctx, start, None);
                    h.run_steps(1);
                    let mods = if cfg!(target_os = "macos") {
                        Modifiers::ALT
                    } else {
                        Modifiers::CTRL
                    };
                    h.key_press_modifiers(mods, key);
                    h.run_steps(2);
                    let endpoint = h.state().console.navigation.caret;
                    h.state_mut().console.navigate_to(&ctx, start, None);
                    h.run_steps(1);
                    h.key_press_modifiers(mods | Modifiers::SHIFT, key);
                    h.run_steps(2);
                    assert_eq!(h.state().console.navigation.caret, endpoint);
                    assert_eq!(h.state().console.navigation.anchor, Some(start));
                    h.key_press_modifiers(mods | Modifiers::SHIFT, key);
                    h.run_steps(2);
                    assert_eq!(h.state().console.navigation.anchor, Some(start));
                    assert_eq!(h.state().console.text, text);
                }
            }
        }
    }

    #[test]
    fn text_events_protect_alt_defaults_including_altgr() {
        for (mods, key) in [
            (Modifiers::ALT, Key::Num7),
            (Modifiers::ALT, Key::Q),
            (Modifiers::ALT | Modifiers::SHIFT, Key::B),
        ] {
            for ctrl in [false, true] {
                let modifiers = if ctrl { mods | Modifiers::CTRL } else { mods };
                let input = egui::RawInput {
                    events: vec![
                        Event::Key {
                            key,
                            physical_key: None,
                            pressed: true,
                            repeat: false,
                            modifiers,
                        },
                        Event::Text("@".into()),
                    ],
                    ..Default::default()
                };
                let mut output = egui::Context::default().run_ui(input, |ui| {
                    ui.input_mut(|i| {
                        assert!(!take_key(i, mods, key));
                        assert_eq!(i.events.len(), 2);
                    });
                });
                output.textures_delta.clear();
            }
        }
    }

    #[test]
    fn many_selected_scopes_reuse_geometry_and_only_paint_visible_rows() {
        let text: String = (0..5000).map(|i| format!("SELECT {i};\n")).collect();
        let ctx = egui::Context::default();
        let mut galley = None;
        let mut output = ctx.run_ui(egui::RawInput::default(), |ctx| {
            galley = Some(ctx.fonts_mut(|f| {
                f.layout_no_wrap(text.clone(), FontId::monospace(14.0), egui::Color32::WHITE)
            }));
        });
        output.textures_delta.clear();
        let galley = galley.unwrap();
        let mut state = State::new(&text);
        state.ensure_index(&text, Driver::Postgres);
        state.open(Popup::Structure);
        state.selected = (0..5000).collect();
        state.update_scope(&text, Some((0, 0)), Driver::Postgres);
        let clip = Rect::from_min_size(egui::Pos2::ZERO, vec2(1000.0, 100.0));
        for origin in [egui::Pos2::ZERO, pos2(0.0, -7000.0)] {
            let shapes = state.outlines(&galley, origin, egui::Color32::GREEN, clip);
            assert!(!shapes.is_empty() && shapes.len() < 16);
            assert!(
                shapes
                    .iter()
                    .all(|s| s.visual_bounding_rect().intersects(clip))
            );
        }
        assert_eq!(
            state.outline_builds, 1,
            "scrolling must reuse scope geometry"
        );
    }

    #[test]
    fn recordings_require_command_modifiers_and_preserve_reserved_keys() {
        for mac in [false, true] {
            for mods in [
                Modifiers::NONE,
                Modifiers::SHIFT,
                Modifiers::ALT,
                Modifiers::ALT | Modifiers::SHIFT,
            ] {
                assert!(!shortcut_allowed(
                    &egui::KeyboardShortcut::new(mods, Key::J),
                    mac
                ));
            }
            assert!(shortcut_allowed(
                &egui::KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::ALT, Key::J),
                mac
            ));
            assert!(!shortcut_allowed(
                &egui::KeyboardShortcut::new(Modifiers::COMMAND, Key::Num0),
                mac
            ));
            assert!(!shortcut_allowed(
                &egui::KeyboardShortcut::new(Modifiers::COMMAND, Key::C),
                mac
            ));
        }
        assert!(!shortcut_allowed(
            &egui::KeyboardShortcut::new(Modifiers::CTRL, Key::F3),
            true
        ));
        assert!(!shortcut_allowed(
            &egui::KeyboardShortcut::new(Modifiers::COMMAND, Key::F11),
            true
        ));
    }

    #[test]
    fn unfocused_editor_leaves_grid_navigation_keys_available() {
        struct Keys {
            console: Console,
            commands: Vec<EditCmd>,
            remaining: Vec<Key>,
        }
        let mut h = egui_kittest::Harness::builder().build_ui_state(
            |ui, state: &mut Keys| {
                state.console.navigation_keys(ui, &mut state.commands);
                state.remaining.extend(ui.input(|i| {
                    i.events
                        .iter()
                        .filter_map(|e| {
                            if let Event::Key {
                                key, pressed: true, ..
                            } = e
                            {
                                Some(*key)
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                }));
            },
            Keys {
                console: Console::new(1, "keys".into(), "SELECT (1;".into(), None),
                commands: Vec::new(),
                remaining: Vec::new(),
            },
        );
        for (key, mods) in [
            (Key::F2, Modifiers::NONE),
            (Key::PageUp, Modifiers::COMMAND),
            (Key::PageDown, Modifiers::COMMAND),
            (
                Key::ArrowRight,
                if cfg!(target_os = "macos") {
                    Modifiers::ALT
                } else {
                    Modifiers::CTRL
                },
            ),
        ] {
            h.key_press_modifiers(mods, key);
            h.run_steps(2);
            assert!(h.state().commands.is_empty());
            assert!(h.state().remaining.contains(&key));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn option_character_events_type_without_firing_defaults_or_saved_bindings() {
        for (key, mods, character, command) in [
            (Key::Num7, Modifiers::ALT, "÷", Command::Structure),
            (Key::Q, Modifiers::ALT, "œ", Command::Context),
            (
                Key::B,
                Modifiers::ALT | Modifiers::SHIFT,
                "ı",
                Command::Explorer,
            ),
        ] {
            let mut h = harness("SELECT 1;", false);
            let ctx = h.ctx.clone();
            h.state_mut().console.navigate_to(&ctx, 9, None);
            h.state_mut()
                .console
                .navigation
                .saved
                .bindings
                .push(Binding {
                    command,
                    shortcut: egui::KeyboardShortcut::new(mods, key),
                });
            h.run_steps(2);
            h.input_mut().events.extend([
                Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: mods,
                },
                Event::Text(character.into()),
            ]);
            h.run_steps(2);
            assert_eq!(h.state().console.text, format!("SELECT 1;{character}"));
            assert!(!h.state().console.navigation.is_open());
            assert!(h.state().ran.is_empty());
        }
    }

    #[test]
    fn invalid_recording_and_persisted_plain_keys_do_not_capture_typing() {
        let mut h = harness("SELECT 1;", false);
        h.state_mut()
            .console
            .navigation
            .pending
            .push(Command::Settings);
        h.run_steps(2);
        h.state_mut().console.navigation.recording = Some(Command::GoToLine);
        h.key_press(Key::J);
        h.run_steps(2);
        assert!(h.state().console.navigation.saved.bindings.is_empty());
        assert_eq!(
            h.state().console.navigation.recording,
            Some(Command::GoToLine)
        );
        h.key_press(Key::Escape);
        h.run_steps(2);
        assert!(h.state().console.navigation.recording.is_none());
        h.state_mut().console.navigation.popup = None;
        h.state_mut()
            .console
            .navigation
            .saved
            .bindings
            .push(Binding {
                command: Command::Structure,
                shortcut: egui::KeyboardShortcut::new(Modifiers::NONE, Key::J),
            });
        let ctx = h.ctx.clone();
        h.state_mut().console.navigate_to(&ctx, 9, None);
        h.run_steps(2);
        h.input_mut().events.extend([
            Event::Key {
                key: Key::J,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
            Event::Text("j".into()),
        ]);
        h.run_steps(2);
        assert_eq!(h.state().console.text, "SELECT 1;j");
        assert!(!h.state().console.navigation.is_open());
    }

    #[test]
    fn text_analysis_and_structure_filter_are_cached_across_idle_frames() {
        let script: String = (0..1000).map(|i| format!("SELECT {i};\n")).collect();
        let mut h = harness(&script, false);
        h.run_steps(3);
        let initial = h.state().console.navigation.index_builds;
        let scopes = h.state().console.navigation.scope_builds;
        let outlines = h.state().console.navigation.outline_builds;
        h.run_steps(5);
        assert_eq!(h.state().console.navigation.index_builds, initial);
        assert_eq!(h.state().console.navigation.scope_builds, scopes);
        assert_eq!(h.state().console.navigation.outline_builds, outlines);
        let ctx = h.ctx.clone();
        h.state_mut().console.navigate_to(&ctx, 12, None);
        h.run_steps(2);
        assert_eq!(h.state().console.navigation.index_builds, initial);
        assert!(h.state().console.navigation.scope_builds > scopes);
        h.state_mut().console.open_structure(Driver::Postgres);
        h.run_steps(3);
        let filters = h.state().console.navigation.filter_builds;
        h.run_steps(5);
        assert_eq!(h.state().console.navigation.index_builds, initial);
        assert_eq!(h.state().console.navigation.filter_builds, filters);
        h.state_mut().console.navigation.input = "SELECT 999".into();
        h.run_steps(3);
        assert_eq!(h.state().console.navigation.filter_builds, filters + 1);
        assert_eq!(h.state().console.navigation.visible.as_ref(), &vec![999]);
    }

    #[test]
    fn a_dialect_change_rebuilds_the_index_without_rebasing_text() {
        let text = "CREATE TRIGGER t AFTER INSERT ON a BEGIN UPDATE a SET x=1; END; SELECT 2;";
        let mut state = State::new(text);
        let revision = state.revision();
        state.sync_revision(text, Driver::Postgres, revision);
        assert_eq!(state.index().statements.len(), 3);
        let builds = state.index_builds;
        state.sync_revision(text, Driver::Sqlite, revision);
        assert_eq!(state.index().statements.len(), 2);
        assert_eq!(state.index_builds, builds + 1);
        state.sync_revision(text, Driver::Sqlite, revision);
        assert_eq!(state.index_builds, builds + 1);
    }

    struct Fixture {
        console: Console,
        ran: Vec<Vec<String>>,
        dark: bool,
    }
    fn harness(text: &str, execution: bool) -> egui_kittest::Harness<'static, Fixture> {
        let mut console = Console::new(1, "Navigation test".into(), text.into(), None);
        console.cursor = Some((0, 0));
        if execution {
            console.open_structure(Driver::Postgres);
        }
        let mut first = true;
        let mut h = egui_kittest::Harness::builder()
            .with_size(vec2(1000.0, 720.0))
            .build_ui_state(
                move |ui, f: &mut Fixture| {
                    if std::mem::take(&mut first) {
                        theme::install_fonts(ui.ctx());
                        theme::apply(ui.ctx(), true);
                        return;
                    }
                    let cx = ConsoleCtx {
                        connections: &[],
                        schemas: None,
                        driver: Driver::Postgres,
                        dark: f.dark,
                        font_size: 14.0,
                        columns: None,
                        auto_complete: false,
                    };
                    let actions = if f.console.routine.is_some() {
                        f.console.show_routine(ui, &cx)
                    } else {
                        f.console.show_console(ui, &cx)
                    };
                    for action in actions {
                        if let ConsoleAction::RunStatements(s) = action {
                            f.ran.push(s);
                        }
                    }
                },
                Fixture {
                    console,
                    ran: Vec::new(),
                    dark: true,
                },
            );
        h.run_steps(3);
        h
    }

    #[test]
    fn structure_arrows_preview_and_execute_only_the_selected_statement_without_editing_sql() {
        let text = "SELECT 'سلام';\n\nSELECT 2;\nSELECT 3;";
        let mut h = harness(text, true);
        h.key_press(Key::ArrowDown);
        h.run_steps(2);
        let console = &h.state().console;
        let r =
            console
                .navigation
                .execution_ranges(&console.text, console.cursor, Driver::Postgres);
        assert_eq!(&text[r[0].clone()], "SELECT 2");
        h.get_by_label("Execute selected statements").click();
        h.run_steps(2);
        assert_eq!(h.state().ran, vec![vec!["SELECT 2".to_string()]]);
        assert_eq!(h.state().console.text, text);
        assert!(!h.state().console.navigation.is_open());
    }

    #[test]
    fn structure_cancel_and_source_changes_never_execute() {
        let mut h = harness("SELECT 1; SELECT 2;", true);
        h.key_press(Key::Escape);
        h.run_steps(2);
        assert!(h.state().ran.is_empty());
        assert!(!h.state().console.navigation.is_open());
        h.state_mut().console.open_structure(Driver::Postgres);
        h.state_mut().console.conn = Some(99);
        h.run_steps(2);
        assert!(!h.state().console.navigation.is_open());
        assert!(h.state().ran.is_empty());
    }

    #[test]
    fn gutter_button_executes_its_own_statement() {
        let mut h = harness("SELECT 1;\n\nSELECT 'سلام';", false);
        h.get_by_label("Execute statement 2").click();
        h.run_steps(2);
        assert_eq!(h.state().ran, vec![vec!["SELECT 'سلام'".to_string()]]);
    }

    #[test]
    fn execution_duration_is_visible_beside_sql_and_disappears_when_that_sql_changes() {
        let mut h = harness("SELECT 1;\nSELECT 'سلام';", false);
        let console = &mut h.state_mut().console;
        let statements = console.prepare_run(Driver::Postgres, false);
        console.execution_started(&statements);
        console.execution_finished(
            &[],
            &[crate::worker::StatementExecution {
                statement: statements[0].clone(),
                results: 0..0,
                elapsed: std::time::Duration::from_millis(12),
                failed: false,
            }],
        );
        h.run_steps(2);
        h.get_by_label("12 ms");
        assert_eq!(h.state().console.text, "SELECT 1;\nSELECT 'سلام';");
        h.state_mut().console.text.insert_str(0, "-- سلام\n");
        h.run_steps(2);
        h.get_by_label("12 ms");
        h.state_mut().console.text = "-- سلام\nSELECT 99;\nSELECT 'سلام';".into();
        h.run_steps(2);
        assert!(h.query_by_label("12 ms").is_none());
    }

    #[test]
    fn long_script_only_builds_execution_time_labels_for_visible_lines() {
        fn labels(shape: &egui::Shape) -> usize {
            match shape {
                egui::Shape::Vec(shapes) => shapes.iter().map(labels).sum(),
                egui::Shape::Text(t) => usize::from(t.galley.text() == "12 ms"),
                _ => 0,
            }
        }
        let mut h = harness(&"SELECT 1;\n".repeat(1000), false);
        let c = &mut h.state_mut().console;
        let statements = c.prepare_run(Driver::Postgres, true);
        c.execution_started(&statements);
        let executions: Vec<_> = statements
            .into_iter()
            .map(|statement| crate::worker::StatementExecution {
                statement,
                results: 0..0,
                elapsed: std::time::Duration::from_millis(12),
                failed: false,
            })
            .collect();
        c.execution_finished(&[], &executions);
        h.run_steps(2);
        let count: usize = h.output().shapes.iter().map(|s| labels(&s.shape)).sum();
        assert!(
            count > 0 && count < 64,
            "only viewport timing labels should be built, got {count}"
        );
        let ctx = h.ctx.clone();
        let end = h.state().console.text.chars().count() - 1;
        h.state_mut().console.navigate_to(&ctx, end, None);
        h.run_steps(4);
        let count: usize = h.output().shapes.iter().map(|s| labels(&s.shape)).sum();
        assert!(
            count > 0 && count < 64,
            "timing labels must remain visible after scrolling"
        );
    }

    #[test]
    fn long_script_only_builds_gutter_controls_for_visible_lines() {
        fn run_icons(shape: &egui::Shape) -> usize {
            match shape {
                egui::Shape::Vec(shapes) => shapes.iter().map(run_icons).sum(),
                egui::Shape::Text(t) => usize::from(t.galley.text() == "▶"),
                _ => 0,
            }
        }
        let mut h = harness(&"SELECT 1;\n".repeat(1000), false);
        let icons: usize = h.output().shapes.iter().map(|s| run_icons(&s.shape)).sum();
        assert!(
            icons > 0 && icons < 64,
            "only viewport gutter controls should be built, got {icons}"
        );
        let ctx = h.ctx.clone();
        let end = h.state().console.text.chars().count() - 1;
        h.state_mut().console.navigate_to(&ctx, end, None);
        h.run_steps(4);
        h.get_by_label("Execute statement 1000").click();
        h.run_steps(2);
        assert_eq!(h.state().ran, vec![vec!["SELECT 1".to_string()]]);
    }

    #[test]
    fn backspace_before_a_trailing_newline_rebuilds_gutter_offsets() {
        let text = format!("{}  \n", "SELECT 1;".repeat(6));
        assert_eq!(text.len(), 57);
        let mut h = harness(&text, false);
        let ctx = h.ctx.clone();
        h.state_mut().console.navigate_to(&ctx, 56, None);
        h.run_steps(2);
        h.key_press(Key::Backspace);
        h.run_steps(2);
        assert_eq!(h.state().console.text.len(), 56);
        assert!(h.state().console.text.ends_with('\n'));
        assert!(h.state().ran.is_empty());
    }

    #[test]
    fn execution_scope_preserves_syntax_colors_and_has_an_unfilled_outline() {
        fn visit<'a>(
            shape: &'a egui::Shape,
            text: &str,
            editors: &mut Vec<&'a egui::Galley>,
            paths: &mut Vec<&'a egui::epaint::PathShape>,
        ) {
            match shape {
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        visit(shape, text, editors, paths);
                    }
                }
                egui::Shape::Text(t) if t.galley.text() == text => editors.push(&t.galley),
                egui::Shape::Path(path) => paths.push(path),
                _ => {}
            }
        }
        let text = "SELECT id,\n       email\nFROM customers;\n\nSELECT 2;";
        let mut h = harness(text, false);
        for dark in [true, false] {
            h.state_mut().dark = dark;
            theme::apply(&h.ctx, dark);
            h.run_steps(2);
            let mut editors = Vec::new();
            let mut paths = Vec::new();
            for clipped in &h.output().shapes {
                visit(&clipped.shape, text, &mut editors, &mut paths);
            }
            assert!(!editors.is_empty());
            for galley in editors {
                assert!(
                    galley
                        .job
                        .sections
                        .iter()
                        .all(|s| s.format.background == egui::Color32::TRANSPARENT),
                    "Execution scope must not fill the syntax-highlighted text background"
                );
                let plain =
                    sql::highlight_for(text, Driver::Postgres, dark, FontId::monospace(14.0));
                assert_eq!(
                    galley.job.sections, plain.sections,
                    "Execution scope must preserve the syntax colors"
                );
            }
            let border = if dark {
                egui::Color32::from_rgb(61, 122, 73)
            } else {
                egui::Color32::from_rgb(186, 168, 255)
            };
            assert!(
                paths.iter().any(|path| path.closed
                    && path.fill == egui::Color32::TRANSPARENT
                    && path.stroke.width == 1.0
                    && matches!(path.stroke.color,egui::epaint::ColorMode::Solid(c) if c==border)),
                "The current statement must have DataGrip's unfilled boxed outline"
            );
        }
    }

    #[test]
    fn unicode_insertions_do_not_reuse_offsets_inside_a_multibyte_character() {
        let mut h = harness("a\né\n", false);
        let ctx = h.ctx.clone();
        h.state_mut().console.navigate_to(&ctx, 0, None);
        h.run_steps(2);
        h.input_mut().events.push(Event::Text("é".into()));
        h.run_steps(1);
        assert_eq!(h.state().console.text, "éa\né\n");
        assert!(h.state().ran.is_empty());
    }

    #[test]
    fn gutter_statement_targets_update_in_the_same_frame_as_paste() {
        let mut h = harness("SELECT 2;", false);
        let ctx = h.ctx.clone();
        h.state_mut().console.navigate_to(&ctx, 0, None);
        h.run_steps(2);
        h.input_mut()
            .events
            .push(Event::Paste("SELECT 'سلام';\n\n".into()));
        h.run_steps(1);
        assert_eq!(h.state().console.text, "SELECT 'سلام';\n\nSELECT 2;");
        h.get_by_label("Execute statement 2").click();
        h.run_steps(2);
        assert_eq!(h.state().ran, vec![vec!["SELECT 2".to_owned()]]);
    }

    #[test]
    fn mixed_unicode_edits_undo_and_redo_keep_source_ranges_valid() {
        let mut h = harness("SELECT 'سلام';\r\nSELECT 'é';\n", false);
        let ctx = h.ctx.clone();
        let mut seed = 0x5eed_u32;
        for step in 0..80 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let len = h.state().console.text.chars().count();
            h.state_mut()
                .console
                .navigate_to(&ctx, (seed as usize) % (len + 1), None);
            h.run_steps(1);
            match step % 6 {
                0 => h.input_mut().events.push(Event::Text("é".into())),
                1 => h.key_press(Key::Backspace),
                2 => h
                    .input_mut()
                    .events
                    .push(Event::Paste("\nSELECT 'سلام';\n".into())),
                3 => h.key_press(Key::Delete),
                4 => h.key_press_modifiers(Modifiers::COMMAND, Key::Z),
                _ => h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z),
            }
            h.run_steps(1);
            let c = &h.state().console;
            for range in c
                .navigation
                .execution_ranges(&c.text, c.cursor, Driver::Postgres)
            {
                assert!(
                    c.text.get(range).is_some(),
                    "stale source range after edit {step}"
                );
            }
            assert!(h.state().ran.is_empty());
        }
    }

    #[test]
    fn multiple_statement_scopes_have_separate_boxes_and_skip_unselected_sql() {
        fn collect<'a>(shape: &'a egui::Shape, out: &mut Vec<&'a egui::epaint::PathShape>) {
            match shape {
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        collect(shape, out);
                    }
                }
                egui::Shape::Path(path) if matches!(path.stroke.color,egui::epaint::ColorMode::Solid(c) if c==egui::Color32::from_rgb(61,122,73)) => {
                    out.push(path)
                }
                _ => {}
            }
        }
        let mut h = harness("SELECT 1; SELECT 2; SELECT 3;", true);
        h.state_mut().console.navigation.selected = vec![0, 2];
        h.run_steps(2);
        let mut paths = Vec::new();
        for clipped in &h.output().shapes {
            collect(&clipped.shape, &mut paths);
        }
        assert_eq!(paths.len(), 2);
        assert!(
            paths
                .iter()
                .all(|p| p.fill == egui::Color32::TRANSPARENT && p.closed)
        );
        let a = Rect::from_points(&paths[0].points);
        let b = Rect::from_points(&paths[1].points);
        assert!(
            a.right() < b.left(),
            "The box must not include the unselected middle statement"
        );
    }

    #[test]
    fn navigation_from_a_routine_preview_restores_the_definition_before_focusing_it() {
        use crate::db::routines::{RoutineKind, RoutineRef};
        use crate::ui::routine_dialog::{RoutineDialog, RoutineEditor, RoutinePage};
        let mut h = harness("CREATE PROCEDURE p AS SELECT 1;", false);
        h.state_mut().console.routine = Some(RoutineEditor {
            comment: Default::default(),
            reference: RoutineRef {
                conn: 7,
                db: "shop".into(),
                schema: "public".into(),
                name: "p".into(),
                kind: RoutineKind::Procedure,
                signature: String::new(),
            },
            dialog: RoutineDialog::new_routine(
                7,
                Driver::Postgres,
                "shop".into(),
                "public".into(),
                RoutineKind::Procedure,
            ),
            saved: String::new(),
            submitted: None,
            transaction: false,
            page: RoutinePage::Preview,
            word_wrap: false,
            outline: false,
            snippets: false,
        });
        h.state_mut()
            .console
            .navigation
            .pending
            .push(Command::GoToLine);
        h.run_steps(3);
        assert!(matches!(
            h.state().console.routine.as_ref().unwrap().page,
            RoutinePage::Definition
        ));
        assert!(h.state().console.navigation.is_open());
    }

    #[test]
    fn assigned_shortcut_persists_and_selects_a_paragraph_without_inserting_text() {
        let mut h = harness("one\n\ntwo\n\nthree", false);
        let ctx = h.ctx.clone();
        h.state_mut()
            .console
            .navigation
            .saved
            .bindings
            .push(Binding {
                command: Command::ParagraphForwardSelect,
                shortcut: egui::KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::ALT, Key::J),
            });
        h.state_mut().console.navigate_to(&ctx, 0, None);
        h.run_steps(2);
        h.key_press_modifiers(Modifiers::COMMAND | Modifiers::ALT, Key::J);
        h.run_steps(2);
        assert_eq!(h.state().console.selection(), Some("one\n\n"));
        assert_eq!(h.state().console.text, "one\n\ntwo\n\nthree");
        let saved: Saved = serde_json::from_str(
            &serde_json::to_string(&h.state().console.navigation.saved).unwrap(),
        )
        .unwrap();
        assert_eq!(saved.bindings[0].command, Command::ParagraphForwardSelect);
    }

    #[test]
    fn shortcut_recording_consumes_keys_and_escape_cancels_only_recording() {
        let mut h = harness("SELECT 1;", false);
        h.state_mut()
            .console
            .navigation
            .pending
            .push(Command::Settings);
        h.run_steps(2);
        h.state_mut().console.navigation.recording = Some(Command::ParagraphForward);
        h.key_press_modifiers(Modifiers::COMMAND | Modifiers::ALT, Key::J);
        h.run_steps(2);
        assert_eq!(
            h.state().console.navigation.saved.bindings[0].shortcut,
            egui::KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::ALT, Key::J)
        );
        h.state_mut().console.navigation.recording = Some(Command::ParagraphBackward);
        h.key_press(Key::Escape);
        h.run_steps(2);
        assert!(h.state().console.navigation.recording.is_none());
        assert!(h.state().console.navigation.is_open());
        assert_eq!(h.state().console.text, "SELECT 1;");
    }

    #[test]
    fn word_navigation_stops_at_line_breaks_and_word_search_skips_substrings() {
        let mut c = Console::new(1, "test".into(), "id order_id id\nnext".into(), None);
        let ctx = egui::Context::default();
        c.navigate_to(&ctx, 0, None);
        c.navigation_command(&ctx, Driver::Postgres, Command::FindWord, &mut Vec::new());
        assert_eq!(c.cursor, Some((12, 14)));
        c.navigate_to(&ctx, 14, None);
        c.navigation_command(
            &ctx,
            Driver::Postgres,
            Command::WordForward,
            &mut Vec::new(),
        );
        assert_eq!(c.cursor, Some((15, 15)));
        c.navigation_command(
            &ctx,
            Driver::Postgres,
            Command::WordBackward,
            &mut Vec::new(),
        );
        assert_eq!(c.cursor, Some((14, 14)));
    }

    #[test]
    fn structure_filter_enter_navigates_to_the_visible_statement() {
        let mut h = harness("SELECT 1;\nSELECT 99;", false);
        h.state_mut()
            .console
            .navigation
            .pending
            .push(Command::Structure);
        h.run_steps(3);
        h.input_mut().events.push(Event::Text("99".into()));
        h.run_steps(2);
        h.key_press(Key::Enter);
        h.run_steps(2);
        assert_eq!(h.state().console.cursor, Some((10, 10)));
        assert!(h.state().ran.is_empty());
    }

    #[test]
    fn multiple_statements_execute_in_source_order_and_run_all_includes_every_statement() {
        let mut h = harness("SELECT 1; SELECT 2; SELECT 3;", true);
        h.state_mut().console.navigation.selected = vec![2, 0, 2];
        h.run_steps(2);
        assert_eq!(
            h.state().console.navigation.selected,
            vec![2, 0, 2],
            "rendering must preserve multi-selection"
        );
        h.get_by_label("Execute selected statements").click();
        h.run_steps(2);
        assert_eq!(h.state().ran[0], vec!["SELECT 1", "SELECT 3"]);
        h.state_mut().console.open_structure(Driver::Postgres);
        h.run_steps(2);
        h.get_by_label("Run all statements").click();
        h.run_steps(2);
        assert_eq!(h.state().ran[1], vec!["SELECT 1", "SELECT 2", "SELECT 3"]);
    }

    #[test]
    fn bookmark_follows_insertions_and_persists_with_editor_settings() {
        let mut h = harness("SELECT 1;\nSELECT 2;", false);
        h.state_mut()
            .console
            .navigation
            .saved
            .bookmarks
            .push(Bookmark {
                position: 10,
                mnemonic: "2".into(),
                description: "Second query".into(),
            });
        h.state_mut().console.text.insert_str(0, "-- سلام\n");
        h.run_steps(2);
        let saved = &h.state().console.navigation.saved;
        assert_eq!(saved.bookmarks[0].position, 18);
        let restored: Saved = serde_json::from_str(&serde_json::to_string(saved).unwrap()).unwrap();
        assert_eq!(restored.bookmarks[0].description, "Second query");
        assert_eq!(restored.bookmarks[0].mnemonic, "2");
        assert!(restored.line_numbers);
        assert!(serde_json::from_str::<Saved>("{}").unwrap().line_numbers);
    }

    #[test]
    fn paragraph_selection_can_extend_backwards_repeatedly() {
        let mut c = Console::new(1, "test".into(), "one\n\ntwo\n\nthree".into(), None);
        let ctx = egui::Context::default();
        c.navigate_to(&ctx, 15, None);
        c.navigation_command(
            &ctx,
            Driver::Postgres,
            Command::ParagraphBackwardSelect,
            &mut Vec::new(),
        );
        assert_eq!(c.selection(), Some("three"));
        c.navigation_command(
            &ctx,
            Driver::Postgres,
            Command::ParagraphBackwardSelect,
            &mut Vec::new(),
        );
        assert_eq!(c.selection(), Some("two\n\nthree"));
    }

    #[test]
    fn go_to_line_rejects_invalid_numbers_and_uses_unicode_columns() {
        let mut h = harness("SELECT 'سلام';\nSELECT 2;", false);
        h.state_mut()
            .console
            .navigation
            .pending
            .push(Command::GoToLine);
        h.run_steps(3);
        h.input_mut().events.push(Event::Text("2:8".into()));
        h.run_steps(2);
        h.key_press(Key::Enter);
        h.run_steps(2);
        assert_eq!(h.state().console.line_col(), (2, 8));
        assert_eq!(h.state().console.text, "SELECT 'سلام';\nSELECT 2;");
    }

    #[test]
    fn next_issue_wraps_without_flagging_whitespace() {
        let mut c = Console::new(1, "test".into(), "SELECT 1;  \nSELECT (2;".into(), None);
        let ctx = egui::Context::default();
        c.navigate_to(&ctx, 0, None);
        c.navigation.saved.highest_priority = true;
        c.navigation_command(&ctx, Driver::Postgres, Command::NextIssue, &mut Vec::new());
        assert_eq!(c.cursor, Some((19, 19)));
        c.navigation_command(&ctx, Driver::Postgres, Command::NextIssue, &mut Vec::new());
        assert_eq!(c.cursor, Some((19, 19)));
    }

    #[test]
    fn sqlite_trigger_is_one_executable_statement() {
        let text = "CREATE TRIGGER audit AFTER INSERT ON notes BEGIN\n INSERT INTO log VALUES ('a;');\n UPDATE log SET n = 2;\nEND;\nSELECT 42;";
        let ss = sql::split_statements(text, Driver::Sqlite);
        assert_eq!(ss.len(), 2);
        assert!(ss[0].text.ends_with("END"));
        assert_eq!(ss[1].text, "SELECT 42");
    }
}
