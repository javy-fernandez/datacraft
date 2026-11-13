//! Execution provenance and non-editable durations beside the SQL that produced them.

use super::*;
use crate::db::ResultSet;
use crate::worker::StatementExecution;
use std::ops::Range;
use std::time::Duration;

type Source = (Option<ConnId>, Option<String>);

#[derive(Clone)]
struct Unit {
    sql: String,
    range: Range<usize>,
    members: Vec<(String, Range<usize>)>,
}

struct Plan {
    text: String,
    source: Source,
    driver: Driver,
    units: Vec<Unit>,
    statement_ranges: std::collections::HashSet<Range<usize>>,
}

#[derive(Clone, Debug)]
struct Timing {
    /// Character offsets, like the editor cursor. A changed query loses its duration.
    range: Range<usize>,
    elapsed: Duration,
    /// Partial selections retain their own scope when the surrounding SQL changes.
    complete_statement: bool,
}

#[derive(Default)]
pub(super) struct State {
    prepared: Option<Plan>,
    in_flight: Option<Plan>,
    previous: String,
    source: Option<Source>,
    driver: Option<Driver>,
    timings: Vec<Timing>,
    revision: Option<u64>,
    line_starts: Vec<usize>,
    #[cfg(test)]
    text_syncs: usize,
}

/// Revalidate moved annotations only after an edit. An edit outside a range can
/// still change its statement, for example by prepending EXPLAIN or deleting `;`.
fn timing_ranges(text: &str, driver: Driver) -> std::collections::HashSet<Range<usize>> {
    let characters: Vec<_> = text.char_indices().map(|(byte, _)| byte).collect();
    indexed_timing_ranges(text, driver, &characters)
}

fn indexed_timing_ranges(
    text: &str,
    driver: Driver,
    characters: &[usize],
) -> std::collections::HashSet<Range<usize>> {
    let character_at = |byte| characters.partition_point(|&start| start < byte);
    let mut statements = sql::split_statements(text, driver);
    if matches!(driver, Driver::MsSql | Driver::Sqlite) {
        statements.extend(sql::split_batches(text, driver));
    }
    statements
        .into_iter()
        .map(|s| character_at(s.start)..character_at(s.end))
        .collect()
}

/// One changed span shared by every annotation, in character offsets.
struct Edit {
    prefix: usize,
    end: usize,
    delta: isize,
}

