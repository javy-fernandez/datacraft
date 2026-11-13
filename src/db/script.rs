//! "Run SQL Script…": `.sql` files run on a connection without opening them. The file is read in
//! pieces and each statement runs as soon as it is complete, so memory follows the largest
//! statement rather than the file. A failing statement waits for Retry, Skip, Skip all or Stop.

use super::{ConnectionConfig, Driver, Session};
use crate::i18n::tr;
use crate::sql::{Statement, split_batches, split_statements};
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Bytes read before the statements found so far are run.
const CHUNK: usize = 1 << 20;

/// What the user chose for a failing statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Retry,
    Skip,
    SkipAll,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub file: String,
    /// 1-based line where the statement starts.
    pub line: usize,
    pub statement: String,
    pub error: String,
}

#[derive(Debug, Clone, Default)]
pub struct Progress {
    /// Index in `files` of the file being run.
    pub file: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// Statements that ran without error.
    pub statements: usize,
    pub failed: Vec<Failure>,
    /// The failing statement waiting for a decision.
    pub waiting: Option<Failure>,
    pub finished: bool,
    /// Why the run ended early: Stop, Cancel, an unreadable file or a lost connection.
    pub stopped: Option<String>,
}

/// Shared between the dialog and the task that runs the files.
#[derive(Default)]
pub struct ScriptRun {
    pub progress: Mutex<Progress>,
    pub cancel: AtomicBool,
    pub decision: Mutex<Option<Decision>>,
}

