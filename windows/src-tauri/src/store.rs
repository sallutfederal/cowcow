// Chat history on disk, so a conversation survives the app being closed.
//
// One SQLite file at `%LOCALAPPDATA%\Coucou\sessions.db`, opened with the
// bundled SQLite — no runtime to install, no DLL to ship.
//
// Two columns for the message body, and the reason is the tool blocks:
//
//   content      the whole message as JSON, so a turn reloads with its
//                tool_use / tool_result structure intact and can go straight
//                back to the API;
//   content_text the same message flattened to plain prose, which is what
//                FTS5 indexes. Indexing the JSON would make a search for
//                "tool_use" or "content" match every single turn.
//
// One write, two views: the JSON is the truth, the text is the index over it.

use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use serde_json::Value;

use crate::settings;

/// Unix seconds. Only ever compared and ordered, never shown to the user.
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The database. One connection behind a mutex: the app is single-user and the
/// writes are tiny, so a pool would be machinery without a job.
pub struct ChatStore {
    conn: Mutex<Connection>,
}

impl ChatStore {
    /// Opens (creating if needed) the session database and applies the schema.
    pub fn open() -> Result<Self, String> {
        let dir = settings::local_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        Self::open_at(&dir.join("sessions.db"))
    }

    /// Opens a database at an explicit path. Tests use this; the app uses
    /// [`ChatStore::open`].
    pub fn open_at(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    /// The schema, applied idempotently.
    fn migrate(&self) -> Result<(), String> {
        let conn = self.lock()?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| format!("schema: {e}"))
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, String> {
        match self.conn.lock() {
            Ok(guard) => Ok(guard),
            // A poisoned lock means a previous statement panicked mid-write.
            // SQLite rolled that one statement back, so the database is still
            // consistent and carrying on beats refusing to start.
            Err(poisoned) => Ok(poisoned.into_inner()),
        }
    }

    /// Starts a session and returns its id.
    pub fn new_session(&self, cwd: &str) -> Result<String, String> {
        let id = uuid::Uuid::new_v4().to_string();
        let stamp = now();
        self.lock()?
            .execute(
                "INSERT INTO sessions (id, title, cwd, created_at, last_seen) VALUES (?1, NULL, ?2, ?3, ?3)",
                params![id, cwd, stamp],
            )
            .map_err(|e| format!("nova sessão: {e}"))?;
        Ok(id)
    }

    /// The session with the most recent activity, if there is one.
    pub fn last_session(&self) -> Result<Option<String>, String> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT id FROM sessions ORDER BY last_seen DESC LIMIT 1")
            .map_err(|e| e.to_string())?;
        let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
        match rows.next().map_err(|e| e.to_string())? {
            Some(row) => Ok(Some(row.get::<_, String>(0).map_err(|e| e.to_string())?)),
            None => Ok(None),
        }
    }

    /// The session to resume at startup: the last one, or a fresh one.
    pub fn resume_or_create(&self, cwd: &str) -> Result<String, String> {
        match self.last_session()? {
            Some(id) => Ok(id),
            None => self.new_session(cwd),
        }
    }

    /// The whole conversation, ready to go back to the API.
    pub fn load_session(&self, id: &str) -> Result<Vec<Value>, String> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT content FROM messages WHERE session_id = ?1 ORDER BY id ASC")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![id], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?;

