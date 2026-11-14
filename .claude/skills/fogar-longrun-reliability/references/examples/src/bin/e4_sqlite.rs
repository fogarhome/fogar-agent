//! E4: SQLite behaviours that matter for a long-running single process.
//! (a) BEGIN DEFERRED read->write upgrade returns SQLITE_BUSY despite busy_timeout,
//!     BEGIN IMMEDIATE waits on the busy handler instead.
//! (b) A long-lived read transaction starves checkpoints -> WAL grows; ends -> recover.
use rusqlite::{Connection, ErrorCode, TransactionBehavior};
use std::time::{Duration, Instant};

fn open(path: &str) -> Connection {
    let c = Connection::open(path).unwrap();
    c.pragma_update(None, "journal_mode", "WAL").unwrap();
    c.pragma_update(None, "synchronous", "NORMAL").unwrap();
    c.busy_timeout(Duration::from_secs(2)).unwrap();
    c
}
fn wal_size(path: &str) -> u64 {
    std::fs::metadata(format!("{path}-wal")).map(|m| m.len()).unwrap_or(0)
}

fn main() {
    let dir = std::env::temp_dir().join(format!("e4-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.db").to_string_lossy().to_string();
    let a = open(&path);
    a.execute_batch("CREATE TABLE kv(k INTEGER PRIMARY KEY, v BLOB);").unwrap();
    a.execute("INSERT INTO kv VALUES(1, x'00')", []).unwrap();

    // (a) deferred upgrade
    let mut b = open(&path);
    let tx_b = b.transaction_with_behavior(TransactionBehavior::Deferred).unwrap();
    let _: i64 = tx_b.query_row("SELECT count(*) FROM kv", [], |r| r.get(0)).unwrap(); // read snapshot
    a.execute("UPDATE kv SET v = x'01' WHERE k = 1", []).unwrap(); // another writer commits
    let t = Instant::now();
    let r = tx_b.execute("UPDATE kv SET v = x'02' WHERE k = 1", []);
    match r {
        Err(rusqlite::Error::SqliteFailure(e, _)) => println!(
            "(a) DEFERRED upgrade -> {:?} (extended code {}) after {:?} (busy_timeout=2s NOT honoured)",
            e.code, e.extended_code, t.elapsed()),
        other => println!("(a) unexpected: {other:?}"),
    }
    drop(tx_b);
    // IMMEDIATE waits on the handler while another writer holds the lock briefly
    let path2 = path.clone();
    let holder = std::thread::spawn(move || {
        let mut c = open(&path2);
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
        tx.execute("UPDATE kv SET v = x'03' WHERE k = 1", []).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        tx.commit().unwrap();
    });
    std::thread::sleep(Duration::from_millis(50));
    let t = Instant::now();
    let tx = b.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
    tx.execute("UPDATE kv SET v = x'04' WHERE k = 1", []).unwrap();
    tx.commit().unwrap();
    println!("(a) IMMEDIATE waited {:?} on the busy handler and then succeeded", t.elapsed());
    holder.join().unwrap();
    let _ = ErrorCode::DatabaseBusy;

    // (b) checkpoint starvation
    let reader = open(&path);
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader.query_row("SELECT count(*) FROM kv", [], |r| r.get(0)).unwrap();
    let blob = vec![7u8; 4096];
    for i in 0..5000 {
        a.execute("INSERT OR REPLACE INTO kv VALUES(?1, ?2)", rusqlite::params![i % 500 + 10, blob]).unwrap();
    }
    println!("(b) WAL with a reader pinned open: {} KiB (autocheckpoint=1000 pages could not reset it)", wal_size(&path) / 1024);
    reader.execute_batch("COMMIT").unwrap();
    for i in 0..50 {
        a.execute("INSERT OR REPLACE INTO kv VALUES(?1, ?2)", rusqlite::params![i + 10, blob]).unwrap();
    }
    println!("(b) after reader ended + more commits: WAL file still {} KiB (reused, not truncated)", wal_size(&path) / 1024);
    let (busy, log, ckpt): (i64, i64, i64) = a
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap();
    println!("(b) wal_checkpoint(TRUNCATE) -> busy={busy} log={log} ckpt={ckpt}; WAL now {} KiB", wal_size(&path) / 1024);
    a.pragma_update(None, "journal_size_limit", 4 * 1024 * 1024).unwrap();
    println!("(b) set journal_size_limit=4MiB so future checkpoints truncate oversize WAL");
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader.query_row("SELECT count(*) FROM kv", [], |r| r.get(0)).unwrap();
    for i in 0..5000 {
        a.execute("INSERT OR REPLACE INTO kv VALUES(?1, ?2)", rusqlite::params![i % 500 + 10, blob]).unwrap();
    }
    println!("(b) starved again with limit set: WAL {} KiB", wal_size(&path) / 1024);
    reader.execute_batch("COMMIT").unwrap();
    for i in 0..300 {
        a.execute("INSERT OR REPLACE INTO kv VALUES(?1, ?2)", rusqlite::params![i + 10, blob]).unwrap();
    }
    println!("(b) reader ended + 300 commits (passive autocheckpoint only): WAL {} KiB", wal_size(&path) / 1024);
    let _ = std::fs::remove_dir_all(&dir);
}