impl Edit {
    fn between(before: &str, after: &str) -> Option<Self> {
        if before == after {
            return None;
        }
        let a: Vec<_> = before.chars().collect();
        let b: Vec<_> = after.chars().collect();
        let prefix = a.iter().zip(&b).take_while(|(a, b)| a == b).count();
        let suffix = a[prefix..]
            .iter()
            .rev()
            .zip(b[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        Some(Self {
            prefix,
            end: a.len() - suffix,
            delta: b.len() as isize - a.len() as isize,
        })
    }

    fn rebase(&self, range: &Range<usize>) -> Option<Range<usize>> {
        // The parser omits a statement's terminator. Insertion exactly at end
        // can therefore change its SQL (SELECT 1 -> SELECT 10).
        if range.end < self.prefix {
            Some(range.clone())
        } else if range.start >= self.end {
            Some(
                range.start.checked_add_signed(self.delta)?
                    ..range.end.checked_add_signed(self.delta)?,
            )
        } else {
            None
        }
    }
}

impl State {
    fn sync_source(&mut self, conn: Option<ConnId>, db: &Option<String>) {
        if !self
            .source
            .as_ref()
            .is_some_and(|(c, d)| *c == conn && d == db)
        {
            self.timings.clear();
            self.source = Some((conn, db.clone()));
        }
    }

    /// Reuses the editor layouter's text hash; idle frames never compare or parse SQL.
    pub(super) fn sync_revision(
        &mut self,
        text: &str,
        conn: Option<ConnId>,
        db: &Option<String>,
        revision: u64,
    ) {
        if self.revision != Some(revision) {
            self.sync(text, conn, db);
            self.revision = Some(revision);
        } else {
            self.sync_source(conn, db);
        }
    }

    pub(super) fn line_at(&self, caret: usize) -> usize {
        self.line_starts
            .partition_point(|&start| start <= caret)
            .max(1)
    }

    pub(super) fn sync(&mut self, text: &str, conn: Option<ConnId>, db: &Option<String>) {
        #[cfg(test)]
        {
            self.text_syncs += 1;
        }
        self.sync_source(conn, db);
        if !self.timings.is_empty()
            && let Some(edit) = Edit::between(&self.previous, text)
        {
            let valid = timing_ranges(text, self.driver.unwrap_or(Driver::Postgres));
            self.timings.retain_mut(|t| {
                if let Some(range) = edit
                    .rebase(&t.range)
                    .filter(|range| !t.complete_statement || valid.contains(range))
                {
                    t.range = range;
                    true
                } else {
                    false
                }
            });
        }
        if self.previous != text {
            self.previous = text.into();
            self.revision = None;
            self.line_starts.clear();
            self.line_starts.push(0);
            self.line_starts.extend(
                text.chars()
                    .enumerate()
                    .filter_map(|(i, ch)| (ch == '\n').then_some(i + 1)),
            );
        }
    }

    fn prepare(
        &mut self,
        text: &str,
        source: Source,
        driver: Driver,
        ranges: Vec<Range<usize>>,
    ) -> Vec<String> {
        let mut ranges = ranges;
        ranges.sort_by_key(|r| r.start);
        ranges.dedup();
        // Build once for the entire script, rather than rescanning its prefix
        // for every unit and every member of a batch.
        let characters: Vec<_> = text.char_indices().map(|(byte, _)| byte).collect();
        let statement_ranges = indexed_timing_ranges(text, driver, &characters);
        let character_at = |byte| characters.partition_point(|&start| start < byte);
        let units: Vec<_> = ranges
            .into_iter()
            .filter_map(|range| text.get(range.clone()).map(|s| (range.start, s)))
            .flat_map(|(offset, fragment)| {
                sql::split_batches(fragment, driver)
                    .into_iter()
                    .map(move |s| (offset, s))
            })
            .map(|(offset, s)| {
                let start = offset + s.start;
                let members = sql::split_statements(&s.text, driver)
                    .into_iter()
                    .map(|m| {
                        let range = character_at(start + m.start)..character_at(start + m.end);
                        (m.text, range)
                    })
                    .collect();
                Unit {
                    sql: s.text,
                    range: character_at(start)..character_at(offset + s.end),
                    members,
                }
            })
            .collect();
        let statements = units.iter().map(|u| u.sql.clone()).collect();
        self.prepared = Some(Plan {
            text: text.into(),
            source,
            driver,
            units,
            statement_ranges,
        });
        statements
    }

    pub(super) fn begin(
        &mut self,
        text: &str,
        conn: Option<ConnId>,
        db: &Option<String>,
        statements: &[String],
    ) {
        self.sync(text, conn, db);
        self.in_flight = self.prepared.take().filter(|p| {
            p.source == (conn, db.clone())
                && p.text == text
                && p.units.iter().map(|u| &u.sql).eq(statements.iter())
        });
        if let Some(plan) = &self.in_flight {
            self.driver = Some(plan.driver);
            self.timings.retain(|t| {
                let next = plan.units.partition_point(|u| u.range.end <= t.range.start);
                !plan
                    .units
                    .get(next)
                    .is_some_and(|u| u.range.start < t.range.end)
            });
        }
    }

    pub(super) fn finish(
        &mut self,
        text: &str,
        conn: Option<ConnId>,
        db: &Option<String>,
        results: &[ResultSet],
        executions: &[StatementExecution],
    ) {
        self.sync(text, conn, db);
        let Some(plan) = self
            .in_flight
            .take()
            .filter(|p| p.source == (conn, db.clone()))
        else {
            return;
        };
        let edit = Edit::between(&plan.text, text);
        let valid = edit.as_ref().map(|_| timing_ranges(text, plan.driver));
        let mut timings = Vec::new();
        for (unit, execution) in plan.units.iter().zip(executions) {
            if unit.sql != execution.statement || execution.failed {
                continue;
            }
            let mut record = |range: &Range<usize>, elapsed| {
                let complete_statement = plan.statement_ranges.contains(range);
                if let Some(range) = edit
                    .as_ref()
                    .map_or_else(|| Some(range.clone()), |e| e.rebase(range))
                    .filter(|range| {
                        !complete_statement
                            || valid.as_ref().is_none_or(|valid| valid.contains(range))
                    })
                {
                    timings.push(Timing {
                        range,
                        elapsed,
                        complete_statement,
                    });
                }
            };
            if unit.members.len() <= 1 {
                record(&unit.range, execution.elapsed);
            } else if plan.driver == Driver::Sqlite {
                // SQLite returns one independently timed result per source statement.
                // Match inside this submitted batch, so identical queries elsewhere can't steal it.
                let Some(rs) = results.get(execution.results.clone()) else {
                    continue;
                };
                let mut next = 0;
                for result in rs {
                    let parsed = sql::split_statements(&result.statement, plan.driver);
                    let Some(actual) = parsed.first().filter(|_| parsed.len() == 1) else {
                        continue;
                    };
                    if let Some(i) = unit.members[next..]
                        .iter()
                        .position(|(s, _)| s == &actual.text)
                    {
                        next += i;
                        record(&unit.members[next].1, result.elapsed);
                        next += 1;
                    }
                }
            } else {
                // SQL Server executes a batch as a unit; its result sets aren't separate executions.
                record(&unit.range, execution.elapsed);
            }
        }
        let replaced: std::collections::HashSet<_> = timings
            .iter()
            .map(|t| (t.range.start, t.range.end))
            .collect();
        self.timings
            .retain(|t| !replaced.contains(&(t.range.start, t.range.end)));
        self.timings.extend(timings);
        self.timings.sort_by_key(|t| t.range.start);
    }

    pub(super) fn show(
        &self,
        ui: &mut Ui,
        galley: &egui::Galley,
        origin: egui::Pos2,
        font_size: f32,
        color: egui::Color32,
    ) {
        if self.timings.is_empty() {
            return;
        }
        let clip = ui.clip_rect();
        let mut character = 0;
        let mut next = None;
        // Both rows and annotations are ordered. Walk rows once instead of searching
        // the entire galley separately for every query, and build only visible labels.
        for row in &galley.rows {
            let start = character;
            let end = start + row.char_count_excluding_newline().0;
            character += row.char_count_including_newline().0;
            if origin.y + row.min_y() > clip.bottom() {
                break;
            }
            if origin.y + row.max_y() < clip.top() {
                continue;
            }
            let index =
                next.get_or_insert_with(|| self.timings.partition_point(|t| t.range.start < start));
            let mut x = origin.x + row.rect().right() + 12.0;
            while let Some(timing) = self.timings.get(*index).filter(|t| t.range.start <= end) {
                let label = results::format_duration(timing.elapsed);
                let layout = ui.painter().layout_no_wrap(
                    label.clone(),
                    FontId::monospace((font_size - 2.0).max(10.0)),
                    color,
                );
                let rect = Rect::from_min_size(pos2(x, origin.y + row.min_y()), layout.size());
                ui.expand_to_include_rect(rect);
                ui.painter().galley(rect.min, layout, color);
                let response = ui.interact(
                    rect,
                    ui.id().with(("execution_time", timing.range.start)),
                    Sense::hover(),
                );
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label)
                });
                response.on_hover_text(tr("Execution time, including data retrieval"));
                x = rect.right() + 12.0;
                *index += 1;
            }
        }
    }
}

