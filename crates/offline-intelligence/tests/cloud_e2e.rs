//! On Cloud, end to end against a mock OpenAI-compatible provider.
//!
//! A tiny HTTP server on 127.0.0.1 plays the provider. The real LLM worker is
//! switched On Cloud at it, and every model call the app makes goes through
//! the same entry points the chat, agents and drafting use:
//!   * the request body each provider is sent (OpenAI's newer parameters),
//!   * the bearer key on every request,
//!   * reasoning fields streamed into <think>…</think> (shown as Thoughts),
//!   * a provider error mid-stream ends the answer with that error,
//!   * a rejected key fails with the provider's own message,
//!   * a character split across network chunks arrives whole,
//!   * non-streaming calls (titles, reading notes) use the cloud model,
//!   * switching back to On Computer leaves the cloud alone.

use std::sync::{Arc, Mutex};

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use futures_util::StreamExt;
use offline_intelligence::cloud_providers::CloudProvider;
use offline_intelligence::memory::Message;
use offline_intelligence::worker_threads::llm_worker::ActiveCloud;
use offline_intelligence::worker_threads::LLMWorker;
use serde_json::{json, Value};

#[derive(Clone, Default)]
struct Seen {
    body: Arc<Mutex<Option<Value>>>,
    auth: Arc<Mutex<Option<String>>>,
}

fn sse(frames: Vec<Vec<u8>>) -> Response {
    // Each frame is sent as its own chunk, with a pause, so chunk boundaries
    // really fall where the test puts them.
    let stream = futures_util::stream::iter(frames).then(|f| async move {
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        Ok::<_, std::io::Error>(f)
    });
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

fn delta(field: &str, text: &str) -> Vec<u8> {
    format!("data: {}\n\n", json!({ "choices": [{ "index": 0, "delta": { field: text } }] })).into_bytes()
}

async fn chat(State(seen): State<Seen>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    *seen.auth.lock().unwrap() = headers.get("authorization").and_then(|h| h.to_str().ok()).map(str::to_string);
    *seen.body.lock().unwrap() = Some(body.clone());
    let model = body["model"].as_str().unwrap_or("").to_string();
    let stream = body["stream"].as_bool().unwrap_or(false);

    if model == "bad-key" {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "error": { "message": "Incorrect API key provided." } }))).into_response();
    }
    if !stream {
        return Json(json!({ "choices": [{ "index": 0, "message": { "role": "assistant", "content": "A short title" } }] })).into_response();
    }
    match model.as_str() {
        "reasoner" => sse(vec![
            delta("reasoning_content", "Weigh the clause. "),
            delta("reasoning_content", "Then answer."),
            delta("content", "The clause is "),
            delta("content", "valid."),
            b"data: [DONE]\n\n".to_vec(),
        ]),
        "failing" => sse(vec![
            delta("content", "Partial "),
            format!("data: {}\n\n", json!({ "error": { "message": "Rate limit reached" } })).into_bytes(),
        ]),
        "unicode" => {
            // "Café §" with the é (C3 A9) split across two chunks.
            let frame = delta("content", "Café §");
            let cut = frame.windows(2).position(|w| w == [0xC3, 0xA9]).unwrap() + 1;
            sse(vec![frame[..cut].to_vec(), frame[cut..].to_vec(), b"data: [DONE]\n\n".to_vec()])
        }
        _ => sse(vec![delta("content", "Hello"), b"data: [DONE]\n\n".to_vec()]),
    }
}

async fn mock_provider() -> (String, Seen) {
    let seen = Seen::default();
    let app = Router::new().route("/v1/chat/completions", post(chat)).with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{}/v1", addr), seen)
}

fn provider(id: &'static str, base: &str) -> &'static CloudProvider {
    Box::leak(Box::new(CloudProvider { id, name: "Mock", base_url: Box::leak(base.to_string().into_boxed_str()) }))
}

fn worker_on_cloud(p: &'static CloudProvider, model: &str) -> LLMWorker {
    let w = LLMWorker::new_with_backend("http://127.0.0.1:9".into());
    w.set_cloud(Some(ActiveCloud { provider: p, key: "sk-test-123".into(), model: model.into() }));
    w
}

fn msgs() -> Vec<Message> {
    vec![Message { role: "user".into(), content: "Is the clause valid?".into() }]
}

/// Run a stream to the end: the text the app would show, or the error.
async fn collect(w: &LLMWorker) -> Result<String, String> {
    let stream = w.stream_response(msgs(), 256, 0.7).await.map_err(|e| e.to_string())?;
    futures_util::pin_mut!(stream);
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        let line = item.map_err(|e| e.to_string())?;
        let data = line.trim().trim_start_matches("data: ");
        if data == "[DONE]" {
            break;
        }
        let v: Value = serde_json::from_str(data).unwrap();
        text.push_str(v["choices"][0]["delta"]["content"].as_str().unwrap_or(""));
    }
    Ok(text)
}

