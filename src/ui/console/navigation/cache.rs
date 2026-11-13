//! Text-derived editor data. Rebuilt for SQL changes, never for idle UI frames.

use super::*;
use std::collections::HashMap;

pub(crate) struct StructureEntry {
    pub label: String,
    pub search: String,
}

pub(crate) struct TextIndex {
    pub revision: u64,
    pub driver: Driver,
    pub characters: Vec<usize>,
    pub line_bytes: Vec<usize>,
    pub line_chars: Vec<usize>,
    pub statements: Vec<sql::Statement>,
    pub statement_chars: Vec<Range<usize>>,
    pub statement_lines: HashMap<usize, usize>,
    pub issues: Vec<source::Issue>,
    pub problem_lines: HashMap<usize, usize>,
    pub structure: Vec<StructureEntry>,
}

impl TextIndex {
    pub fn new(text: &str, driver: Driver, revision: u64) -> Self {
        let mut characters = Vec::new();
        let mut line_bytes = vec![0];
        let mut line_chars = vec![0];
        for (character, (byte, ch)) in text.char_indices().enumerate() {
            characters.push(byte);
            if ch == '\n' {
                line_bytes.push(byte + 1);
                line_chars.push(character + 1);
            }
        }
        characters.push(text.len());
        let statements = sql::split_statements(text, driver);
        let statement_chars: Vec<_> = statements
            .iter()
            .map(|s| {
                characters.partition_point(|&b| b < s.start)
                    ..characters.partition_point(|&b| b < s.end)
            })
            .collect();
        let mut statement_lines = HashMap::new();
        let structure = statements
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let line = line_bytes.partition_point(|&b| b <= s.start);
                statement_lines.entry(line).or_insert(i);
                let summary: String = s
                    .text
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(180)
                    .collect();
                StructureEntry {
                    label: format!("{line}:  {summary}"),
                    search: s.text.to_lowercase(),
                }
            })
            .collect();
        let issues = source::issues(text, driver);
        let problem_lines = issues
            .iter()
            .enumerate()
            .rev()
            .map(|(i, p)| (line_chars.partition_point(|&s| s <= p.position), i))
            .collect();
        Self {
            revision,
            driver,
            characters,
            line_bytes,
            line_chars,
            statements,
            statement_chars,
            statement_lines,
            issues,
            problem_lines,
            structure,
        }
    }

    pub fn byte(&self, character: usize) -> usize {
        self.characters
            .get(character)
            .copied()
            .unwrap_or_else(|| *self.characters.last().unwrap())
    }

    pub fn character(&self, byte: usize) -> usize {
        self.characters.partition_point(|&b| b < byte)
    }

    pub fn line_column(&self, character: usize) -> (usize, usize) {
        let line = self.line_chars.partition_point(|&c| c <= character).max(1);
        (
            line,
            character.saturating_sub(self.line_chars[line - 1]) + 1,
        )
    }

    pub fn statement_at(&self, text: &str, caret: usize) -> Option<usize> {
        if self.statements.is_empty() {
            return None;
        }
        let caret = self.byte(caret);
        let next = self.statements.partition_point(|s| s.start <= caret);
        if let Some(statement) = self.statements.get(next) {
            let line = self
                .line_bytes
                .partition_point(|&b| b <= caret)
                .saturating_sub(1);
            if next == 0
                || (text[self.line_bytes[line]..caret].trim().is_empty()
                    && !text[caret..statement.start].contains('\n')
                    && text[caret..statement.start].trim().is_empty())
            {
                return Some(next);
            }
        }
        Some(next - 1)
    }
}

pub(super) struct OutlineCache {
    galley: Arc<egui::Galley>,
    epoch: u64,
    color: egui::Color32,
    shapes: Vec<(Rect, egui::Shape)>,
}

impl State {
    pub fn revision(&self) -> u64 {
        self.revision.unwrap_or(0)
    }

    pub fn new(text: &str) -> Self {
        let mut state = Self::default();
        state.sync_text(text);
        state
    }

    pub(in crate::ui::console) fn ensure_index(&mut self, text: &str, driver: Driver) {
        let revision = self.revision();
        if !self
            .index
            .as_ref()
            .is_some_and(|i| i.revision == revision && i.driver == driver)
        {
            self.index = Some(Arc::new(TextIndex::new(text, driver, revision)));
            self.scope_key = None;
            self.filter_key = None;
            #[cfg(test)]
            {
                self.index_builds += 1;
            }
        }
    }

    pub(in crate::ui::console) fn sync_revision(
        &mut self,
        text: &str,
        driver: Driver,
        revision: u64,
    ) {
        self.sync_changed_text(text, revision);
        self.ensure_index(text, driver);
    }

    pub(in crate::ui::console) fn index(&self) -> Arc<TextIndex> {
        Arc::clone(
            self.index
                .as_ref()
                .expect("editor text index synchronized before drawing"),
        )
    }