impl Console {
    /// Preserve the existing Run All, nonblank selection, and caret statement scopes.
    pub fn prepare_run(&mut self, driver: Driver, all: bool) -> Vec<String> {
        let ranges = if all {
            vec![0..self.text.len()]
        } else if self.selection().is_some() {
            let (a, b) = self.cursor.unwrap();
            vec![char_to_byte(&self.text, a)..char_to_byte(&self.text, b)]
        } else {
            sql::statement_at(&self.text, self.cursor_byte(), driver)
                .into_iter()
                .map(|s| s.start..s.end)
                .collect()
        };
        self.prepare_ranges(driver, ranges)
    }

    pub(super) fn prepare_ranges(
        &mut self,
        driver: Driver,
        ranges: Vec<Range<usize>>,
    ) -> Vec<String> {
        self.execution
            .prepare(&self.text, (self.conn, self.db.clone()), driver, ranges)
    }

    pub fn execution_started(&mut self, statements: &[String]) {
        self.execution
            .begin(&self.text, self.conn, &self.db, statements);
    }

    pub fn execution_finished(&mut self, results: &[ResultSet], executions: &[StatementExecution]) {
        self.execution
            .finish(&self.text, self.conn, &self.db, results, executions);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn harness(text: &str) -> egui_kittest::Harness<'static, Console> {
        let mut first = true;
        let mut h = egui_kittest::Harness::builder()
            .with_size(vec2(1000.0, 720.0))
            .build_ui_state(
                move |ui, console: &mut Console| {
                    if std::mem::take(&mut first) {
                        theme::install_fonts(ui.ctx());
                        theme::apply(ui.ctx(), true);
                        return;
                    }
                    console.show_console(
                        ui,
                        &ConsoleCtx {
                            connections: &[],
                            schemas: None,
                            driver: Driver::Postgres,
                            dark: true,
                            font_size: 14.0,
                            columns: None,
                            auto_complete: false,
                        },
                    );
                },
                Console::new(1, "Execution test".into(), text.into(), None),
            );
        h.run_steps(3);
        h
    }