#[tokio::test]
async fn openai_gets_new_parameters_and_the_key() {
    let (base, seen) = mock_provider().await;
    let w = worker_on_cloud(provider("openai", &base), "gpt-5-mini");
    assert_eq!(collect(&w).await.unwrap(), "Hello");
    let body = seen.body.lock().unwrap().clone().unwrap();
    assert_eq!(body["model"], "gpt-5-mini");
    assert_eq!(body["max_completion_tokens"], 256);
    assert!(body.get("max_tokens").is_none(), "OpenAI rejects max_tokens on current models");
    assert!(body.get("temperature").is_none(), "GPT-5 accepts only the default temperature");
    assert_eq!(seen.auth.lock().unwrap().as_deref(), Some("Bearer sk-test-123"));
}

#[tokio::test]
async fn other_providers_get_the_classic_parameters() {
    let (base, seen) = mock_provider().await;
    let w = worker_on_cloud(provider("groq", &base), "llama-3.3-70b-versatile");
    collect(&w).await.unwrap();
    let body = seen.body.lock().unwrap().clone().unwrap();
    assert_eq!(body["max_tokens"], 256);
    assert!((body["temperature"].as_f64().unwrap() - 0.7).abs() < 1e-6);
    assert!(body.get("max_completion_tokens").is_none());
}

#[tokio::test]
async fn reasoning_streams_into_thoughts() {
    let (base, _) = mock_provider().await;
    let w = worker_on_cloud(provider("deepseek", &base), "reasoner");
    assert_eq!(
        collect(&w).await.unwrap(),
        "<think>Weigh the clause. Then answer.</think>The clause is valid."
    );
}

#[tokio::test]
async fn provider_error_mid_stream_is_reported() {
    let (base, _) = mock_provider().await;
    let w = worker_on_cloud(provider("openrouter", &base), "failing");
    let err = collect(&w).await.unwrap_err();
    assert!(err.contains("Rate limit reached"), "got: {err}");
}

#[tokio::test]
async fn rejected_key_gives_the_providers_message() {
    let (base, _) = mock_provider().await;
    let w = worker_on_cloud(provider("openai", &base), "bad-key");
    let err = collect(&w).await.unwrap_err();
    assert!(err.contains("401") && err.contains("Incorrect API key provided."), "got: {err}");
}

#[tokio::test]
async fn split_characters_arrive_whole() {
    let (base, _) = mock_provider().await;
    let w = worker_on_cloud(provider("mistral", &base), "unicode");
    assert_eq!(collect(&w).await.unwrap(), "Café §");
}

#[tokio::test]
async fn non_streaming_calls_use_the_cloud_model() {
    let (base, seen) = mock_provider().await;
    let w = worker_on_cloud(provider("openai", &base), "gpt-4.1");
    let out = w.generate_with(msgs(), 32, 0.2).await.unwrap();
    assert_eq!(out, "A short title");
    let body = seen.body.lock().unwrap().clone().unwrap();
    assert_eq!(body["stream"], false);
    assert_eq!(body["max_completion_tokens"], 32);
    assert!((body["temperature"].as_f64().unwrap() - 0.2).abs() < 1e-6, "gpt-4.1 keeps temperature");
}

#[tokio::test]
async fn on_computer_does_not_touch_the_cloud() {
    let (base, seen) = mock_provider().await;
    let w = worker_on_cloud(provider("openai", &base), "gpt-4.1");
    w.set_cloud(None);
    assert!(w.cloud().is_none());
    // No local engine in this test, so the local path refuses; the point is
    // that nothing reached the provider.
    assert!(w.stream_response(msgs(), 16, 0.7).await.is_err());
    assert!(seen.body.lock().unwrap().is_none());
}
