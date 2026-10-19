//! Session + message store (Epic C1).
//!
//! rusqlite (bundled) behind the tokio-rusqlite dedicated thread (ADR-005),
//! WAL + `synchronous=FULL` (a kill -9 loses nothing committed — throughput
//! is irrelevant for a single-user agent), `rusqlite_migration` versions,
//! backup-before-migrate, refuse-on-downgrade.
//!
//! Also home to the READ LEDGER (C1-for-F): every range the agent shows the
//! model is recorded with its snapshot tag, so the future hashline
//! seen-lines guard can distinguish shown lines from elided ones. The ledger
//! stores tag STRINGS only (no dependency on polymorph-tools).
//!
//! Error discipline: worker closures return `Result<_, StoreError>` directly
//! (no stringly markers); the single `call_err` maps the (non-exhaustive)
//! tokio-rusqlite enum at the boundary.
//!
//! Crash-consistency contract (TBM-13, unit form — true OS-kill fuzz stays
//! OPEN): dropping the store without `close()` (the in-process kill analog)
//! loses nothing committed; reopen + `integrity_check` proves it. Pinned by
//! `crash_reopen_stress` below.

use rusqlite_migration::{Migrations, M};
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Current schema version. Bump with a new `M::up` below, never edit old ones.
pub const SCHEMA_VERSION: u32 = 4;

fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(
            "CREATE TABLE sessions (\
               id TEXT PRIMARY KEY, \
               title TEXT NOT NULL DEFAULT 'untitled', \
               created_at INTEGER NOT NULL, \
               updated_at INTEGER NOT NULL\
             );\
             CREATE TABLE messages (\
               id INTEGER PRIMARY KEY AUTOINCREMENT, \
               session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE, \
               role TEXT NOT NULL, \
               content TEXT NOT NULL, \
               created_at INTEGER NOT NULL\
             );\
             CREATE INDEX idx_messages_session ON messages(session_id, id);\
             CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);",
        ),
        M::up(
            "CREATE TABLE reads (\
               id INTEGER PRIMARY KEY AUTOINCREMENT, \
               session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE, \
               path TEXT NOT NULL, \
               start_line INTEGER NOT NULL, \
               end_line INTEGER NOT NULL, \
               tag TEXT NOT NULL, \
               shown_at INTEGER NOT NULL\
             );\
             CREATE INDEX idx_reads_session_path ON reads(session_id, path);",
        ),
        // Chunk 50 (G task persistence, first slice): one plan graph per
        // session, stored as validated JSON. Cascade keeps deletes clean
        // (FK enforced per-connection in configure(), traps-#11).
        M::up(
            "CREATE TABLE plan_state (\
               session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE, \
               graph_json TEXT NOT NULL, \
               updated_at INTEGER NOT NULL\
             );",
        ),
        // Chunk 154: persist circuit traces / tool execution log on messages
        // so tool history and circuit quickview survive restart/switching.
        M::up("ALTER TABLE messages ADD COLUMN circuit_log TEXT;"),
    ])
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Tool,
}

impl MessageRole {
    fn as_str(self) -> &'static str {
        match self {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::System => "system",
            MessageRole::Tool => "tool",
        }
    }
}

impl FromStr for MessageRole {
    type Err = StoreError;
    fn from_str(s: &str) -> Result<Self, StoreError> {
        match s {
            "user" => Ok(MessageRole::User),
            "assistant" => Ok(MessageRole::Assistant),
            "system" => Ok(MessageRole::System),
            "tool" => Ok(MessageRole::Tool),
            _ => Err(StoreError::BadRole(s.into())),
        }
    }
}

impl std::fmt::Display for MessageRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub id: i64,
    pub session_id: String,
    pub role: MessageRole,
    pub content: String,
    pub circuit_log: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReadRecord {
    pub session_id: String,
    pub path: String,
    pub start_line: i64,
    pub end_line: i64,
    pub tag: String,
    pub shown_at: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(String),
    #[error("connection closed")]
    Closed,
    #[error("unknown message role: {0}")]
    BadRole(String),
    #[error("session not found: {0}")]
    NoSession(String),
    #[error("refusing to open newer schema v{found} (known v{known}) — downgrade refused")]
    Downgrade { found: u32, known: u32 },
    #[error("integrity check failed: {0}")]
    Integrity(String),
    #[error("I/O error: {0}")]
    Io(String),
}

impl StoreError {
    fn sql(e: impl std::fmt::Display) -> Self {
        StoreError::Sqlite(e.to_string())
    }
}

/// Single boundary mapper for the non-exhaustive tokio-rusqlite enum.
/// `call()` closures below return `Result<_, StoreError>`, so `Error(inner)`
/// unwraps back to itself and only transport failures map here.
fn call_err(e: tokio_rusqlite::Error<StoreError>) -> StoreError {
    match e {
        tokio_rusqlite::Error::ConnectionClosed => StoreError::Closed,
        tokio_rusqlite::Error::Close((_, inner)) => {
            StoreError::Sqlite(format!("close failed: {inner}"))
        }
        tokio_rusqlite::Error::Error(inner) => inner,
        _ => StoreError::Sqlite("tokio-rusqlite: unknown error variant".into()),
    }
}

