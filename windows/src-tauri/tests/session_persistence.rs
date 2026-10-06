// Session persistence, from the island's point of view.
//
// A fake OpenAI-compatible server stands in for the real provider, so the whole
// path — load the session, send, save the diff — runs for real without an API
// key and without spending a token. What is being tested is the persistence,
// not the provider.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use coucou_lib::testing_store::ChatStore;
use serde_json::{json, Value};

/// A one-shot HTTP server: answers every request with the same JSON, on a port
/// the OS picked, so nothing collides with anything else running.
struct FakeProvider {
    port: u16,
    seen: Arc<Mutex<Vec<Value>>>,
}

impl FakeProvider {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("porta livre");
        let port = listener.local_addr().expect("endereco").port();
        let seen = Arc::new(Mutex::new(Vec::new()));

        let sink = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(match stream.try_clone() {
                    Ok(s) => s,
                    Err(_) => continue,
                });

                // Read the request: headers, then the body length.
                let mut content_length = 0usize;
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    let trimmed = line.trim_end();
                    if trimmed.is_empty() {
                        break;
                    }
                    if let Some(value) = trimmed
                        .strip_prefix("content-length:")
                        .or_else(|| trimmed.strip_prefix("Content-Length:"))
                    {
                        content_length = value.trim().parse().unwrap_or(0);
                    }
                    line.clear();
                }
                let mut body = vec![0u8; content_length];
                let _ = std::io::Read::read_exact(&mut reader, &mut body);
                if let Ok(value) = serde_json::from_slice::<Value>(&body) {
                    sink.lock().unwrap().push(value);
                }

                // OpenAI's reply shape: one choice, one message.
                let reply = json!({
                    "choices": [{ "message": { "role": "assistant", "content": "lembrado" } }]
                });
                let payload = reply.to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                let _ = stream.flush();
            }
        });

        Self { port, seen }
        }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// What the server was asked, so a test can prove the history went with it.
    fn requests(&self) -> Vec<Value> {
        self.seen.lock().unwrap().clone()
    }
}