    #[test]
    fn backspace_before_a_trailing_newline_uses_updated_offsets() {
        let mut h = harness(&format!("{}  \n", "SELECT 1;".repeat(6)));
        let ctx = h.ctx.clone();
        h.state_mut().set_cursor(&ctx, 56, 56);
        ctx.memory_mut(|m| m.request_focus(h.state().editor_id()));
        h.run_steps(2);
        h.key_press(Key::Backspace);
        h.run_steps(2);
        assert_eq!(h.state().text.len(), 56);
        assert!(h.state().text.ends_with('\n'));
        assert_eq!(h.state().execution.line_starts, vec![0, 56]);
        h.key_press(Key::ArrowDown);
        h.run_steps(2);
        assert_eq!(h.state().line_col().0, 2);
        assert_eq!(h.state().execution.line_at(56), 2);
    }

    #[test]
    fn idle_frames_and_caret_moves_reuse_text_offsets_but_edits_refresh_them() {
        let mut h = harness("SELECT 'سلام';\nSELECT 2;");
        let ctx = h.ctx.clone();
        let statements = h.state_mut().prepare_run(Driver::Postgres, false);
        h.state_mut().execution_started(&statements);
        h.state_mut()
            .execution_finished(&[], &completion(&statements, 12));
        assert_eq!(h.state().execution.timings.len(), 1);
        h.run_steps(2);
        let syncs = h.state().execution.text_syncs;
        h.state_mut().set_cursor(&ctx, 2, 2);
        ctx.memory_mut(|m| m.request_focus(h.state().editor_id()));
        h.run_steps(5);
        h.key_press(Key::ArrowRight);
        h.run_steps(3);
        assert_eq!(h.state().execution.text_syncs, syncs);
        h.key_press(Key::Enter);
        h.run_steps(2);
        assert_eq!(h.state().execution.text_syncs, syncs + 1);
        assert_eq!(h.state().execution.line_starts.len(), 3);
        h.state_mut().conn = Some(7);
        h.run_steps(2);
        assert_eq!(h.state().execution.text_syncs, syncs + 1);
        assert_eq!(h.state().execution.source, Some((Some(7), None)));
    }

    fn completion(statements: &[String], ms: u64) -> Vec<StatementExecution> {
        statements
            .iter()
            .enumerate()
            .map(|(i, s)| StatementExecution {
                statement: s.clone(),
                results: i..i + 1,
                elapsed: Duration::from_millis(ms + i as u64),
                failed: false,
            })
            .collect()
    }

    #[test]
    fn direct_run_preserves_caret_selection_and_run_all_scopes() {
        let mut c = Console::new(
            1,
            "test".into(),
            "SELECT 1;\nSELECT 'سلام';\nSELECT 3;".into(),
            None,
        );
        c.cursor = Some((12, 12));
        assert_eq!(c.prepare_run(Driver::Postgres, false), vec!["SELECT 'سلام'"]);
        c.cursor = Some((10, c.text.chars().count()));
        assert_eq!(
            c.prepare_run(Driver::Postgres, false),
            vec!["SELECT 'سلام'", "SELECT 3"]
        );
        assert_eq!(
            c.prepare_run(Driver::Postgres, true),
            vec!["SELECT 1", "SELECT 'سلام'", "SELECT 3"]
        );
        c.text = "SELECT 1;\n   \nSELECT 2;".into();
        c.cursor = Some((10, 13));
        assert_eq!(c.prepare_run(Driver::Postgres, false), vec!["SELECT 1"]);
    }