/// Per-connection pragmas. FK enforcement is per-connection in SQLite —
/// forgetting this silently orphans messages on session delete.
fn configure(conn: &mut rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;\
         PRAGMA synchronous=FULL;\
         PRAGMA foreign_keys=ON;",
    )
}

/// Copy `db_path` to `<parent>/backup-<unix-ts>[-<n>]/<file>` before migrating.
/// Best-effort: a failed backup ABORTS the open (migrating without a backup
/// is worse than not opening). The `-<n>` suffix disambiguates two migrates
/// within one timestamp second (same-second overwrites would eat a backup).
fn backup_db(db_path: &Path) -> Result<PathBuf, StoreError> {
    let ts = now_unix();
    let parent = db_path.parent().unwrap_or(Path::new("."));
    let mut backup_dir = parent.join(format!("backup-{ts}"));
    for n in 1..1000 {
        if !backup_dir.exists() {
            break;
        }
        backup_dir = parent.join(format!("backup-{ts}-{n}"));
    }
    std::fs::create_dir_all(&backup_dir).map_err(StoreError::sql)?;
    let dest = backup_dir.join(
        db_path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("sessions.db")),
    );
    std::fs::copy(db_path, &dest).map_err(StoreError::sql)?;
    Ok(dest)
}

pub struct Store {
    conn: tokio_rusqlite::Connection,
}

impl Store {
    async fn open_conn(
        conn: tokio_rusqlite::Connection,
        db_path: Option<&Path>,
        db_existed_before_open: bool,
    ) -> Result<Self, StoreError> {
        // Pragmas first (FK behavior needed by every op below).
        conn.call(|c| configure(c).map_err(StoreError::sql))
            .await
            .map_err(call_err)?;

        // Refuse-on-downgrade BEFORE migrating: a newer user_version means a
        // newer Polymorph touched this DB; opening would corrupt it.
        let found: u32 = conn
            .call(|c| {
                c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                    .map(|v| v.max(0) as u32)
                    .map_err(StoreError::sql)
            })
            .await
            .map_err(call_err)?;
        if found > SCHEMA_VERSION {
            return Err(StoreError::Downgrade {
                found,
                known: SCHEMA_VERSION,
            });
        }

        // Backup-before-migrate: only when migration will actually run AND a
        // file existed BEFORE we opened (fresh creates have nothing to lose).
        // NOTE: `existed` is captured pre-open because our own WAL pragma
        // materializes the file — checking here would always be true.
        if found < SCHEMA_VERSION {
            if let Some(p) = db_path {
                if db_existed_before_open && p.exists() {
                    let dest = backup_db(p)?;
                    tracing::info!(backup = %dest.display(), "session DB backed up before migration");
                }
            }
        }

        conn.call(|c| migrations().to_latest(c).map_err(StoreError::sql))
            .await
            .map_err(call_err)?;
        Ok(Store { conn })
    }

