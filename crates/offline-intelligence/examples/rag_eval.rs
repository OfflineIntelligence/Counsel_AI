//! R0 retrieval evaluation with the real embedding model.
//!
//! Synthetic multilingual corpus (always):
//!   cargo run --release --example rag_eval
//! Your own documents and golden questions:
//!   cargo run --release --example rag_eval -- --corpus <dir> --golden <questions.jsonl>
//! Golden line: {"question": "...", "document": "file.pdf", "expected_pages": [47]}

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use offline_intelligence::memory_db::{MemoryDatabase, NewDocument};
use offline_intelligence::rag::{self, embedder::LlamaEmbedder, eval, search::RetrieveOptions, RagRuntime};

#[derive(serde::Deserialize)]
struct Golden {
    question: String,
    document: String,
    expected_pages: Vec<i32>,
}

struct Case {
    question: String,
    document: String,
    pages: Vec<i32>,
}

async fn score(db: &MemoryDatabase, rt: &RagRuntime, docs: &[i64], cases: &[Case], router: bool) -> eval::Metrics {
    let scope: HashSet<i64> = docs.iter().copied().collect();
    let mut m = eval::Metrics::default();
    for c in cases {
        let opts = RetrieveOptions { budget_bytes: 20_000, use_router: router, ..Default::default() };
        let started = Instant::now();
        let r = rag::search::retrieve(db, rt, &c.question, Some(&scope), docs, &opts).await;
        let latency = started.elapsed().as_secs_f64() * 1000.0;
        let rank = r.passages.iter().position(|p| {
            p.filename == c.document
                && c.pages.iter().any(|pg| {
                    p.page_start.map(|s| s <= *pg).unwrap_or(false) && p.page_end.or(p.page_start).map(|e| e >= *pg).unwrap_or(false)
                })
        });
        m.record(&r.route, rank, latency);
    }
    m
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();

    let db = Arc::new(MemoryDatabase::new_in_memory()?);
    let mut docs: Vec<i64> = Vec::new();
    let mut cases: Vec<Case> = Vec::new();

    if let (Some(dir), Some(golden)) = (arg("--corpus"), arg("--golden")) {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let bytes = std::fs::read(&path)?;
            let (text, status, error) = offline_intelligence::utils::extraction_outcome(
                offline_intelligence::utils::extract_content_from_bytes(&bytes, &name).await,
            );
            let doc = db.documents.upsert_document(NewDocument {
                original_bytes: &bytes,
                original_filename: &name,
                source_path: None,
                source_kind: "paperclip",
                mime_type: None,
                size_bytes: bytes.len() as i64,
                extracted_text: text,
                extraction_status: status,
                extraction_error: error,
                local_file_id: None,
            })?;
            println!("loaded {} ({} chars, {})", name, doc.char_count, doc.extraction_status);
            docs.push(doc.id);
        }
        for line in std::fs::read_to_string(golden)?.lines().filter(|l| !l.trim().is_empty()) {
            let g: Golden = serde_json::from_str(line)?;
            cases.push(Case { question: g.question, document: g.document, pages: g.expected_pages });
        }
    } else {
        for (id, _) in eval::load_corpus(&db, "rag-eval") {
            docs.push(id);
        }
        for g in eval::golden() {
            cases.push(Case { question: g.question.to_string(), document: g.document.to_string(), pages: vec![g.expected_page] });
        }
    }

    let keyword_rt = RagRuntime::with_embedder(Arc::new(NoEmbedder));
    println!("keyword          {}", score(&db, &keyword_rt, &docs, &cases, false).await.summary());
    println!("keyword + router {}", score(&db, &keyword_rt, &docs, &cases, true).await.summary());

    let llama = Arc::new(LlamaEmbedder::new());
    if !rag::embedder::Embedder::available(llama.as_ref()) {
        println!("embedding model or engine not installed; skipping vector modes");
        return Ok(());
    }
    let rt = RagRuntime::with_embedder(llama.clone());
    let started = Instant::now();
    let (_, embedded) = rag::indexer::run_once_with(&db, &rt).await;
    println!("embedded {} chunks in {:.1}s", embedded, started.elapsed().as_secs_f64());
    // Warm-up so server start-up is not counted as query latency.
    let _ = rag::embedder::Embedder::embed_query(llama.as_ref(), "warm up").await;
    println!("hybrid           {}", score(&db, &rt, &docs, &cases, false).await.summary());
    println!("hybrid + router  {}", score(&db, &rt, &docs, &cases, true).await.summary());
    llama.shutdown().await;
    Ok(())
}

struct NoEmbedder;

#[async_trait::async_trait]
impl rag::embedder::Embedder for NoEmbedder {
    async fn embed_documents(&self, _: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        anyhow::bail!("disabled")
    }
    async fn embed_query(&self, _: &str) -> anyhow::Result<Vec<f32>> {
        anyhow::bail!("disabled")
    }
    fn available(&self) -> bool {
        false
    }
    fn warm(&self) -> bool {
        false
    }
}