    #[test]
    fn run_all_indexes_unicode_units_and_batch_members_in_source_order() {
        for driver in [Driver::Postgres, Driver::Sqlite] {
            let mut c = Console::new(1, "test".into(), "SELECT 'سلام';\n".repeat(10_000), None);
            let statements = c.prepare_run(driver, true);
            let plan = c.execution.prepared.as_ref().unwrap();
            if driver == Driver::Postgres {
                assert_eq!(statements.len(), 10_000);
                for (i, unit) in plan.units.iter().enumerate() {
                    assert_eq!(unit.range, i * 15..i * 15 + 13);
                }
            } else {
                assert_eq!(statements.len(), 1);
                assert_eq!(plan.units[0].members.len(), 10_000);
                for (i, (_, range)) in plan.units[0].members.iter().enumerate() {
                    assert_eq!(*range, i * 15..i * 15 + 13);
                }
            }
        }
    }

    #[test]
    fn partial_selection_timings_follow_their_unchanged_source() {
        for driver in [Driver::Postgres, Driver::Sqlite, Driver::MsSql] {
            for in_flight in [false, true] {
                let mut c =
                    Console::new(1, "test".into(), "SELECT 1 UNION SELECT 2;".into(), Some(7));
                c.cursor = Some((0, 8));
                let statements = c.prepare_run(driver, false);
                assert_eq!(statements, vec!["SELECT 1"]);
                c.execution_started(&statements);
                if !in_flight {
                    c.execution_finished(&[], &completion(&statements, 12));
                }
                c.text.insert_str(0, "-- before\n");
                if in_flight {
                    c.execution_finished(&[], &completion(&statements, 12));
                } else {
                    c.execution.sync(&c.text, c.conn, &c.db);
                }
                assert_eq!(c.execution.timings.len(), 1);
                assert_eq!(c.execution.timings[0].range, 10..18);
            }
        }
    }

    #[test]
    fn sql_prefix_edits_invalidate_completed_and_in_flight_timings() {
        for original in ["SELECT 1;", "\nSELECT 1;"] {
            for in_flight in [false, true] {
                let mut c = Console::new(1, "test".into(), original.into(), Some(7));
                let statements = c.prepare_run(Driver::Postgres, false);
                c.execution_started(&statements);
                if !in_flight {
                    c.execution_finished(&[], &completion(&statements, 12));
                    assert_eq!(c.execution.timings.len(), 1);
                }
                c.text.insert_str(0, "EXPLAIN ");
                if in_flight {
                    c.execution_finished(&[], &completion(&statements, 12));
                } else {
                    c.execution.sync(&c.text, c.conn, &c.db);
                }
                assert!(
                    c.execution.timings.is_empty(),
                    "a SQL prefix changed the timed statement"
                );
            }
        }
    }

    #[test]
    fn deleting_a_prior_separator_invalidates_timings_for_the_merged_statement() {
        for in_flight in [false, true] {
            let mut c = Console::new(1, "test".into(), "SELECT 0;\nSELECT 1;".into(), Some(7));
            c.cursor = Some((12, 12));
            let statements = c.prepare_run(Driver::Postgres, false);
            assert_eq!(statements, vec!["SELECT 1"]);
            c.execution_started(&statements);
            if !in_flight {
                c.execution_finished(&[], &completion(&statements, 12));
                assert_eq!(c.execution.timings.len(), 1);
            }
            c.text.remove(8);
            if in_flight {
                c.execution_finished(&[], &completion(&statements, 12));
            } else {
                c.execution.sync(&c.text, c.conn, &c.db);
            }
            assert!(
                c.execution.timings.is_empty(),
                "the separator joined two statements"
            );
        }
    }

    #[test]
    fn typing_at_statement_end_invalidates_completed_and_in_flight_timings() {
        for in_flight in [false, true] {
            let mut c = Console::new(1, "test".into(), "SELECT 1;".into(), Some(7));
            let statements = c.prepare_run(Driver::Postgres, false);
            c.execution_started(&statements);
            if !in_flight {
                c.execution_finished(&[], &completion(&statements, 12));
                assert_eq!(c.execution.timings.len(), 1);
            }
            c.text.insert(8, '0');
            if in_flight {
                c.execution_finished(&[], &completion(&statements, 12));
            } else {
                c.execution.sync(&c.text, c.conn, &c.db);
            }
            assert!(
                c.execution.timings.is_empty(),
                "changed SQL kept its duration"
            );
        }
    }