        let mut out = Vec::new();
        for row in rows {
            let raw = row.map_err(|e| e.to_string())?;
            // A row we cannot parse is dropped rather than failing the load: one
            // bad message should not cost the user the whole conversation.
            match serde_json::from_str::<Value>(&raw) {
                Ok(value) => out.push(value),
                Err(e) => crate::log::line(format!("sessão {id}: mensagem ilegível ({e})")),
            }
        }
        Ok(out)
    }

    /// Stores one message, JSON and all.
    pub fn append(
        &self,
        session_id: &str,
        provider: &str,
        message: &Value,
        tokens_in: Option<i64>,
        tokens_out: Option<i64>,
    ) -> Result<(), String> {
        let content = serde_json::to_string(message).map_err(|e| e.to_string())?;
        let text = flatten(message);
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        let stamp = now();

        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO messages (session_id, provider, role, content, content_text, tokens_in, tokens_out, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![session_id, provider, role, content, text, tokens_in, tokens_out, stamp],
        )
        .map_err(|e| format!("append: {e}"))?;
        conn.execute(
            "UPDATE sessions SET last_seen = ?2 WHERE id = ?1",
            params![session_id, stamp],
        )
        .map_err(|e| format!("last_seen: {e}"))?;
        Ok(())
    }

    /// The title of a session, shown wherever a chat is listed.
    pub fn set_title(&self, session_id: &str, title: &str) -> Result<(), String> {
        self.lock()?
            .execute(
                "UPDATE sessions SET title = ?2 WHERE id = ?1",
                params![session_id, title],
            )
            .map_err(|e| format!("título: {e}"))?;
        Ok(())
    }

    /// Every session, most recent first.
    pub fn list_sessions(&self) -> Result<Vec<Session>, String> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, title, cwd, created_at, last_seen FROM sessions ORDER BY last_seen DESC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Session {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    cwd: row.get(2)?,
                    created_at: row.get(3)?,
                    last_seen: row.get(4)?,
                })
            })
            .map_err(|e| e.to_string())?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| e.to_string())?);
        }
        Ok(out)
    }

    /// Full-text search across every session, newest first.
    pub fn search_fts(&self, query: &str, limit: usize) -> Result<Vec<Hit>, String> {
        let conn = self.lock()?;
        // FTS5 matches on bare words; quoting keeps a stray quote or hyphen in
        // the question from being read as query syntax.
        let needle = format!("\"{}\"", query.replace('"', "\"\""));
        let mut stmt = conn
            .prepare(
                "SELECT m.session_id, m.role, m.content_text, m.created_at
                 FROM messages_fts f
                 JOIN messages m ON m.id = f.rowid
                 WHERE messages_fts MATCH ?1
                 ORDER BY m.created_at DESC
                 LIMIT ?2",
            )
            .map_err(|e| format!("fts: {e}"))?;

        let rows = stmt
            .query_map(params![needle, limit as i64], |row| {
                Ok(Hit {
                    session_id: row.get(0)?,
                    role: row.get(1)?,
                    text: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })
            .map_err(|e| e.to_string())?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| e.to_string())?);
        }
        Ok(out)
    }

    /// How many messages a session holds.
    pub fn message_count(&self, session_id: &str) -> Result<i64, String> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())
    }

    /// Whether an FTS5 index is really there and working.
    ///
    /// The schema creates it, so this only answers "did that actually happen",
    /// which is worth knowing before promising the user a search box.
    pub fn fts_available(&self) -> bool {
        let Ok(conn) = self.lock() else {
            return false;
        };
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='messages_fts'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n == 1)
        .unwrap_or(false)
    }
}

/// One saved conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub id: String,
    pub title: Option<String>,
    pub cwd: String,
    pub created_at: i64,
    pub last_seen: i64,
}

/// One search result: the session it came from and the text that matched.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub session_id: String,
    pub role: String,
    pub text: String,
    pub created_at: i64,
}

/// Turns a message into the prose a human would recognise.
///
/// Plain string content is itself. A block list contributes its `text` blocks,
/// plus a word for each tool the turn used or answered — enough that searching
/// for a tool name finds the turn that called it, without the JSON noise.
pub fn flatten(message: &Value) -> String {
    let mut out: Vec<String> = Vec::new();

    match message.get("content") {
        Some(Value::String(text)) => out.push(text.clone()),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            out.push(text.to_string());
                        }
                    }
                    Some("tool_use") => {
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                        out.push(format!("[usou a ferramenta {name}]"));
                    }
                    Some("tool_result") => {
                        let content = block.get("content").and_then(Value::as_str).unwrap_or("");
                        out.push(format!("[resultado da ferramenta] {content}"));
                    }
                    Some("server_tool_use") => {
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                        out.push(format!("[pesquisou na web: {name}]"));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    out.retain(|part| !part.trim().is_empty());
    out.join("\n")
}