    /// Open (creating parents). Runs backup + migrations as above.
    pub async fn open(db_path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = db_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(StoreError::sql)?;
            }
        }
        // Existence BEFORE open: SQLite (and our WAL pragma) materializes the
        // file, so this flag is the only honest fresh-create signal.
        let existed = db_path.exists();
        let conn = tokio_rusqlite::Connection::open(db_path)
            .await
            .map_err(|e| StoreError::Sqlite(e.to_string()))?;
        Self::open_conn(conn, Some(db_path), existed).await
    }

    /// In-memory store for tests (no backup: nothing on disk to lose).
    pub async fn open_in_memory() -> Result<Self, StoreError> {
        let conn = tokio_rusqlite::Connection::open_in_memory()
            .await
            .map_err(|e| StoreError::Sqlite(e.to_string()))?;
        Self::open_conn(conn, None, false).await
    }

    pub async fn close(self) -> Result<(), StoreError> {
        self.conn.close().await.map_err(|_| StoreError::Closed)
    }

    pub async fn create_session(&self, title: &str) -> Result<Session, StoreError> {
        let id = uuid::Uuid::new_v4().to_string();
        self.create_session_with_id(&id, title).await
    }

    /// Create a session with a caller-chosen id (chunk 51: `plan-drive
    /// --session` initializes its persistence slot on first use —
    /// checkpoint-file precedent: an absent slot means fresh, saved at
    /// the end). Duplicate ids refuse with the SQLite constraint error.
    pub async fn create_session_with_id(
        &self,
        id: &str,
        title: &str,
    ) -> Result<Session, StoreError> {
        let now = now_unix();
        let (id_c, title_c) = (id.to_string(), title.to_string());
        self.conn
            .call(move |c| {
                c.execute(
                    "INSERT INTO sessions (id, title, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![id_c, title_c, now, now],
                )
                .map_err(StoreError::sql)?;
                Ok(())
            })
            .await
            .map_err(call_err)?;
        Ok(Session {
            id: id.to_string(),
            title: title.to_string(),
            created_at: now,
            updated_at: now,
        })
    }

    pub async fn list_sessions(&self) -> Result<Vec<Session>, StoreError> {
        self.conn
            .call(|c| {
                let mut stmt = c
                    .prepare("SELECT id, title, created_at, updated_at FROM sessions ORDER BY updated_at DESC, id ASC")
                    .map_err(StoreError::sql)?;
                let rows = stmt
                    .query_map([], |r| {
                        Ok(Session {
                            id: r.get(0)?,
                            title: r.get(1)?,
                            created_at: r.get(2)?,
                            updated_at: r.get(3)?,
                        })
                    })
                    .map_err(StoreError::sql)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(StoreError::sql)
            })
            .await
            .map_err(call_err)
    }

    async fn update_title(&self, id: &str, title: &str) -> Result<usize, StoreError> {
        let (id_c, title_c) = (id.to_string(), title.to_string());
        self.conn
            .call(move |c| {
                c.execute(
                    "UPDATE sessions SET title = ?1 WHERE id = ?2",
                    rusqlite::params![title_c, id_c],
                )
                .map_err(StoreError::sql)
            })
            .await
            .map_err(call_err)
    }

    pub async fn rename_session(&self, id: &str, title: &str) -> Result<(), StoreError> {
        if self.update_title(id, title).await? == 0 {
            return Err(StoreError::NoSession(id.into()));
        }
        Ok(())
    }

    pub async fn delete_session(&self, id: &str) -> Result<(), StoreError> {
        let id_c = id.to_string();
        let changed = self
            .conn
            .call(move |c| {
                c.execute(
                    "DELETE FROM sessions WHERE id = ?1",
                    rusqlite::params![id_c],
                )
                .map_err(StoreError::sql)
            })
            .await
            .map_err(call_err)?;
        if changed == 0 {
            return Err(StoreError::NoSession(id.into()));
        }
        Ok(())
    }

    /// Append one message; bumps the session's updated_at in the same thread
    /// hop (one round trip, no torn timestamps).
    pub async fn append_message(
        &self,
        session_id: &str,
        role: MessageRole,
        content: &str,
    ) -> Result<Message, StoreError> {
        self.append_message_with_circuit(session_id, role, content, None)
            .await
    }

    /// Append one message with optional circuit execution log / telemetry.
    pub async fn append_message_with_circuit(
        &self,
        session_id: &str,
        role: MessageRole,
        content: &str,
        circuit_log: Option<&str>,
    ) -> Result<Message, StoreError> {
        let (sid_c, content_c) = (session_id.to_string(), content.to_string());
        let circuit_log_c = circuit_log.map(|s| s.to_string());
        let circuit_log_sql = circuit_log_c.clone();
        let role_s = role.as_str().to_string();
        let now = now_unix();
        let mid = self
            .conn
            .call(move |c| {
                let exists: i64 = c
                    .query_row(
                        "SELECT COUNT(*) FROM sessions WHERE id = ?1",
                        rusqlite::params![sid_c],
                        |r| r.get(0),
                    )
                    .map_err(StoreError::sql)?;
                if exists == 0 {
                    return Err(StoreError::NoSession(sid_c.clone()));
                }
                c.execute(
                    "INSERT INTO messages (session_id, role, content, circuit_log, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![sid_c, role_s, content_c, circuit_log_sql, now],
                )
                .map_err(StoreError::sql)?;
                Ok(c.last_insert_rowid())
            })
            .await
            .map_err(call_err)?;
        Ok(Message {
            id: mid,
            session_id: session_id.into(),
            role,
            content: content.into(),
            circuit_log: circuit_log_c,
            created_at: now,
        })
    }

    /// Last `limit` messages, oldest-first (`limit <= 0` = all).
    pub async fn history(&self, session_id: &str, limit: i64) -> Result<Vec<Message>, StoreError> {
        let sid_c = session_id.to_string();
        self.conn
            .call(move |c| {
                let sql = if limit > 0 {
                    "SELECT id, session_id, role, content, circuit_log, created_at FROM \
                     (SELECT * FROM messages WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2) \
                     ORDER BY id ASC"
                } else {
                    "SELECT id, session_id, role, content, circuit_log, created_at FROM messages \
                     WHERE session_id = ?1 ORDER BY id ASC"
                };
                let mut stmt = c.prepare(sql).map_err(StoreError::sql)?;
                // Roles parse OUTSIDE the row closure: a tampered role string
                // becomes a clean StoreError, not rusqlite internals.
                let map = |r: &rusqlite::Row| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, i64>(5)?,
                    ))
                };
                let rows = if limit > 0 {
                    stmt.query_map(rusqlite::params![sid_c, limit], map)
                } else {
                    stmt.query_map(rusqlite::params![sid_c], map)
                }
                .map_err(StoreError::sql)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(StoreError::sql)?
                    .into_iter()
                    .map(|(id, sid, role_s, content, circuit_log, ts)| {
                        Ok(Message {
                            id,
                            session_id: sid,
                            role: role_s.parse::<MessageRole>()?,
                            content,
                            circuit_log,
                            created_at: ts,
                        })
                    })
                    .collect::<Result<Vec<_>, StoreError>>()
            })
            .await
            .map_err(call_err)
    }

    /// Retrieve the most recent circuit log stored on any message in this session.
    pub async fn get_latest_circuit_log(
        &self,
        session_id: &str,
    ) -> Result<Option<String>, StoreError> {
        let sid_c = session_id.to_string();
        self.conn
            .call(move |c| {
                let mut stmt = c
                    .prepare(
                        "SELECT circuit_log FROM messages \
                         WHERE session_id = ?1 AND circuit_log IS NOT NULL AND circuit_log != '' \
                         ORDER BY id DESC LIMIT 1",
                    )
                    .map_err(StoreError::sql)?;
                let mut rows = stmt
                    .query_map(rusqlite::params![sid_c], |r| r.get::<_, String>(0))
                    .map_err(StoreError::sql)?;
                match rows.next() {
                    Some(Ok(s)) => Ok(Some(s)),
                    Some(Err(e)) => Err(StoreError::sql(e)),
                    None => Ok(None),
                }
            })
            .await
            .map_err(call_err)
    }

    /// Fork: new session copying messages AND read-ledger rows (same shown
    /// content ⇒ same tags stay valid). Returns the new session. A ghost
    /// source rolls back the empty session (no litter) and reports NoSession.
    pub async fn fork_session(&self, id: &str, title: &str) -> Result<Session, StoreError> {
        let new_session = self.create_session(title).await?;
        let (from_c, to_c) = (id.to_string(), new_session.id.clone());
        let copy: Result<(), StoreError> = self
            .conn
            .call(move |c| {
                let n: i64 = c
                    .query_row(
                        "SELECT COUNT(*) FROM sessions WHERE id = ?1",
                        rusqlite::params![from_c],
                        |r| r.get(0),
                    )
                    .map_err(StoreError::sql)?;
                if n == 0 {
                    return Err(StoreError::NoSession(from_c.clone()));
                }
                c.execute(
                    "INSERT INTO messages (session_id, role, content, circuit_log, created_at) \
                     SELECT ?1, role, content, circuit_log, created_at FROM messages WHERE session_id = ?2 ORDER BY id ASC",
                    rusqlite::params![to_c, from_c],
                )
                .map_err(StoreError::sql)?;
                c.execute(
                    "INSERT INTO reads (session_id, path, start_line, end_line, tag, shown_at) \
                     SELECT ?1, path, start_line, end_line, tag, shown_at FROM reads WHERE session_id = ?2 ORDER BY id ASC",
                    rusqlite::params![to_c, from_c],
                )
                .map_err(StoreError::sql)?;
                // Chunk 51 (fork-copy decision: COPY, like messages+reads —
                // a forked plan drive resumes where the parent stood).
                c.execute(
                    "INSERT INTO plan_state (session_id, graph_json, updated_at) \
                     SELECT ?1, graph_json, updated_at FROM plan_state WHERE session_id = ?2",
                    rusqlite::params![to_c, from_c],
                )
                .map_err(StoreError::sql)?;
                Ok(())
            })
            .await
            .map_err(call_err);
        if copy.is_err() {
            let _ = self.delete_session(&new_session.id).await;
        }
        copy.map(|()| new_session)
    }

    /// Record a shown range (the read ledger for the future seen-lines guard).
    pub async fn record_read(
        &self,
        session_id: &str,
        path: &str,
        start_line: i64,
        end_line: i64,
        tag: &str,
    ) -> Result<(), StoreError> {
        let (sid_c, path_c, tag_c) = (session_id.to_string(), path.to_string(), tag.to_string());
        let now = now_unix();
        self.conn
            .call(move |c| {
                c.execute(
                    "INSERT INTO reads (session_id, path, start_line, end_line, tag, shown_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![sid_c, path_c, start_line, end_line, tag_c, now],
                )
                .map_err(StoreError::sql)?;
                Ok(())
            })
            .await
            .map_err(call_err)
    }

    /// All shown ranges for (session, path), oldest-first.
    pub async fn reads_for(
        &self,
        session_id: &str,
        path: &str,
    ) -> Result<Vec<ReadRecord>, StoreError> {
        let (sid_c, path_c) = (session_id.to_string(), path.to_string());
        self.conn
            .call(move |c| {
                let mut stmt = c
                    .prepare(
                        "SELECT session_id, path, start_line, end_line, tag, shown_at FROM reads \
                         WHERE session_id = ?1 AND path = ?2 ORDER BY id ASC",
                    )
                    .map_err(StoreError::sql)?;
                let rows = stmt
                    .query_map(rusqlite::params![sid_c, path_c], |r| {
                        Ok(ReadRecord {
                            session_id: r.get(0)?,
                            path: r.get(1)?,
                            start_line: r.get(2)?,
                            end_line: r.get(3)?,
                            tag: r.get(4)?,
                            shown_at: r.get(5)?,
                        })
                    })
                    .map_err(StoreError::sql)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(StoreError::sql)
            })
            .await
            .map_err(call_err)
    }

    /// Current-session pointer (meta table; backing for `sessions switch`).
    pub async fn switch_session(&self, id: &str) -> Result<(), StoreError> {
        // Verify the session exists first (switching to a ghost is a UX trap).
        let sessions = self.list_sessions().await?;
        if !sessions.iter().any(|s| s.id == id) {
            return Err(StoreError::NoSession(id.into()));
        }
        let id_c = id.to_string();
        self.conn
            .call(move |c| {
                c.execute(
                    "INSERT INTO meta (k, v) VALUES ('current_session', ?1) \
                     ON CONFLICT(k) DO UPDATE SET v = excluded.v",
                    rusqlite::params![id_c],
                )
                .map_err(StoreError::sql)?;
                Ok(())
            })
            .await
            .map_err(call_err)
    }

    pub async fn current_session(&self) -> Result<Option<String>, StoreError> {
        self.conn
            .call(|c| {
                let v: rusqlite::Result<String> =
                    c.query_row("SELECT v FROM meta WHERE k = 'current_session'", [], |r| {
                        r.get(0)
                    });
                match v {
                    Ok(s) => Ok(Some(s)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(StoreError::sql(e)),
                }
            })
            .await
            .map_err(call_err)
    }

    /// Save one plan graph for a session (chunk 50, G persistence slice).
    /// Upsert: re-driving overwrites. Ghost sessions refuse (append
    /// precedent — a plan on a dead session must not silently succeed).
    /// Forks inherit the plan (chunk-51 COPY decision, like messages+reads).
    pub async fn save_plan(
        &self,
        session_id: &str,
        graph: &polymorph_tasks::TaskGraph,
    ) -> Result<(), StoreError> {
        let text = serde_json::to_string(graph).map_err(|e| StoreError::Sqlite(e.to_string()))?;
        let (sid_c, text_c) = (session_id.to_string(), text);
        let now = now_unix();
        self.conn
            .call(move |c| {
                let exists: i64 = c
                    .query_row(
                        "SELECT COUNT(*) FROM sessions WHERE id = ?1",
                        rusqlite::params![sid_c],
                        |r| r.get(0),
                    )
                    .map_err(StoreError::sql)?;
                if exists == 0 {
                    return Err(StoreError::NoSession(sid_c.clone()));
                }
                c.execute(
                    "INSERT INTO plan_state (session_id, graph_json, updated_at) VALUES (?1, ?2, ?3) \
                     ON CONFLICT(session_id) DO UPDATE SET graph_json = excluded.graph_json, updated_at = excluded.updated_at",
                    rusqlite::params![sid_c, text_c, now],
                )
                .map_err(StoreError::sql)
            })
            .await
            .map_err(call_err)?;
        Ok(())
    }

    /// Load the plan graph for a session (chunk 50).
    /// Ghost session → `NoSession` (a dead id must not read as "no plan
    /// yet" — that conflation would strand work); live session without a
    /// saved plan → `None` (absence is data, immediate-pass precedent).
    /// Corrupt JSON → hard error naming the session (recording-loader
    /// precedent — never a partial graph).
    pub async fn load_plan(
        &self,
        session_id: &str,
    ) -> Result<Option<polymorph_tasks::TaskGraph>, StoreError> {
        let sid_c = session_id.to_string();
        self.conn
            .call(move |c| {
                let exists: i64 = c
                    .query_row(
                        "SELECT COUNT(*) FROM sessions WHERE id = ?1",
                        rusqlite::params![sid_c],
                        |r| r.get(0),
                    )
                    .map_err(StoreError::sql)?;
                if exists == 0 {
                    return Err(StoreError::NoSession(sid_c.clone()));
                }
                let text: rusqlite::Result<String> = c.query_row(
                    "SELECT graph_json FROM plan_state WHERE session_id = ?1",
                    rusqlite::params![sid_c],
                    |r| r.get(0),
                );
                match text {
                    Ok(json) => {
                        let graph: polymorph_tasks::TaskGraph = serde_json::from_str(&json)
                            .map_err(|e| {
                                StoreError::Sqlite(format!(
                                    "plan_state for session {} is corrupt: {e}",
                                    sid_c
                                ))
                            })?;
                        graph.validate().map_err(|e| {
                            StoreError::Sqlite(format!(
                                "plan_state for session {} is corrupt: {e}",
                                sid_c
                            ))
                        })?;
                        Ok(Some(graph))
                    }
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(StoreError::sql(e)),
                }
            })
            .await
            .map_err(call_err)
    }

    /// `PRAGMA integrity_check` — must return exactly `ok`.
    pub async fn integrity_check(&self) -> Result<(), StoreError> {
        let rows: Vec<String> = self
            .conn
            .call(|c| {
                let mut stmt = c
                    .prepare("PRAGMA integrity_check")
                    .map_err(StoreError::sql)?;
                let rows = stmt.query_map([], |r| r.get(0)).map_err(StoreError::sql)?;
                rows.collect::<rusqlite::Result<Vec<String>>>()
                    .map_err(StoreError::sql)
            })
            .await
            .map_err(call_err)?;
        if rows == ["ok".to_string()] {
            Ok(())
        } else {
            Err(StoreError::Integrity(rows.join("; ")))
        }
    }

    /// DB + WAL + SHM sizes in bytes (C2 soak-sampler core).
    pub fn sample_db_sizes(db_path: &Path) -> DbSizes {
        let sibling = |suffix: &str| {
            let mut p = db_path.as_os_str().to_owned();
            p.push(suffix);
            std::fs::metadata(Path::new(&p))
                .map(|m| m.len())
                .unwrap_or(0)
        };
        DbSizes {
            db_bytes: std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0),
            wal_bytes: sibling("-wal"),
            shm_bytes: sibling("-shm"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DbSizes {
    pub db_bytes: u64,
    pub wal_bytes: u64,
    pub shm_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn crud_roundtrip() {
        let s = Store::open_in_memory().await.unwrap();
        let a = s.create_session("alpha").await.unwrap();
        s.append_message(&a.id, MessageRole::User, "hello")
            .await
            .unwrap();
        s.append_message(&a.id, MessageRole::Assistant, "hi")
            .await
            .unwrap();
        let h = s.history(&a.id, 0).await.unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].role, MessageRole::User);
        assert_eq!(h[1].content, "hi");
        // last-N paging
        let last = s.history(&a.id, 1).await.unwrap();
        assert_eq!(last.len(), 1);
        assert_eq!(last[0].content, "hi");
        s.rename_session(&a.id, "beta").await.unwrap();
        let list = s.list_sessions().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "beta");
        s.delete_session(&a.id).await.unwrap();
        assert!(s.list_sessions().await.unwrap().is_empty());
        // cascade: messages die with the session (FK enforced — configure()).
        assert!(s.history(&a.id, 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn missing_session_errors() {
        let s = Store::open_in_memory().await.unwrap();
        assert!(matches!(
            s.append_message("ghost", MessageRole::User, "x").await,
            Err(StoreError::NoSession(_))
        ));
        assert!(matches!(
            s.delete_session("ghost").await,
            Err(StoreError::NoSession(_))
        ));
        assert!(matches!(
            s.rename_session("ghost", "t").await,
            Err(StoreError::NoSession(_))
        ));
        assert!(matches!(
            s.switch_session("ghost").await,
            Err(StoreError::NoSession(_))
        ));
    }

    #[tokio::test]
    async fn fork_copies_messages_and_reads() {
        let s = Store::open_in_memory().await.unwrap();
        let a = s.create_session("orig").await.unwrap();
        s.append_message(&a.id, MessageRole::User, "q")
            .await
            .unwrap();
        s.record_read(&a.id, "f.rs", 1, 10, "ab12").await.unwrap();
        let b = s.fork_session(&a.id, "fork").await.unwrap();
        assert_ne!(a.id, b.id);
        let hb = s.history(&b.id, 0).await.unwrap();
        assert_eq!(hb.len(), 1);
        assert_eq!(hb[0].content, "q");
        let rb = s.reads_for(&b.id, "f.rs").await.unwrap();
        assert_eq!(rb.len(), 1);
        assert_eq!(rb[0].tag, "ab12");
        // Fork of a ghost rolls back its own empty session (no litter).
        assert!(matches!(
            s.fork_session("ghost", "x").await,
            Err(StoreError::NoSession(_))
        ));
        assert_eq!(s.list_sessions().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn current_pointer_roundtrip() {
        let s = Store::open_in_memory().await.unwrap();
        assert_eq!(s.current_session().await.unwrap(), None);
        let a = s.create_session("a").await.unwrap();
        s.switch_session(&a.id).await.unwrap();
        assert_eq!(s.current_session().await.unwrap(), Some(a.id.clone()));
        // Deletion does not clear the pointer (documented: current_session callers
        // must handle dangling ids — cheap, no trigger machinery).
        s.delete_session(&a.id).await.unwrap();
        assert_eq!(s.current_session().await.unwrap(), Some(a.id));
    }

    #[tokio::test]
    async fn read_ledger_roundtrip() {
        let s = Store::open_in_memory().await.unwrap();
        let a = s.create_session("a").await.unwrap();
        s.record_read(&a.id, "f.rs", 1, 40, "t1").await.unwrap();
        s.record_read(&a.id, "f.rs", 50, 60, "t2").await.unwrap();
        let rs = s.reads_for(&a.id, "f.rs").await.unwrap();
        assert_eq!(rs.len(), 2);
        assert_eq!((rs[0].start_line, rs[0].end_line), (1, 40));
        assert_eq!(rs[1].tag, "t2");
        assert!(s.reads_for(&a.id, "other.rs").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn bad_role_rejected() {
        assert!("systemx".parse::<MessageRole>().is_err());
    }

    #[tokio::test]
    async fn crash_reopen_stress() {
        // TBM-13 unit form: 20 cycles of {open, write, DROP WITHOUT CLOSE
        // (the in-process kill analog — no checkpoint, no graceful close),
        // reopen}. synchronous=FULL means committed rows survive; the final
        // count, order, and integrity_check prove it.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("s.db");
        let total_cycles = 20;
        let per_cycle = 5;
        for cycle in 0..total_cycles {
            let s = Store::open(&db).await.unwrap();
            // Reuse one session across cycles (created once).
            let sid = if cycle == 0 {
                s.create_session("stress").await.unwrap().id
            } else {
                s.list_sessions()
                    .await
                    .unwrap()
                    .into_iter()
                    .next()
                    .unwrap()
                    .id
            };
            for k in 0..per_cycle {
                s.append_message(&sid, MessageRole::User, &format!("c{cycle}m{k}"))
                    .await
                    .unwrap();
            }
            if cycle % 5 == 0 {
                s.integrity_check().await.unwrap();
            }
            drop(s); // <-- the "kill": no close(), no checkpoint
        }
        let s = Store::open(&db).await.unwrap();
        s.integrity_check().await.unwrap();
        let sessions = s.list_sessions().await.unwrap();
        assert_eq!(sessions.len(), 1);
        let h = s.history(&sessions[0].id, 0).await.unwrap();
        assert_eq!(h.len(), total_cycles * per_cycle);
        // Order preserved across kills.
        assert_eq!(h[0].content, "c0m0");
        assert_eq!(
            h.last().unwrap().content,
            format!("c{}m{}", total_cycles - 1, per_cycle - 1)
        );
        // Soak-sampler core rides along: sizes must be non-zero post-write.
        let sizes = Store::sample_db_sizes(&db);
        assert!(sizes.db_bytes > 0);
    }

    #[tokio::test]
    async fn backup_before_migrate_fires() {
        // ISOLATED dirs per case: backup names carry 1-second timestamps, so
        // sharing one dir across cases could collide names and false-pass.
        // A pre-existing v0 file forces migration → backup must exist first.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("s.db");
        std::fs::write(&db, b"").unwrap(); // exists, version 0
        Store::open(&db).await.unwrap();
        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .collect();
        assert_eq!(backups.len(), 1, "exactly one backup dir");
        // Fresh creates back up nothing (nothing to lose) — own dir, so any
        // backup at all fails the assertion (no timestamp games).
        let dir2 = tempfile::tempdir().unwrap();
        Store::open(&dir2.path().join("fresh.db")).await.unwrap();
        let backups2: Vec<_> = std::fs::read_dir(dir2.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .collect();
        assert!(backups2.is_empty(), "fresh create adds no backup");
    }

    #[tokio::test]
    async fn downgrade_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("s.db");
        // Plant a newer user_version directly.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
                .unwrap();
        }
        assert!(matches!(
            Store::open(&db).await,
            Err(StoreError::Downgrade { .. })
        ));
    }

    #[tokio::test]
    async fn create_with_id_round_trips_and_refuses_duplicates() {
        // Chunk 51: `plan-drive --session` initializes its slot on first
        // use, so ids are caller-chosen (there is no other CLI path that
        // creates sessions).
        let s = Store::open_in_memory().await.unwrap();
        let a = s.create_session_with_id("slot-1", "drive").await.unwrap();
        assert_eq!(a.id, "slot-1");
        let list = s.list_sessions().await.unwrap();
        assert!(list.iter().any(|x| x.id == "slot-1" && x.title == "drive"));
        assert!(s.create_session_with_id("slot-1", "again").await.is_err());
    }

    fn plan_sample() -> polymorph_tasks::TaskGraph {
        use polymorph_tasks::{Task, TaskGraph, TaskStatus};
        let mut g = TaskGraph::new();
        let mut a = Task::new("a", "First", "ship it\n", vec![]);
        a.status = TaskStatus::Done;
        g.add_task(a).unwrap();
        g.add_task(Task::new("b", "Second", "multi\nline\n", vec!["a".into()]))
            .unwrap();
        g
    }

    #[tokio::test]
    async fn plan_round_trips_exactly() {
        use polymorph_tasks::TaskStatus;
        let s = Store::open_in_memory().await.unwrap();
        let sess = s.create_session("plan").await.unwrap();
        // No plan yet: absence is data, not an error.
        assert!(s.load_plan(&sess.id).await.unwrap().is_none());
        let g = plan_sample();
        s.save_plan(&sess.id, &g).await.unwrap();
        let back = s.load_plan(&sess.id).await.unwrap().expect("saved");
        assert_eq!(back, g);
        assert_eq!(back.get("a").unwrap().status, TaskStatus::Done);
        assert_eq!(back.get("b").unwrap().deps, vec!["a".to_string()]);
        // Overwrite replaces (re-drive updates the stored state).
        let mut g2 = plan_sample();
        g2.set_status("b", TaskStatus::Failed).unwrap();
        s.save_plan(&sess.id, &g2).await.unwrap();
        let back2 = s.load_plan(&sess.id).await.unwrap().expect("saved");
        assert_eq!(back2, g2);
    }

    #[tokio::test]
    async fn plan_ghost_refuses_and_corrupt_is_hard_error() {
        let s = Store::open_in_memory().await.unwrap();
        let g = plan_sample();
        // Ghost sessions refuse both directions (never strand work on a
        // dead id — stricter than history(), stated in load_plan docs).
        assert!(matches!(
            s.save_plan("ghost", &g).await,
            Err(StoreError::NoSession(_))
        ));
        assert!(matches!(
            s.load_plan("ghost").await,
            Err(StoreError::NoSession(_))
        ));
        // Corrupt JSON is a hard error naming the session, never partial.
        let sess = s.create_session("corrupt").await.unwrap();
        s.save_plan(&sess.id, &g).await.unwrap();
        let sid_c = sess.id.clone();
        s.conn
            .call(move |c| {
                c.execute(
                    "UPDATE plan_state SET graph_json = ?1 WHERE session_id = ?2",
                    rusqlite::params!["not-json{", sid_c],
                )
                .map_err(StoreError::sql)?;
                Ok(())
            })
            .await
            .map_err(call_err)
            .unwrap();
        let err = s.load_plan(&sess.id).await.unwrap_err().to_string();
        assert!(err.contains(&sess.id), "{err}");
    }

    #[tokio::test]
    async fn plan_dies_with_its_session_and_forks_inherit() {
        let s = Store::open_in_memory().await.unwrap();
        let a = s.create_session("orig").await.unwrap();
        s.save_plan(&a.id, &plan_sample()).await.unwrap();
        // Fork-copy decision (chunk 51: COPY, like messages+reads) — a
        // forked drive resumes where the parent stood. A plan-less parent
        // still forks plan-less (absence carries over, not a phantom).
        let b = s.fork_session(&a.id, "fork").await.unwrap();
        assert_eq!(
            s.load_plan(&b.id).await.unwrap().expect("inherited"),
            plan_sample()
        );
        let c = s.create_session("empty").await.unwrap();
        let d = s.fork_session(&c.id, "fork-empty").await.unwrap();
        assert!(s.load_plan(&d.id).await.unwrap().is_none());
        // Delete cascades via FK (configure() PRAGMA, traps-#11): the plan
        // row dies with the session — asserted directly on the dead id (a
        // bare COUNT(*) would count the live fork's inherited copy).
        s.delete_session(&a.id).await.unwrap();
        let aid_c = a.id.clone();
        let leftover: i64 = s
            .conn
            .call(move |c| {
                c.query_row(
                    "SELECT COUNT(*) FROM plan_state WHERE session_id = ?1",
                    rusqlite::params![aid_c],
                    |r| r.get(0),
                )
                .map_err(StoreError::sql)
            })
            .await
            .map_err(call_err)
            .unwrap();
        assert_eq!(leftover, 0);
        // And a later load names the ghost — never a silent None on a dead id.
        assert!(matches!(
            s.load_plan(&a.id).await,
            Err(StoreError::NoSession(_))
        ));
    }

    #[tokio::test]
    async fn circuit_log_round_trips_and_latest_retrieved() {
        let s = Store::open_in_memory().await.unwrap();
        let a = s.create_session("circuit-test").await.unwrap();
        assert_eq!(s.get_latest_circuit_log(&a.id).await.unwrap(), None);

        // Append user turn without circuit
        s.append_message(&a.id, MessageRole::User, "hi")
            .await
            .unwrap();
        assert_eq!(s.get_latest_circuit_log(&a.id).await.unwrap(), None);

        // Append assistant turn with circuit log
        s.append_message_with_circuit(
            &a.id,
            MessageRole::Assistant,
            "here is the answer",
            Some("[Stage 1: Plan]\n[Stage 2: Act]"),
        )
        .await
        .unwrap();

        assert_eq!(
            s.get_latest_circuit_log(&a.id).await.unwrap().as_deref(),
            Some("[Stage 1: Plan]\n[Stage 2: Act]")
        );

        // Fork session copies circuit_log as well
        let b = s.fork_session(&a.id, "fork-circuit").await.unwrap();
        assert_eq!(
            s.get_latest_circuit_log(&b.id).await.unwrap().as_deref(),
            Some("[Stage 1: Plan]\n[Stage 2: Act]")
        );

        let hist = s.history(&b.id, 10).await.unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].circuit_log, None);
        assert_eq!(
            hist[1].circuit_log.as_deref(),
            Some("[Stage 1: Plan]\n[Stage 2: Act]")
        );
    }
}