    pub(in crate::ui::console) fn update_scope(
        &mut self,
        text: &str,
        cursor: Option<(usize, usize)>,
        driver: Driver,
    ) {
        self.ensure_index(text, driver);
        let structure = self.popup == Some(Popup::Structure);
        if self.scope_key.as_ref().is_some_and(|(c, s, selected)| {
            *c == cursor && *s == structure && (!structure || selected == &self.selected)
        }) {
            return;
        }
        let index = self.index();
        self.scopes = if structure {
            self.selected
                .iter()
                .filter_map(|&i| index.statements.get(i).map(|s| s.start..s.end))
                .collect()
        } else if let Some((a, b)) = cursor.filter(|(a, b)| a != b)
            && !text[index.byte(a)..index.byte(b)].trim().is_empty()
        {
            vec![index.byte(a)..index.byte(b)]
        } else {
            index
                .statement_at(text, cursor.map_or(index.characters.len() - 1, |(_, b)| b))
                .map(|i| vec![index.statements[i].start..index.statements[i].end])
                .unwrap_or_default()
        };
        self.scope_chars = self
            .scopes
            .iter()
            .map(|r| index.character(r.start)..index.character(r.end))
            .collect();
        self.scope_key = Some((
            cursor,
            structure,
            if structure {
                self.selected.clone()
            } else {
                Vec::new()
            },
        ));
        self.scope_epoch = self.scope_epoch.wrapping_add(1);
        #[cfg(test)]
        {
            self.scope_builds += 1;
        }
    }

    pub fn prepare_metadata(&mut self, text: &str, driver: Driver) {
        self.ensure_index(text, driver);
    }

    pub fn cached_line_column(&self, character: usize) -> (usize, usize) {
        self.index
            .as_ref()
            .expect("metadata index synchronized before use")
            .line_column(character)
    }

    pub(in crate::ui::console) fn line_count(&self) -> Option<usize> {
        self.index.as_ref().map(|i| i.line_chars.len())
    }

    pub(in crate::ui::console) fn bookmark_lines(
        &mut self,
    ) -> Arc<std::collections::HashSet<usize>> {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        for bookmark in &self.saved.bookmarks {
            bookmark.position.hash(&mut hash);
        }
        let key = (self.revision(), hash.finish());
        if self.bookmark_key != Some(key) {
            let index = self.index();
            self.bookmark_lines = Arc::new(
                self.saved
                    .bookmarks
                    .iter()
                    .map(|b| index.line_column(b.position).0)
                    .collect(),
            );
            self.bookmark_key = Some(key);
        }
        Arc::clone(&self.bookmark_lines)
    }

    pub(in crate::ui::console) fn queue_gutter(
        &mut self,
        index: usize,
        conn: Option<ConnId>,
        db: &Option<String>,
        driver: Driver,
    ) {
        if let Some(statement) = self.index().statements.get(index) {
            self.gutter_run = Some(GutterRun {
                revision: self.revision(),
                source: (conn, db.clone(), driver),
                range: statement.start..statement.end,
            });
        }
    }

    pub(in crate::ui::console) fn filtered_statements(&mut self) -> Arc<Vec<usize>> {
        let index = self.index();
        let key = (index.revision, index.driver, self.input.clone());
        if self.filter_key.as_ref() != Some(&key) {
            let filter = self.input.to_lowercase();
            self.visible = Arc::new(
                index
                    .structure
                    .iter()
                    .enumerate()
                    .filter_map(|(i, s)| s.search.contains(&filter).then_some(i))
                    .collect(),
            );
            self.filter_key = Some(key);
            #[cfg(test)]
            {
                self.filter_builds += 1;
            }
        }
        Arc::clone(&self.visible)
    }

    pub(in crate::ui::console) fn outlines(
        &mut self,
        galley: &Arc<egui::Galley>,
        origin: egui::Pos2,
        color: egui::Color32,
        clip: Rect,
    ) -> Vec<egui::Shape> {
        if !self.outline_cache.as_ref().is_some_and(|c| {
            Arc::ptr_eq(&c.galley, galley) && c.epoch == self.scope_epoch && c.color == color
        }) {
            let mut shapes: Vec<_> =
                super::outline_chars(galley, &self.scope_chars, egui::Pos2::ZERO, color)
                    .into_iter()
                    .map(|shape| (shape.visual_bounding_rect(), shape))
                    .collect();
            shapes.sort_by(|a, b| {
                a.0.top()
                    .total_cmp(&b.0.top())
                    .then(a.0.bottom().total_cmp(&b.0.bottom()))
            });
            self.outline_cache = Some(OutlineCache {
                galley: Arc::clone(galley),
                epoch: self.scope_epoch,
                color,
                shapes,
            });
            #[cfg(test)]
            {
                self.outline_builds += 1;
            }
        }
        let viewport = clip.translate(-origin.to_vec2());
        let shapes = &self.outline_cache.as_ref().unwrap().shapes;
        // Statement scopes are disjoint and ordered in the editor. Their
        // lower edges are ordered too, so skip offscreen annotations in O(log n).
        let first = shapes.partition_point(|(bounds, _)| bounds.bottom() < viewport.top());
        shapes[first..]
            .iter()
            .take_while(|(bounds, _)| bounds.top() <= viewport.bottom())
            .filter(|(bounds, _)| bounds.intersects(viewport))
            .map(|(_, shape)| {
                let mut shape = shape.clone();
                shape.translate(origin.to_vec2());
                shape
            })
            .collect()
    }
}
