//! E9: single-writer SQLite actor on a dedicated OS thread (rusqlite), with
//! group commit, migrations via user_version, periodic maintenance and a
//! clean shutdown. Async callers talk to it through a bounded channel.
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug)]
pub enum DbError {
    /// The statement itself failed (constraint, etc.): fatal for this request.
    Statement(rusqlite::Error),
    /// The group transaction failed to commit: retryable by the caller.
    BatchAborted(String),
}

pub enum Cmd {
    AppendEvent { session: String, payload: Vec<u8>, reply: oneshot::Sender<Result<i64, DbError>> },
}

const MIGRATIONS: &[&str] = &[
    // v1
    "CREATE TABLE events(id INTEGER PRIMARY KEY, session TEXT NOT NULL, payload BLOB NOT NULL,
                         created_unix_ms INTEGER NOT NULL);
     CREATE INDEX events_session ON events(session, id);",
    // v2
    "CREATE TABLE outbox(seq INTEGER PRIMARY KEY, body BLOB NOT NULL, acked INTEGER NOT NULL DEFAULT 0);",
];

fn open_writer(path: &Path) -> rusqlite::Result<Connection> {
    let c = Connection::open(path)?;
    c.busy_timeout(Duration::from_secs(5))?;
    c.pragma_update(None, "journal_mode", "WAL")?;
    c.pragma_update(None, "synchronous", "NORMAL")?; // see RESEARCH.md: FULL if every commit must survive power loss
    c.pragma_update(None, "foreign_keys", "ON")?;
    c.pragma_update(None, "journal_size_limit", 16 * 1024 * 1024)?;
    c.set_prepared_statement_cache_capacity(64);
    let ok: String = c.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if ok != "ok" { eprintln!("quick_check failed: {ok}"); /* -> enter recovery path */ }
    migrate(&c)?;
    c.execute_batch("PRAGMA optimize=0x10002;")?;
    Ok(c)
}

fn migrate(c: &Connection) -> rusqlite::Result<()> {
    let v: usize = c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? as usize;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(v) {
        // Each step atomically: DDL + version bump in one IMMEDIATE transaction.
        c.execute_batch(&format!("BEGIN IMMEDIATE; {sql}; PRAGMA user_version = {}; COMMIT;", i + 1))?;
    }
    Ok(())
}

pub fn open_reader(path: &Path) -> rusqlite::Result<Connection> {
    let c = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    c.busy_timeout(Duration::from_secs(5))?;
    Ok(c) // keep read transactions SHORT: a pinned reader starves checkpoints
}

pub fn spawn_writer(path: PathBuf, cap: usize) -> (mpsc::Sender<Cmd>, std::thread::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<Cmd>(cap);
    let h = std::thread::Builder::new().name("sqlite-writer".into()).spawn(move || {
        let mut conn = open_writer(&path).expect("open db"); // fatal at startup -> exit non-zero
        let mut last_maint = Instant::now();
        let mut batch = Vec::with_capacity(64);
        // blocking_recv: we are on our own OS thread, not a runtime worker.
        while let Some(first) = rx.blocking_recv() {
            batch.push(first);
            while batch.len() < 64 { match rx.try_recv() { Ok(c) => batch.push(c), Err(_) => break } }
            let mut replies = Vec::new();
            let res = (|| -> rusqlite::Result<()> {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                for cmd in batch.drain(..) {
                    match cmd {
                        Cmd::AppendEvent { session, payload, reply } => {
                            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
                            let r = tx.prepare_cached("INSERT INTO events(session, payload, created_unix_ms) VALUES (?1, ?2, ?3)")
                                .and_then(|mut s| s.insert(rusqlite::params![session, payload, now]));
                            replies.push((reply, r));
                        }
                    }
                }
                tx.commit()
            })();
            for (reply, r) in replies.drain(..) {
                let r = match &res {
                    Ok(()) => r.map_err(DbError::Statement),
                    Err(e) => Err(DbError::BatchAborted(e.to_string())),
                };
                let _ = reply.send(r); // caller may have given up (timeout): ignore
            }
            if last_maint.elapsed() > Duration::from_secs(3600) {
                let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE); PRAGMA optimize;");
                last_maint = Instant::now();
            }
        }
        // All senders dropped => orderly shutdown.
        let _ = conn.execute_batch("PRAGMA optimize; PRAGMA wal_checkpoint(TRUNCATE);");
        drop(conn);
    }).expect("spawn writer");
    (tx, h)
}

pub fn backup_with_vacuum_into(reader: &Connection, dest: &Path) -> rusqlite::Result<()> {
    let _ = std::fs::remove_file(dest); // VACUUM INTO requires a non-existent or empty file
    reader.execute("VACUUM INTO ?1", [dest.to_string_lossy()])?;
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let dir = std::env::temp_dir().join(format!("e9-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("agent.db");
    let (tx, writer) = spawn_writer(db.clone(), 1024);
    let t = Instant::now();
    let mut pending = Vec::new();
    for i in 0..10_000 {
        let (rtx, rrx) = oneshot::channel();
        tx.send(Cmd::AppendEvent { session: format!("s{}", i % 7), payload: vec![1; 128], reply: rtx }).await.unwrap();
        pending.push(rrx);
    }
    for p in pending { p.await.unwrap().unwrap(); }
    println!("10k group-committed appends in {:?}", t.elapsed());
    let r = open_reader(&db).unwrap();
    let n: i64 = r.query_row("SELECT count(*) FROM events", [], |x| x.get(0)).unwrap();
    let v: i64 = r.query_row("PRAGMA user_version", [], |x| x.get(0)).unwrap();
    backup_with_vacuum_into(&r, &dir.join("backup.db")).unwrap();
    println!("rows={n} user_version={v} backup_bytes={}", std::fs::metadata(dir.join("backup.db")).unwrap().len());
    drop(r);
    drop(tx);
    writer.join().unwrap();
    println!("writer exited; wal after TRUNCATE = {} bytes",
        std::fs::metadata(dir.join("agent.db-wal")).map(|m| m.len()).unwrap_or(0));
    let _ = std::fs::remove_dir_all(&dir);
}
