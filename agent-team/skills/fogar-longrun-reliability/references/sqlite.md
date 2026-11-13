# SQLite in a weeks-long single process (Storage/Outbox adapter)

## rusqlite vs sqlx

| | rusqlite 0.42.0 | sqlx 0.9.0 (sqlite) |
|---|---|---|
| Model | Synchronous `Connection`. You choose the thread. | Async API. **Each connection runs on its own OS thread** (`sqlx-sqlite-worker-N`) fed by a bounded flume channel (command channel 50, row channel 50) [primary, `sqlx-sqlite/src/connection/worker.rs`, `options/mod.rs`]. |
| Defaults | Busy timeout **5000 ms** on new connections [primary, `busy.rs`]. The `bundled` feature compiles SQLite **3.54.1** with `-DSQLITE_DEFAULT_FOREIGN_KEYS=1` [primary, `libsqlite3-sys/build.rs`]. | busy_timeout 5 s, statement cache 100, foreign keys enabled by default, `optimize_on_close` disabled [primary]. |
| Control | Full access to pragmas, `wal_checkpoint_v2` (feature `wal`), the backup API (feature `backup`), `prepare_cached`, `get_interrupt_handle`, `release_memory`. | Pooling, compile-time checked queries, migrations macro. |
| Verdict for fogar | **Use this**: one writer thread you own and see, deterministic shutdown, no pool. | Fine, but the pool and its hidden threads add moving parts and nothing a single-writer daemon needs. |

**Recommendation**: rusqlite with `bundled`, which gives one known SQLite version on every OS and avoids macOS's system libsqlite3 version drift. Do not also link another SQLite: "if multiple copies of SQLite are linked into the same application ... A `close()` operation on one connection might unknowingly clear the locks on a different database connection, leading to database corruption" [primary, sqlite.org/howtocorrupt §2.3].

## Single-writer actor (compiled and run: `examples/src/bin/e9_sqlite_actor.rs`)

- A dedicated `std::thread` named `sqlite-writer` owns the only write `Connection`. Async code sends `Cmd`s over a bounded `tokio::sync::mpsc` and gets replies on a `oneshot`. The thread uses `blocking_recv()` (allowed because it is not a runtime worker).
- **Group commit**: take the first command, `try_recv` up to 63 more, then run them all in one `BEGIN IMMEDIATE` transaction. [exp]: 10,000 appends took about 51 ms on Linux tmpfs-like storage. Expect different numbers on a Mac SSD, but the same shape.
- The reply type distinguishes `Statement(err)` (fatal for that request) from `BatchAborted` (retryable).
- Maintenance runs inside the writer thread: hourly `PRAGMA wal_checkpoint(PASSIVE); PRAGMA optimize;`. On channel close it runs `PRAGMA optimize; PRAGMA wal_checkpoint(TRUNCATE);` and then drops the connection. [exp]: the WAL was 0 bytes after shutdown.
- Readers: separate `SQLITE_OPEN_READ_ONLY` connections, used from `spawn_blocking` behind a small semaphore or from a reader thread. **Keep read transactions short** (next section).

Open sequence:
```rust
c.busy_timeout(Duration::from_secs(5))?;
c.pragma_update(None, "journal_mode", "WAL")?;      // persistent once set
c.pragma_update(None, "synchronous", "NORMAL")?;    // see the trade-off below
c.pragma_update(None, "foreign_keys", "ON")?;       // explicit; do not rely on build flags
c.pragma_update(None, "journal_size_limit", 16 * 1024 * 1024)?;
let ok: String = c.query_row("PRAGMA quick_check", [], |r| r.get(0))?; // O(N); integrity_check is O(N log N)
migrate(&c)?;                                        // user_version-driven, one IMMEDIATE txn per step
c.execute_batch("PRAGMA optimize=0x10002;")?;        // sqlite.org advice for long-lived connections
```

## Busy handling: the trap (E4, executed)

`examples/src/bin/e4_sqlite.rs` [exp]:
```
(a) DEFERRED upgrade -> DatabaseBusy (extended code 517) after 4.497µs (busy_timeout=2s NOT honoured)
(a) IMMEDIATE waited 329ms on the busy handler and then succeeded
```
517 is `SQLITE_BUSY_SNAPSHOT`. Docs [primary]: a deferred read transaction that tries to write after another connection has committed "will fail with SQLITE_BUSY" (lang_transaction), and the busy handler is skipped when "invoking the busy handler could result in a deadlock" (sqlite3.h). **Rule: every write transaction is `TransactionBehavior::Immediate`**. With a single writer this mostly matters for external tools and backups, but it is still the rule.

## WAL growth and checkpoint starvation (E4b, executed)