/// A store in its own directory, wiped on entry.
fn store_for(name: &str) -> ChatStore {
    let dir = std::env::temp_dir().join(format!("coucou-e2e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir for this test");
    ChatStore::open_at(&dir.join("sessions.db")).expect("store opens")
}

/// The same load → send → save-difference the `chat_send` command does.
async fn turn(
    store: &ChatStore,
    session_id: &str,
    provider: &str,
    base_url: &str,
    history: &mut Vec<Value>,
    query: &str,
) -> Result<String, String> {
    let already_saved = history.len();
    let outcome = coucou_lib::testing_clients::openai_send(
        history,
        "fake-model",
        base_url,
        "fake-key",
        query.to_string(),
        None,
    )
    .await;

    for message in history.iter().skip(already_saved) {
        store.append(session_id, provider, message, None, None)?;
    }
    outcome.map(|r| r.text)
}

#[tokio::test]
async fn a_question_and_its_answer_survive_a_restart() {
    let server = FakeProvider::start();
    let dir = std::env::temp_dir().join(format!("coucou-e2e-restart-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sessions.db");

    let session_id;
    {
        let store = ChatStore::open_at(&path).unwrap();
        session_id = store.new_session("C:\\work").unwrap();
        let mut history = Vec::new();
        let answer = turn(
            &store,
            &session_id,
            "openai",
            &server.base_url(),
            &mut history,
            "qual era o nome do servidor?",
        )
        .await
        .expect("a resposta chega");

        assert_eq!(answer, "lembrado");
        assert_eq!(store.message_count(&session_id).unwrap(), 2, "pergunta e resposta");
    }

    // The app restarting: a brand new store and a brand new history, same file.
    let reborn = ChatStore::open_at(&path).unwrap();
    let resumed = reborn.resume_or_create("C:\\outro").unwrap();
    assert_eq!(resumed, session_id, "não retomou a sessão");

    let history = reborn.load_session(&resumed).unwrap();
    assert_eq!(history.len(), 2);
    let said = history[0]["content"].as_str().unwrap();
    assert!(said.contains("qual era o nome do servidor?"), "{said}");
    assert_eq!(history[1]["content"], "lembrado");
}

#[tokio::test]
async fn the_second_question_carries_the_first_one() {
    let server = FakeProvider::start();
    let store = store_for("carry");
    let id = store.new_session("C:\\work").unwrap();

    let mut history = Vec::new();
    turn(&store, &id, "openai", &server.base_url(), &mut history, "primeira").await.unwrap();
    turn(&store, &id, "openai", &server.base_url(), &mut history, "segunda")
        .await
        .unwrap();

    // The proof is in what went over the wire: the second request must contain
    // both turns.
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let second = requests[1]["messages"].as_array().expect("messages");
    let roles: Vec<&str> = second
        .iter()
        .filter_map(|m| m["role"].as_str())
        .collect();
    assert!(
        roles.contains(&"user"),
        "a segunda requisicao perdeu o historico: {roles:?}"
    );
    let text: Vec<String> = second
        .iter()
        .filter_map(|m| m["content"].as_str().map(str::to_string))
        .collect();
    assert!(
        text.iter().any(|t| t.contains("primeira")),
        "a primeira pergunta nao foi: {text:?}"
    );
    assert!(text.iter().any(|t| t.contains("segunda")), "{text:?}");
}

#[tokio::test]
async fn two_sessions_stay_apart_across_two_restarts() {
    let server = FakeProvider::start();
    let dir = std::env::temp_dir().join(format!("coucou-e2e-two-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sessions.db");

    let (first, second);
    {
        let store = ChatStore::open_at(&path).unwrap();
        first = store.new_session("C:\\a").unwrap();
        let mut history = Vec::new();
        turn(&store, &first, "openai", &server.base_url(), &mut history, "sobre o projeto a")
            .await
            .unwrap();

        // chat_reset: a new conversation, the old one kept.
        second = store.new_session("C:\\b").unwrap();
        let mut history = Vec::new();
        turn(&store, &second, "openai", &server.base_url(), &mut history, "sobre o projeto b")
            .await
            .unwrap();
    }

    let reborn = ChatStore::open_at(&path).unwrap();
    let a = reborn.load_session(&first).unwrap();
    let b = reborn.load_session(&second).unwrap();

    assert_eq!(a.len(), 2);
    assert_eq!(b.len(), 2);
    assert!(a[0]["content"].as_str().unwrap().contains("projeto a"));
    assert!(!a.iter().any(|m| m["content"].as_str().unwrap_or("").contains("projeto b")));
    assert!(b[0]["content"].as_str().unwrap().contains("projeto b"));
    assert!(!b.iter().any(|m| m["content"].as_str().unwrap_or("").contains("projeto a")));
}

#[tokio::test]
async fn search_finds_yesterdays_answer_after_a_restart() {
    let server = FakeProvider::start();
    let dir = std::env::temp_dir().join(format!("coucou-e2e-fts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sessions.db");

    let id;
    {
        let store = ChatStore::open_at(&path).unwrap();
        id = store.new_session("C:\\work").unwrap();
        let mut history = Vec::new();
        turn(
            &store,
            &id,
            "openai",
            &server.base_url(),
            &mut history,
            "a senha do postgres e a do kubernetes",
        )
        .await
        .unwrap();
    }

    let reborn = ChatStore::open_at(&path).unwrap();
    let hits = reborn.search_fts("kubernetes", 10).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].session_id, id);
    assert!(
        hits[0].text.contains("kubernetes"),
        "o trecho não traz a palavra procurada: {}",
        hits[0].text
    );
    assert!(reborn.search_fts("redis", 10).unwrap().is_empty());
}

#[tokio::test]
async fn a_failed_turn_leaves_the_history_exactly_as_it_was() {
    // The provider is not listening. A turn that never reached the model must not
    // be half-saved: the next question goes out with the history the model has
    // actually seen, not with a stray unanswered question in front of it.
    let store = store_for("failed");
    let id = store.new_session("C:\\work").unwrap();

    let mut history = vec![json!({ "role": "user", "content": "pergunta que ficou" })];
    store.append(&id, "openai", &history[0], None, None).unwrap();

    let failed = coucou_lib::testing_clients::openai_send(
        &mut history,
        "fake-model",
        "http://127.0.0.1:1",
        "k",
        "segunda".into(),
        None,
    )
    .await;
    assert!(failed.is_err(), "esperava falha contra porta fechada");

    let already_saved = 1;
    let appended = history.len() - already_saved;
    for message in history.iter().skip(already_saved) {
        store.append(&id, "openai", message, None, None).unwrap();
    }
    assert_eq!(appended, 0, "a tentativa falhada nao devia deixar rastro");

    let saved = store.load_session(&id).unwrap();
    assert_eq!(saved.len(), 1, "o historico anterior foi mexido");
    assert!(saved[0]["content"].as_str().unwrap().contains("pergunta que ficou"));
}