/// Trims a search result so a stored message does not come back as a wall.
pub fn excerpt(text: &str, needle: &str, radius: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let lower: String = text.to_lowercase();
    let haystack: Vec<char> = lower.chars().collect();
    let pattern: Vec<char> = needle.to_lowercase().chars().collect();

    let found = pattern.is_empty() || haystack.windows(pattern.len()).any(|w| w == pattern.as_slice());
    if !found || chars.len() <= radius * 2 {
        return text.chars().take(radius * 2).collect();
    }

    let at = haystack
        .windows(pattern.len())
        .position(|w| w == pattern.as_slice())
        .unwrap_or(0);
    let start = at.saturating_sub(radius);
    let end = (at + pattern.len() + radius).min(chars.len());
    let mut out: String = chars[start..end].iter().collect();
    if start > 0 {
        out.insert(0, '…');
    }
    if end < chars.len() {
        out.push('…');
    }
    out
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
  id TEXT PRIMARY KEY,
  title TEXT,
  cwd TEXT,
  created_at INTEGER NOT NULL,
  last_seen INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS messages (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  provider TEXT NOT NULL,
  role TEXT NOT NULL,
  content TEXT NOT NULL,
  content_text TEXT NOT NULL DEFAULT '',
  tokens_in INTEGER,
  tokens_out INTEGER,
  created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_msg ON messages(session_id, created_at);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
  content_text,
  session_id UNINDEXED,
  created_at UNINDEXED,
  content='messages',
  content_rowid='id'
);

CREATE TRIGGER IF NOT EXISTS msg_ai AFTER INSERT ON messages BEGIN
  INSERT INTO messages_fts(rowid, content_text, session_id, created_at)
  VALUES (new.id, new.content_text, new.session_id, new.created_at);
END;

CREATE TRIGGER IF NOT EXISTS msg_ad AFTER DELETE ON messages BEGIN
  INSERT INTO messages_fts(messages_fts, rowid, content_text, session_id, created_at)
  VALUES ('delete', old.id, old.content_text, old.session_id, old.created_at);
END;
"#;

/// The store, for the integration tests in `tests/`.
///
/// The modules of this crate are private, so a test outside cannot reach the
/// type. This exposes exactly what those tests need and nothing else.
#[doc(hidden)]
pub mod testing_store {
    pub use super::{flatten, ChatStore, Hit, Session};
}
    /// The OpenAI-compatible client, for the integration tests in `tests/`.
#[doc(hidden)]
pub mod testing_clients {
    pub use crate::claude::{ChatContext, ChatReply};

    /// One turn against an OpenAI-compatible endpoint.
    pub async fn openai_send(
        history: &mut Vec<serde_json::Value>,
        model: &str,
        base_url: &str,
        api_key: &str,
        query: String,
        context: Option<ChatContext>,
    ) -> Result<ChatReply, String> {
        crate::openai::send(history, model, base_url, api_key, query, context).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A store in this test's own directory, wiped on entry.
    fn store_for(name: &str) -> ChatStore {
        let dir = std::env::temp_dir().join(format!("coucou-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir for this test");
        ChatStore::open_at(&dir.join("sessions.db")).expect("store opens")
    }

    fn user(text: &str) -> Value {
        json!({ "role": "user", "content": [{ "type": "text", "text": text }] })
    }

    fn assistant(text: &str) -> Value {
        json!({ "role": "assistant", "content": [{ "type": "text", "text": text }] })
    }

    #[test]
    fn the_schema_lands_with_a_working_fts5() {
        let store = store_for("schema");
        assert!(store.fts_available(), "FTS5 nao foi criado");

        let conn = store.lock().unwrap();
        let tables: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type IN ('table','index','trigger')",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(std::result::Result::unwrap)
            .collect();
        for expected in ["sessions", "messages", "idx_msg", "messages_fts", "msg_ai", "msg_ad"] {
            assert!(tables.contains(&expected.to_string()), "faltou {expected}");
        }
    }

    #[test]
    fn opening_twice_does_not_erase_anything() {
        let dir = std::env::temp_dir().join(format!("coucou-store-reopen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.db");

        let first = ChatStore::open_at(&path).unwrap();
        let id = first.new_session("C:\\work").unwrap();
        first.append(&id, "anthropic", &user("olá"), None, None).unwrap();
        drop(first);

        let second = ChatStore::open_at(&path).unwrap();
        assert_eq!(second.load_session(&id).unwrap().len(), 1, "a sessão sumiu");
    }

    #[test]
    fn a_session_round_trips_with_its_tool_blocks_intact() {
        let store = store_for("round_trip");
        let id = store.new_session("C:\\work").unwrap();

        store
            .append(
                &id,
                "anthropic",
                &json!({
                    "role": "assistant",
                    "content": [
                        { "type": "text", "text": "vou ler" },
                        { "type": "tool_use", "id": "tu_1", "name": "read_file", "input": { "path": "a.rs" } },
                    ],
                }),
                Some(120),
                Some(40),
            )
            .unwrap();
        store
            .append(
                &id,
                "anthropic",
                &json!({
                    "role": "user",
                    "content": [
                        { "type": "tool_result", "tool_use_id": "tu_1", "content": "fn main() {}", "is_error": false },
                    ],
                }),
                None,
                None,
            )
            .unwrap();

        let loaded = store.load_session(&id).unwrap();
        assert_eq!(loaded.len(), 2);
        // The structure has to come back exactly, or the next API call breaks.
        let blocks = loaded[0]["content"].as_array().unwrap();
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["name"], "read_file");
        assert_eq!(blocks[1]["input"]["path"], "a.rs");
        assert_eq!(loaded[1]["content"][0]["tool_use_id"], "tu_1");
        assert_eq!(loaded[1]["content"][0]["content"], "fn main() {}");
    }

    #[test]
    fn two_sessions_never_mix() {
        let store = store_for("isolated");
        let a = store.new_session("C:\\a").unwrap();
        let b = store.new_session("C:\\b").unwrap();
        store.append(&a, "anthropic", &user("segredo de a"), None, None).unwrap();
        store.append(&b, "anthropic", &user("segredo de b"), None, None).unwrap();

        let loaded_a = store.load_session(&a).unwrap();
        assert_eq!(loaded_a.len(), 1);
        assert!(loaded_a[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("segredo de a"));
        assert_eq!(store.message_count(&a).unwrap(), 1);
        assert_eq!(store.message_count(&b).unwrap(), 1);
    }

    #[test]
    fn closing_and_reopening_finds_the_conversation_again() {
        let dir = std::env::temp_dir().join(format!("coucou-store-restart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.db");

        let store = ChatStore::open_at(&path).unwrap();
        let id = store.new_session("C:\\work").unwrap();
        store.append(&id, "anthropic", &user("o que a gente falou antes?"), None, None).unwrap();
        drop(store);

        // This is the app restarting: a brand new store, same file.
        let reborn = ChatStore::open_at(&path).unwrap();
        let resumed = reborn.resume_or_create("C:\\work").unwrap();
        assert_eq!(resumed, id, "não retomou a última sessão");
        let history = reborn.load_session(&resumed).unwrap();
        assert!(history[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("o que a gente falou antes?"));
    }

    #[test]
    fn search_finds_yesterdays_words_across_sessions() {
        let store = store_for("fts");
        let old = store.new_session("C:\\w").unwrap();
        store
            .append(&old, "anthropic", &user("lembre do servidor postgres na(aws)"), None, None)
            .unwrap();
        store.append(&old, "anthropic", &assistant("anotado"), None, None).unwrap();

        let hits = store.search_fts("postgres", 10).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].session_id, old);
        assert_eq!(hits[0].role, "user");
        assert!(hits[0].text.contains("postgres"));

        // A word nobody said finds nothing.
        assert!(store.search_fts("kubernetes", 10).unwrap().is_empty());
    }

    #[test]
    fn the_index_reads_prose_and_not_the_json_envelope() {
        let store = store_for("fts_json");
        let id = store.new_session("C:\\w").unwrap();
        store.append(&id, "anthropic", &user("uma nota qualquer"), None, None).unwrap();

        // Every stored message contains these words in its JSON envelope. If
        // FTS5 were indexing `content`, these would match.
        for noise in ["tool_use", "content_text", "json!"] {
            assert!(
                store.search_fts(noise, 10).unwrap().is_empty(),
                "'{noise}' não deveria aparecer na busca"
            );
        }
    }

    #[test]
    fn a_tool_call_is_findable_by_the_tool_name() {
        let store = store_for("fts_tool");
        let id = store.new_session("C:\\w").unwrap();
        store
            .append(
                &id,
                "anthropic",
                &json!({
                    "role": "assistant",
                    "content": [
                        { "type": "text", "text": "vou olhar" },
                        { "type": "tool_use", "id": "t", "name": "grep", "input": {} },
                    ],
                }),
                None,
                None,
            )
            .unwrap();

        let hits = store.search_fts("grep", 10).unwrap();
        assert_eq!(hits.len(), 1, "o nome da ferramenta deveria ser buscável");
    }

    #[test]
    fn listing_and_titling_work() {
        let store = store_for("list");
        let id = store.new_session("C:\\projeto").unwrap();
        store.set_title(&id, "sobre o agente").unwrap();

        let sessions = store.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title.as_deref(), Some("sobre o agente"));
        assert_eq!(sessions[0].cwd, "C:\\projeto");
    }

    #[test]
    fn a_query_full_of_punctuation_is_not_query_syntax() {
        let store = store_for("fts_quotes");
        let id = store.new_session("C:\\w").unwrap();
        store.append(&id, "anthropic", &user("o valor é \"exato\""), None, None).unwrap();

        // A stray quote would otherwise be a syntax error, not a search.
        assert!(store.search_fts("\"exato\"", 10).is_ok());
        assert_eq!(store.search_fts("exato", 10).unwrap().len(), 1);
    }

    #[test]
    fn flattening_covers_every_block_kind_a_turn_can_carry() {
        assert_eq!(flatten(&json!({ "role": "user", "content": "oi" })), "oi");

        let with_tools = json!({
            "role": "assistant",
            "content": [
                { "type": "text", "text": "vou ver" },
                { "type": "tool_use", "id": "t", "name": "read_file", "input": {} },
                { "type": "server_tool_use", "id": "s", "name": "web_search" },
            ],
        });
        let flat = flatten(&with_tools);
        assert!(flat.contains("vou ver"));
        assert!(flat.contains("read_file"));
        assert!(flat.contains("web_search"));

        let result = json!({
            "role": "user",
            "content": [{ "type": "tool_result", "tool_use_id": "t", "content": "conteudo", "is_error": false }],
        });
        assert!(flatten(&result).contains("conteudo"));

        assert_eq!(flatten(&json!({ "role": "user", "content": [] })), "");
    }

    #[test]
    fn an_excerpt_shows_the_match_with_room_around_it() {
        let text = format!(
            "{}alvo no meio{}",
            " ".repeat(200),
            " ".repeat(200)
        );
        let cut = excerpt(&text, "alvo", 20);
        assert!(cut.contains("alvo no meio"), "{cut}");
        assert!(cut.starts_with('…'), "esperava reticencias na frente");
        assert!(cut.chars().count() < 60, "o excerpt nao cortou: {}", cut.len());
    }
}