impl ScriptRun {
    pub fn snapshot(&self) -> Progress {
        self.progress.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn decide(&self, d: Decision) {
        *self.decision.lock().unwrap_or_else(|e| e.into_inner()) = Some(d);
    }

    fn update(&self, f: impl FnOnce(&mut Progress)) {
        f(&mut self.progress.lock().unwrap_or_else(|e| e.into_inner()));
    }
}

/// Engines whose scripts are SQL (the others have their own command languages).
pub fn supported(driver: Driver) -> bool {
    !driver.is_nosql()
}

pub async fn run(cfg: ConnectionConfig, db: Option<String>, files: Vec<PathBuf>, run: Arc<ScriptRun>) {
    let total = files.iter().filter_map(|f| std::fs::metadata(f).ok()).map(|m| m.len()).sum();
    run.update(|p| p.bytes_total = total);
    let stopped = execute(cfg, db, &files, &run).await.err();
    run.update(|p| {
        p.finished = true;
        p.waiting = None;
        p.stopped = stopped;
    });
}

async fn execute(mut cfg: ConnectionConfig, db: Option<String>, files: &[PathBuf], run: &ScriptRun) -> Result<(), String> {
    let driver = cfg.driver;
    if let Some(db) = db.as_ref().filter(|_| driver != Driver::Oracle) {
        cfg.database.clone_from(db);
    }
    let mut session = Session::connect(&cfg).await.map_err(|e| format!("{e:#}"))?;
    session.use_database(db.as_deref().unwrap_or_default()).await.map_err(|e| format!("{e:#}"))?;
    let mut skip_all = false;
    let mut done_before = 0u64;
    for (index, path) in files.iter().enumerate() {
        run.update(|p| p.file = index);
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut reader = std::io::BufReader::new(file);
        let mut splitter = Splitter::new(driver);
        let mut eof = false;
        while !eof {
            // Whole lines, up to about CHUNK bytes; bytes that aren't UTF-8 become U+FFFD.
            let mut chunk = Vec::new();
            while chunk.len() < CHUNK {
                match reader.read_until(b'\n', &mut chunk) {
                    Ok(0) => {
                        eof = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => return Err(format!("{}: {e}", path.display())),
                }
            }
            let read = chunk.len() as u64;
            let pieces = if eof { splitter.feed_last(&String::from_utf8_lossy(&chunk)) } else { splitter.feed(&String::from_utf8_lossy(&chunk)) };
            for piece in pieces {
                loop {
                    if run.cancel.load(Ordering::SeqCst) {
                        return Err(tr("Cancelled").into());
                    }
                    let error = match session.execute(&piece.text, 0).await {
                        Ok(_) => {
                            run.update(|p| p.statements += 1);
                            break;
                        }
                        Err(e) => format!("{e:#}"),
                    };
                    if session.is_closed() {
                        return Err(tr("Connection lost").into());
                    }
                    let failure = Failure { file: name.clone(), line: piece.line, statement: piece.text.clone(), error };
                    let decision = if skip_all { Decision::Skip } else { ask(run, failure.clone()).await };
                    match decision {
                        Decision::Retry => continue,
                        Decision::Skip | Decision::SkipAll => {
                            skip_all |= decision == Decision::SkipAll;
                            run.update(|p| p.failed.push(failure));
                            break;
                        }
                        Decision::Stop => {
                            run.update(|p| p.failed.push(failure));
                            return Err(tr("Stopped").into());
                        }
                    }
                }
            }
            done_before += read;
            run.update(|p| p.bytes_done = done_before);
        }
    }
    Ok(())
}

/// Shows the failure and waits for the user's decision (Cancel counts as Stop).
async fn ask(run: &ScriptRun, failure: Failure) -> Decision {
    *run.decision.lock().unwrap_or_else(|e| e.into_inner()) = None;
    run.update(|p| p.waiting = Some(failure));
    let decision = loop {
        if run.cancel.load(Ordering::SeqCst) {
            break Decision::Stop;
        }
        if let Some(d) = run.decision.lock().unwrap_or_else(|e| e.into_inner()).take() {
            break d;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    run.update(|p| p.waiting = None);
    decision
}

/// A statement ready to run, with the line it starts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    pub text: String,
    pub line: usize,
}

/// Splits a script given in pieces. The last statement of what was given so far may continue in
/// the next piece, so it is kept until more text (or the end) arrives; everything before it is
/// final, since splitting reads left to right.
struct Splitter {
    driver: Driver,
    buf: String,
    /// Line of the file where `buf` (after `prefix`) starts.
    line: usize,
    /// `DELIMITER x\n` put back in front of `buf` when a MySQL script changed the delimiter in a
    /// part already run.
    prefix: usize,
}

impl Splitter {
    fn new(driver: Driver) -> Self {
        Self { driver, buf: String::new(), line: 1, prefix: 0 }
    }

    fn feed(&mut self, text: &str) -> Vec<Piece> {
        self.buf.push_str(text);
        let mut stmts = self.statements();
        let Some(last) = stmts.pop() else { return Vec::new() };
        let pieces = self.pieces(&stmts);
        let cut = last.start.max(self.prefix);
        let consumed = &self.buf[self.prefix..cut];
        self.line += consumed.matches('\n').count();
        let mut delimiter = self.delimiter_in(&self.buf[..self.prefix]);
        if matches!(self.driver, Driver::MySql | Driver::MariaDb)
            && let Some(d) = self.delimiter_in(consumed)
        {
            delimiter = Some(d);
        }
        let rest = self.buf[cut..].to_string();
        // Text before the kept statement can be a `DELIMITER` line: it is counted above.
        let prefix = delimiter.filter(|d| d != ";").map(|d| format!("DELIMITER {d}\n")).unwrap_or_default();
        self.prefix = prefix.len();
        self.buf = prefix + &rest;
        pieces
    }

    fn feed_last(&mut self, text: &str) -> Vec<Piece> {
        self.buf.push_str(text);
        let stmts = self.statements();
        let pieces = self.pieces(&stmts);
        self.buf.clear();
        pieces
    }

    fn pieces(&self, stmts: &[Statement]) -> Vec<Piece> {
        stmts
            .iter()
            .filter(|s| s.start >= self.prefix)
            .map(|s| Piece { text: s.text.clone(), line: self.line + self.buf[self.prefix..s.start].matches('\n').count() })
            .collect()
    }

    /// The last `DELIMITER x` line of `text` (MySQL's client directive).
    fn delimiter_in(&self, text: &str) -> Option<String> {
        text.lines().rev().find_map(|l| {
            let mut words = l.split_whitespace();
            match (words.next(), words.next()) {
                (Some(w), Some(d)) if w.eq_ignore_ascii_case("delimiter") => Some(d.to_string()),
                _ => None,
            }
        })
    }

    /// The statements (SQL Server: `GO` batches) of `buf`. SQLite's triggers keep their `;`s.
    fn statements(&self) -> Vec<Statement> {
        if self.driver != Driver::Sqlite {
            return split_batches(&self.buf, self.driver);
        }
        split_statements(&self.buf, self.driver)
            .into_iter()
            .map(|mut s| {
                // File pieces omit the outer terminator; the shared splitter preserves a
                // trigger's entire body, including its inner semicolons.
                if s.text.ends_with(';') {
                    s.text.pop();
                    s.end -= 1;
                }
                s
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds `script` in pieces of `size` bytes (on char boundaries) and returns what would run.
    fn run_in_pieces(script: &str, driver: Driver, size: usize) -> Vec<Piece> {
        let mut s = Splitter::new(driver);
        let mut out = Vec::new();
        let mut rest = script;
        while rest.len() > size {
            let mut cut = size;
            while !rest.is_char_boundary(cut) {
                cut += 1;
            }
            out.extend(s.feed(&rest[..cut]));
            rest = &rest[cut..];
        }
        out.extend(s.feed_last(rest));
        out
    }

    fn whole(script: &str, driver: Driver) -> Vec<Piece> {
        Splitter::new(driver).feed_last(script)
    }

    #[test]
    fn sqlite_keyword_identifiers_survive_streamed_trigger_boundaries() {
        let script = "CREATE TABLE begin(end);\nCREATE TABLE log(end);\nCREATE TRIGGER audit AFTER INSERT ON begin BEGIN\n UPDATE begin SET end = 2; INSERT INTO log(end) VALUES (NEW.end);\nEND;\nINSERT INTO begin VALUES (1);\nSELECT * FROM log;";
        for size in 1..script.len() {
            let pieces = run_in_pieces(script, Driver::Sqlite, size);
            assert_eq!(pieces.len(), 5, "pieces of {size}");
            assert!(pieces[2].text.ends_with("END"));
            let db = rusqlite::Connection::open_in_memory().unwrap();
            for piece in pieces {
                db.execute_batch(&piece.text).unwrap();
            }
            assert_eq!(
                db.query_row("SELECT end FROM log", [], |row| row.get::<_, i32>(0))
                    .unwrap(),
                1
            );
        }
    }

    #[test]
    fn pieces_split_like_the_whole_file() {
        let scripts = [
            (Driver::Postgres, "CREATE TABLE t (s text);\nINSERT INTO t VALUES ('a;b');\n-- c;\nCREATE FUNCTION f() RETURNS int AS $$ BEGIN RETURN 1; END $$ LANGUAGE plpgsql;\n/* x; */ SELECT 'ñandú';\n"),
            (Driver::MySql, "CREATE TABLE t (a int);\nDELIMITER $$\nCREATE PROCEDURE p()\nBEGIN\n  SELECT 1;\nEND$$\nDELIMITER ;\nINSERT INTO t VALUES (1);\nINSERT INTO t VALUES (2);\n"),
            (Driver::MsSql, "CREATE TABLE t (a int)\nGO\nDECLARE @x int = 1; INSERT INTO t VALUES (@x);\nGO\nSELECT 'GO;'\ngo\n"),
            (Driver::Oracle, "CREATE TABLE t (a NUMBER);\nCREATE OR REPLACE PROCEDURE p IS\nBEGIN\n  IF 1 = 1 THEN NULL; END IF;\nEND;\n/\nINSERT INTO t VALUES (1);\n"),
            (Driver::Sqlite, "CREATE TABLE a (x int);\nCREATE TRIGGER tr AFTER INSERT ON a BEGIN UPDATE a SET x = 1; DELETE FROM a WHERE x = 2; END;\nINSERT INTO a VALUES (1);\n"),
        ];
        for (driver, script) in scripts {
            let expected = whole(script, driver);
            for size in 1..script.len() {
                assert_eq!(run_in_pieces(script, driver, size), expected, "{driver:?} in pieces of {size}");
            }
        }
    }

    #[test]
    fn statements_keep_their_lines() {
        let script = "CREATE TABLE t (a int);\n\nINSERT INTO t\nVALUES (1);\nDELIMITER //\nSELECT 2 //\n";
        let lines: Vec<usize> = run_in_pieces(script, Driver::MySql, 7).iter().map(|p| p.line).collect();
        assert_eq!(lines, [1, 3, 6]);
        let sqlite = whole("CREATE TABLE a (x int);\nCREATE TRIGGER tr AFTER INSERT ON a BEGIN\n UPDATE a SET x = 1;\nEND;\nSELECT 1;", Driver::Sqlite);
        assert_eq!(sqlite.len(), 3, "{sqlite:?}");
        assert!(sqlite[1].text.ends_with("END"));
        assert_eq!(sqlite[2].line, 5);
    }

    /// A file runs against a real (SQLite) connection: failures wait for a decision, Skip goes on
    /// and the summary says where each one was.
    #[tokio::test]
    async fn runs_files_and_asks_on_errors() {
        let dir = std::env::temp_dir().join(format!("datacraft-script-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("t.db");
        std::fs::write(&db, b"").unwrap();
        let file = dir.join("dump.sql");
        let mut script = String::from("CREATE TABLE t (a int);\nINSERT INTO nope VALUES (1);\n");
        for i in 0..3000 {
            script.push_str(&format!("INSERT INTO t VALUES ({i});\n"));
        }
        script.push_str("INSERT INTO t VALUES (;\n");
        std::fs::write(&file, &script).unwrap();
        let mut cfg = ConnectionConfig::new(1, Driver::Sqlite);
        cfg.file_path = db.to_string_lossy().into_owned();

        let state = Arc::new(ScriptRun::default());
        let task = tokio::spawn(run(cfg.clone(), None, vec![file.clone()], state.clone()));
        let mut decisions = [Decision::Retry, Decision::Skip, Decision::SkipAll].into_iter();
        while !state.snapshot().finished {
            if state.snapshot().waiting.is_some() && state.decision.lock().unwrap().is_none() {
                state.decide(decisions.next().unwrap());
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        task.await.unwrap();
        let p = state.snapshot();
        assert_eq!((p.statements, p.stopped.as_deref(), p.bytes_done, p.bytes_total), (3001, None, script.len() as u64, script.len() as u64));
        let lines: Vec<usize> = p.failed.iter().map(|f| f.line).collect();
        assert_eq!(lines, [2, 3003]);
        assert_eq!(p.failed[0].file, "dump.sql");

        // Stop ends the run where it is (the table exists now, so the first statement fails).
        let state = Arc::new(ScriptRun::default());
        let task = tokio::spawn(run(cfg, None, vec![file], state.clone()));
        while state.snapshot().waiting.is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        state.decide(Decision::Stop);
        task.await.unwrap();
        let p = state.snapshot();
        assert_eq!((p.statements, p.failed.len(), p.failed[0].line, p.stopped.as_deref()), (0, 1, 1, Some(tr("Stopped"))));
        let _ = std::fs::remove_dir_all(dir);
    }
}