    #[test]
    fn repeated_queries_keep_their_own_duration_and_rebase_across_unicode_edits() {
        let mut c = Console::new(1, "test".into(), "SELECT 1;\nSELECT 1;".into(), Some(7));
        c.cursor = Some((12, 12));
        let statements = c.prepare_run(Driver::Postgres, false);
        c.execution_started(&statements);
        c.execution_finished(&[], &completion(&statements, 12));
        assert_eq!(c.execution.timings[0].range, 10..18);
        assert_eq!(c.execution.timings[0].elapsed, Duration::from_millis(12));
        c.text.insert_str(0, "-- سلام\n");
        c.execution.sync(&c.text, c.conn, &c.db);
        assert_eq!(c.execution.timings[0].range, 18..26);
        c.text.push_str("\nSELECT 2;");
        c.execution.sync(&c.text, c.conn, &c.db);
        assert_eq!(c.execution.timings[0].range, 18..26);
        c.text = c.text.replace("SELECT 1;\nSELECT 2", "SELECT 9;\nSELECT 2");
        c.execution.sync(&c.text, c.conn, &c.db);
        assert!(c.execution.timings.is_empty());
    }

    #[test]
    fn completions_ignore_changed_sql_sources_and_failed_executions() {
        for mode in 0..4 {
            let mut c = Console::new(1, "test".into(), "SELECT 'سلام';".into(), Some(7));
            let statements = c.prepare_run(Driver::Postgres, false);
            c.execution_started(&statements);
            let mut executions = completion(&statements, 12);
            match mode {
                0 => c.text = "SELECT 'changed';".into(),
                1 => c.conn = Some(8),
                2 => c.db = Some("other".into()),
                _ => executions[0].failed = true,
            }
            c.execution_finished(&[], &executions);
            assert!(c.execution.timings.is_empty());
        }
    }

    #[test]
    fn a_completed_query_can_move_while_running_and_rerun_clears_old_time() {
        let mut c = Console::new(1, "test".into(), "SELECT 1;".into(), Some(7));
        let statements = c.prepare_run(Driver::Postgres, false);
        c.execution_started(&statements);
        c.text.insert_str(0, "-- inserted\n");
        c.execution_finished(&[], &completion(&statements, 12));
        assert_eq!(c.execution.timings[0].range, 12..20);
        c.cursor = Some((14, 14));
        let statements = c.prepare_run(Driver::Postgres, false);
        c.execution_started(&statements);
        assert!(c.execution.timings.is_empty());
        c.execution_finished(&[], &completion(&statements, 21));
        assert_eq!(c.execution.timings[0].elapsed, Duration::from_millis(21));
        c.conn = Some(9);
        c.execution.sync(&c.text, c.conn, &c.db);
        assert!(c.execution.timings.is_empty());
    }

    #[test]
    fn sqlite_batch_results_and_sql_server_batch_time_have_correct_anchors() {
        for driver in [Driver::Sqlite, Driver::MsSql] {
            let mut c = Console::new(1, "test".into(), "SELECT 1;\nSELECT 1;".into(), None);
            let statements = c.prepare_run(driver, true);
            assert_eq!(statements.len(), 1);
            c.execution_started(&statements);
            let results = vec![
                ResultSet {
                    statement: "SELECT 1;".into(),
                    elapsed: Duration::from_millis(2),
                    ..Default::default()
                };
                2
            ];
            let execution = StatementExecution {
                statement: statements[0].clone(),
                results: 0..2,
                elapsed: Duration::from_millis(5),
                failed: false,
            };
            c.execution_finished(&results, &[execution]);
            if driver == Driver::Sqlite {
                assert_eq!(
                    c.execution
                        .timings
                        .iter()
                        .map(|t| t.range.start)
                        .collect::<Vec<_>>(),
                    vec![0, 10]
                );
                assert!(
                    c.execution
                        .timings
                        .iter()
                        .all(|t| t.elapsed == Duration::from_millis(2))
                );
            } else {
                assert_eq!(c.execution.timings.len(), 1);
                assert_eq!(c.execution.timings[0].elapsed, Duration::from_millis(5));
            }
        }
    }
}