[exp]:
```
(b) WAL with a reader pinned open: 78734 KiB   (autocheckpoint=1000 pages could not reset it)
(b) after reader ended + more commits: WAL file still 78750 KiB (reused, not truncated)
(b) wal_checkpoint(TRUNCATE) -> busy=0 ...; WAL now 0 KiB
(b) set journal_size_limit=4MiB; starved again: 80468 KiB; reader ended + 300 commits: WAL 4096 KiB
```
Docs [primary, sqlite.org/wal]: the default auto-checkpoint is at 1000 pages. "if ... there is always at least one active reader, then no checkpoints will be able to complete and hence the WAL file will grow without bound". The fix is "reader gaps". The checkpoint "does not normally truncate the WAL file (unless the journal_size_limit pragma is set)".
Rules:
- No long-lived read transactions. Iterate in pages (`WHERE id > ? LIMIT 500`), and never hold a `Statement` mid-iteration across an `.await` or a channel wait.
- Set `journal_size_limit`, export the WAL file size as a metric, and alarm if it exceeds 64 MiB.
- Run `wal_checkpoint(TRUNCATE)` at clean shutdown, and periodically when idle.

## Durability vs speed: `synchronous`

- sqlite.org/pragma [primary]: "WAL mode is safe from corruption with synchronous=NORMAL ... A transaction committed in WAL mode with synchronous=NORMAL might roll back following a power loss or system crash. Transactions are durable across application crashes regardless". In WAL+NORMAL, "the checkpoint is the only operation to issue an I/O barrier".
- sqlite.org/howtocorrupt [primary]: "For maximum reliability and for robustness against database corruption, SQLite should always be run with its default synchronous setting of FULL." **This conflicts** with the pragma page's "safe from corruption" statement. Recorded in RESEARCH.md.
- macOS: plain `fsync` does not flush the drive cache. SQLite uses `F_FULLFSYNC` only when `PRAGMA fullfsync=ON` (or `checkpoint_fullfsync`) [primary, sqlite3.c comments on HAVE_FULLFSYNC; secondary for the macOS fsync semantics].
- **Choice for fogar**: `synchronous=NORMAL` for the high-rate tables (events, state), because losing the last second of state on a kernel panic or power loss is acceptable since the gateway resyncs. A MacBook has a battery, which makes power loss rare. The **outbox of must-deliver facts** gets durability by being acked end-to-end (the gateway acks only what it has stored). If a fact must survive power loss locally, write it with `PRAGMA synchronous=FULL` set for that connection, or use a separate DB file. Set `checkpoint_fullfsync=ON` on macOS so checkpoints act as real barriers [unverified cost; measure it].

## Space: vacuum

- `auto_vacuum` must be set **before the first table is created**. Changing it later needs a full `VACUUM` [primary, pragma docs]. Create the DB with `PRAGMA auto_vacuum=INCREMENTAL` in migration v0, then run `PRAGMA incremental_vacuum(N)` in the maintenance tick after retention deletes.
- Full `VACUUM` needs up to twice the DB size in free space and a write lock [primary, lang_vacuum]. Run it rarely (for example monthly, when the freelist exceeds 30% and the agent is idle), never automatically on low disk.
- Retention: delete in batches (`DELETE ... WHERE id IN (SELECT id ... LIMIT 1000)`) to keep transactions short.

## Backup and corruption

- Backups: `VACUUM INTO 'path'` ("a consistent snapshot", minimal size; the destination must not exist or must be empty; "if interrupted ... might be incomplete and corrupt", so write to a temp name and rename after success) or the online backup API (rusqlite `backup` feature) [primary]. **Never copy the db file with `cp` while it is open** [primary, howtocorrupt §1.2]. Never delete `-wal`/`-shm` files.
- Daily: `VACUUM INTO` to `backup-YYYYMMDD.db.tmp`, rename, keep 3.
- On startup, if `quick_check` is not `ok`: rename the db aside (`corrupt-<ts>.db` with its `-wal`), restore the latest backup or start empty, report `db_recovered` health, and let the gateway resync. Do not crash-loop on a corrupt DB.
- Never `fork()` with open connections, and never `close()` a raw fd of the DB file [primary, howtocorrupt §2.2 and §2.7].
- `SQLITE_FULL`: enter a degraded "storage full" mode. Stop non-essential writes, keep the outbox, and alert. Do not retry in a hot loop.

## Migrations

- Use `PRAGMA user_version`, with an ordered list of SQL steps applied each in `BEGIN IMMEDIATE; ...; PRAGMA user_version=N; COMMIT;` (E9). Migrations are forward-only. A DB version newer than the binary knows means **refuse to write** and report it, since that indicates a downgrade.
- Crates: `rusqlite_migration` 2.7.0 and `refinery` 0.10.0 exist [primary, crates.io]. The hand-rolled 15-line version is enough and has no extra dependency.
- After DDL, run `PRAGMA optimize` [primary advice].

## Async boundaries

- Never call rusqlite on a runtime worker. If a quick read is needed from async code, use `spawn_blocking` behind a `Semaphore(2)` with a timeout. On timeout, call `InterruptHandle::interrupt()` from `get_interrupt_handle()` to cancel the statement [primary: API exists; behavior per the SQLite docs].
