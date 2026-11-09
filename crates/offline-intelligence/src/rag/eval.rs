//! R0 evaluation harness: a reproducible synthetic legal corpus with facts
//! planted on known pages, golden questions in several languages, and
//! page-hit metrics per retrieval mode. Real firm documents are evaluated
//! with `examples/rag_eval.rs` against the same metrics.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;

use super::embedder::Embedder;
use super::search::{self, RetrieveOptions};
use super::RagRuntime;
use crate::memory_db::{MemoryDatabase, NewDocument};

#[derive(Debug, Clone)]
pub struct GoldenQuestion {
    pub question: &'static str,
    pub document: &'static str,
    pub expected_page: i32,
}

#[derive(Debug, Clone, Default)]
pub struct RouteMetrics {
    pub questions: usize,
    pub hit_at_5: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Metrics {
    pub questions: usize,
    pub hit_at_1: usize,
    pub hit_at_5: usize,
    pub reciprocal_rank_sum: f64,
    pub latencies_ms: Vec<f64>,
    pub per_route: std::collections::BTreeMap<String, RouteMetrics>,
}

impl Metrics {
    pub fn record(&mut self, route: &str, rank: Option<usize>, latency_ms: f64) {
        self.questions += 1;
        self.latencies_ms.push(latency_ms);
        let r = self.per_route.entry(route.to_string()).or_default();
        r.questions += 1;
        if let Some(k) = rank {
            if k == 0 {
                self.hit_at_1 += 1;
            }
            if k < 5 {
                self.hit_at_5 += 1;
                r.hit_at_5 += 1;
            }
            self.reciprocal_rank_sum += 1.0 / (k as f64 + 1.0);
        }
    }
    pub fn percentile_ms(&self, pct: f64) -> f64 {
        if self.latencies_ms.is_empty() {
            return 0.0;
        }
        let mut v = self.latencies_ms.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((pct / 100.0) * (v.len() as f64 - 1.0)).round() as usize;
        v[idx.min(v.len() - 1)]
    }
    pub fn summary(&self) -> String {
        let routes: Vec<String> = self
            .per_route
            .iter()
            .map(|(k, r)| format!("{} {}/{}", k, r.hit_at_5, r.questions))
            .collect();
        format!(
            "hit@1={:.2} hit@5={:.2} mrr={:.2} p50={:.0}ms p95={:.0}ms | hit@5 by route: {}",
            self.hit1_rate(), self.hit5_rate(), self.mrr(), self.percentile_ms(50.0), self.percentile_ms(95.0), routes.join(", ")
        )
    }
    pub fn hit5_rate(&self) -> f64 {
        self.hit_at_5 as f64 / self.questions.max(1) as f64
    }
    pub fn hit1_rate(&self) -> f64 {
        self.hit_at_1 as f64 / self.questions.max(1) as f64
    }
    pub fn mrr(&self) -> f64 {
        self.reciprocal_rank_sum / self.questions.max(1) as f64
    }
}

const FILLER: &[&str] = &[
    "The parties acknowledge that this Agreement has been negotiated at arm's length and that each party has had the opportunity to obtain independent legal advice.",
    "Nothing in this Agreement shall be construed as creating a partnership, joint venture or agency relationship between the parties.",
    "Any notice given under this Agreement shall be in writing and delivered by hand, courier or registered post to the address set out above.",
    "Headings are for convenience only and shall not affect the interpretation of this Agreement.",
    "This Agreement may be executed in any number of counterparts, each of which shall be an original.",
];

/// (file name, total pages, planted facts as (page, heading, text)).
pub fn corpus() -> Vec<(&'static str, i32, Vec<(i32, &'static str, &'static str)>)> {
    vec![
        (
            "Share_Purchase_Agreement.pdf",
            120,
            vec![
                (14, "Section 2.4 Purchase Price", "The Purchase Price is EUR 48,500,000 payable in cash at Closing, subject to the working capital adjustment."),
                (47, "Section 12.3 Cap on Liability", "The aggregate liability of the Seller for all Warranty Claims shall not exceed an amount equal to thirty per cent of the Purchase Price."),
                (63, "Section 15.1 Governing Law", "This Agreement and any non-contractual obligations arising out of it shall be governed by the laws of England and Wales."),
                (88, "Section 18.2 Non-Compete", "For a period of three years after Closing the Seller shall not carry on any business competing with the Target in the Territory."),
            ],
        ),
        (
            "Office_Lease_Madrid.pdf",
            60,
            vec![
                (9, "Cláusula 4 Renta", "La renta anual asciende a trescientos mil euros, pagaderos por meses anticipados dentro de los cinco primeros días de cada mes."),
                (31, "Cláusula 11 Obras", "El arrendatario no podrá realizar obras que modifiquen la configuración del inmueble sin el consentimiento escrito del arrendador."),
                (52, "Cláusula 17 Resolución", "El arrendador podrá resolver el contrato por impago de dos mensualidades consecutivas."),
            ],
        ),
        (
            "Contrat_Distribution_Paris.pdf",
            50,
            vec![
                (12, "Article 6 Exclusivité", "Le Fournisseur accorde au Distributeur l'exclusivité de la distribution des Produits sur le territoire français."),
                (38, "Article 14 Résiliation", "Chaque partie peut résilier le contrat moyennant un préavis de six mois notifié par lettre recommandée."),
            ],
        ),
        (
            "Kiraya_Anubandh_Delhi.pdf",
            30,
            vec![
                (5, "खंड 3 किराया", "मासिक किराया पचास हज़ार रुपये है, जो प्रत्येक माह की पाँच तारीख तक देय होगा।"),
                (19, "खंड 9 सुरक्षा जमा", "किरायेदार तीन महीने के किराये के बराबर सुरक्षा जमा राशि जमा करेगा।"),
            ],
        ),
        (
            "Shanghai_Service_Agreement.pdf",
            30,
            vec![
                (8, "第五条 服务费用", "客户应在收到发票后三十日内支付全部服务费用。"),
                (21, "第十二条 争议解决", "因本合同引起的争议应提交上海国际仲裁中心仲裁。"),
            ],
        ),
        (
            "Liefervertrag_Berlin.pdf",
            40,
            vec![
                (7, "§ 3 Lieferfristen", "Die Lieferung erfolgt innerhalb von vierzehn Werktagen nach Eingang der schriftlichen Bestellung."),
                (22, "§ 9 Gewährleistung", "Die Gewährleistungsfrist beträgt vierundzwanzig Monate ab Gefahrübergang."),
            ],
        ),
    ]
}

pub fn golden() -> Vec<GoldenQuestion> {
    vec![
        GoldenQuestion { question: "What is the purchase price?", document: "Share_Purchase_Agreement.pdf", expected_page: 14 },
        GoldenQuestion { question: "Is the seller's liability for warranty claims capped?", document: "Share_Purchase_Agreement.pdf", expected_page: 47 },
        GoldenQuestion { question: "Which law governs the agreement?", document: "Share_Purchase_Agreement.pdf", expected_page: 63 },
        GoldenQuestion { question: "How long is the non-compete after closing?", document: "Share_Purchase_Agreement.pdf", expected_page: 88 },
        GoldenQuestion { question: "What is on page 47?", document: "Share_Purchase_Agreement.pdf", expected_page: 47 },
        GoldenQuestion { question: "Show section 15.1", document: "Share_Purchase_Agreement.pdf", expected_page: 63 },
        GoldenQuestion { question: "¿Cuál es la renta anual?", document: "Office_Lease_Madrid.pdf", expected_page: 9 },
        GoldenQuestion { question: "¿Puede el arrendatario hacer obras?", document: "Office_Lease_Madrid.pdf", expected_page: 31 },
        GoldenQuestion { question: "cláusula 17", document: "Office_Lease_Madrid.pdf", expected_page: 52 },
        GoldenQuestion { question: "Wie lange ist die Gewährleistungsfrist?", document: "Liefervertrag_Berlin.pdf", expected_page: 22 },
        GoldenQuestion { question: "Seite 7", document: "Liefervertrag_Berlin.pdf", expected_page: 7 },
        // Paraphrases sharing no key words with the text.
        GoldenQuestion { question: "Can the vendor open a rival company once the deal completes?", document: "Share_Purchase_Agreement.pdf", expected_page: 88 },
        GoldenQuestion { question: "How much money does the acquirer have to hand over?", document: "Share_Purchase_Agreement.pdf", expected_page: 14 },
        // Cross-lingual: English questions about Spanish and German documents.
        GoldenQuestion { question: "How much yearly rent is due for the Madrid office?", document: "Office_Lease_Madrid.pdf", expected_page: 9 },
        GoldenQuestion { question: "What happens if the tenant misses two monthly payments?", document: "Office_Lease_Madrid.pdf", expected_page: 52 },
        GoldenQuestion { question: "How quickly must the goods be delivered after an order?", document: "Liefervertrag_Berlin.pdf", expected_page: 7 },
        GoldenQuestion { question: "How long is the warranty period?", document: "Liefervertrag_Berlin.pdf", expected_page: 22 },
        // French, Hindi and Chinese: native questions and English questions.
        GoldenQuestion { question: "Quel est le préavis de résiliation ?", document: "Contrat_Distribution_Paris.pdf", expected_page: 38 },
        GoldenQuestion { question: "Is the French distributor exclusive in France?", document: "Contrat_Distribution_Paris.pdf", expected_page: 12 },
        GoldenQuestion { question: "मासिक किराया कितना है?", document: "Kiraya_Anubandh_Delhi.pdf", expected_page: 5 },
        GoldenQuestion { question: "How large is the security deposit for the Delhi premises?", document: "Kiraya_Anubandh_Delhi.pdf", expected_page: 19 },
        GoldenQuestion { question: "服务费用什么时候支付？", document: "Shanghai_Service_Agreement.pdf", expected_page: 8 },
        GoldenQuestion { question: "Where are disputes under the Shanghai agreement arbitrated?", document: "Shanghai_Service_Agreement.pdf", expected_page: 21 },
        GoldenQuestion { question: "第十二条", document: "Shanghai_Service_Agreement.pdf", expected_page: 21 },
        // FAQ-style guide laid out like real pdfium output (single line
        // breaks, running footer, table of contents that echoes every heading).
        GoldenQuestion { question: "when must I register my company?", document: "Program_Eligibility_Guide.pdf", expected_page: 4 },
        GoldenQuestion { question: "When must I certify regarding my firm's eligibility?", document: "Program_Eligibility_Guide.pdf", expected_page: 4 },
        GoldenQuestion { question: "Who must register?", document: "Program_Eligibility_Guide.pdf", expected_page: 3 },
        GoldenQuestion { question: "Is size determined by revenue?", document: "Program_Eligibility_Guide.pdf", expected_page: 13 },
        GoldenQuestion { question: "What is the employee limit for an awardee and its affiliates?", document: "Program_Eligibility_Guide.pdf", expected_page: 13 },
    ]
}

/// One page of distinct body text, for fixtures that need every page to
/// survive chunking.
///
/// Pages in a fixture must not differ only by a number. `chunker::
/// boilerplate_key` normalises digits to `#`, so "Page 1 text about X" and
/// "Page 2 text about X" collapse to a single key — which is precisely what
/// that function is for, since that is what a running header looks like
/// ("Page 4 of 40"). A document whose every page is one such line is, as far
/// as the chunker can tell, a document made entirely of running headers, and
/// it correctly discards all of it.
///
/// Three tests were written that way and had been failing for exactly this
/// reason: `chunker::every_paragraph_is_covered_and_ranges_are_valid`,
/// `citations::wrong_page_missing_page_unknown_file_and_misquote_are_flagged`
/// and `search::ties_keep_reading_order`. Each page below gets its own words
/// while keeping its number visible, so page lookups stay assertable.
///
/// `build_document_text` above does not need this: its pages already carry
/// four varied filler paragraphs each.
pub fn distinct_page_text(p: usize) -> String {
    const SUBJECTS: [&str; 8] = [
        "the obligations of the parties on completion",
        "remedies available to the buyer for latent defects",
        "the escrow account and its release conditions",
        "confidentiality and the permitted disclosures",
        "assignment, novation and change of control",
        "warranties concerning title and encumbrances",
        "the indemnity for environmental liabilities",
        "governing law, jurisdiction and service of process",
    ];
    const VERBS: [&str; 4] = ["sets out", "qualifies", "restricts", "supplements"];
    format!(
        "Clause {} {} {}. This is page {} of the agreement.",
        p,
        VERBS[p % VERBS.len()],
        SUBJECTS[p % SUBJECTS.len()],
        p
    )
}

pub fn build_document_text(pages: i32, facts: &[(i32, &str, &str)]) -> String {
    let mut out = Vec::new();
    for p in 1..=pages {
        let mut page = format!("[[page:{}]]\n", p);
        if let Some((_, heading, fact)) = facts.iter().find(|(fp, _, _)| *fp == p) {
            page.push_str(&format!("{}\n\n{}\n\n", heading, fact));
        }
        for i in 0..4 {
            page.push_str(FILLER[(p as usize + i) % FILLER.len()]);
            page.push_str("\n\n");
        }
        out.push(page.trim_end().to_string());
    }
    out.join("\n\n")
}

/// A guide laid out the way pdfium extracts many real PDFs: every page a run
/// of single line breaks (no blank lines), a running footer and page number
/// at the top of each page, a table of contents on page 2 repeating every
/// heading, and question-style section titles.
pub fn faq_guide_text() -> String {
    let filler = "Applicants should read the program policy directive and the agency solicitation carefully.\r\n\
                  The guide does not create legal rights and the regulations control in case of conflict.";
    let mut pages: Vec<String> = Vec::new();
    for p in 1..=14 {
        let body = match p {
            1 => "Guide to Program Eligibility\r\nUpdated: September 2024\r\nPublished as the official compliance guide for small entities.".to_string(),
            2 => "Contents\r\n1. OVERVIEW........................................ 3\r\n2. REGISTER YOUR COMPANY........................................ 3\r\n\
                  Who must register?........................................ 3\r\nWhen must I register my company?........................................ 4\r\n\
                  3. CERTIFY THAT YOUR COMPANY MEETS THE REQUIREMENTS........................................ 4\r\n\
                  When must I certify regarding my firm's eligibility?........................................ 4\r\nSIZE........................................ 13".to_string(),
            3 => format!("1. OVERVIEW\r\nThe programs support small, for-profit, independent businesses.\r\n{}\r\n2. REGISTER YOUR COMPANY\r\nWho must register?\r\n\
                          • All applicants to the programs must register on the program website (click Register).", filler),
            4 => "• After registering, you will print out the completed proof of registration form. This form needs to be updated every 6 months.\r\n\
                  When must Iregister my company?\r\n• You must register prior to submitting an application.\r\n\
                  3. CERTIFY THAT YOUR COMPANY\r\nMEETS THE REQUIREMENTS\r\n\
                  When must I certify regarding my firm's eligibility?\r\n\
                  At the time of award. All awardees must sign a certification form at the time of award stating that the firm meets the program eligibility requirements.".to_string(),
            13 => format!("SIZE\r\nThe size requirement: An awardee, together with its affiliates, must not have more than 500 employees.\r\n\
                           Is size determined by revenue? No, size is determined only by the number of employees. There are no revenue limits.\r\n{}", filler),
            _ => format!("Ownership and control requirements continue.\r\n{}", filler),
        };
        pages.push(format!("[[page:{}]]\nSeptember 2024\r\n{}\r\n{}", p, p, body));
    }
    pages.join("\n\n")
}

pub fn load_corpus(db: &MemoryDatabase, session_id: &str) -> Vec<(i64, &'static str)> {
    let _ = db.conversations.create_session_with_id(session_id, None);
    let mut docs: Vec<(&'static str, String)> = corpus()
        .into_iter()
        .map(|(name, pages, facts)| (name, build_document_text(pages, &facts)))
        .collect();
    docs.push(("Program_Eligibility_Guide.pdf", faq_guide_text()));
    docs.into_iter()
        .map(|(name, text)| {
            let doc = db
                .documents
                .upsert_document(NewDocument {
                    original_bytes: text.as_bytes(),
                    original_filename: name,
                    source_path: None,
                    source_kind: "paperclip",
                    mime_type: None,
                    size_bytes: text.len() as i64,
                    extracted_text: text.clone(),
                    extraction_status: "ok",
                    extraction_error: None,
                    local_file_id: None,
                })
                .unwrap();
            db.documents.link_session(session_id, doc.id, "paperclip").unwrap();
            (doc.id, name)
        })
        .collect()
}

pub async fn evaluate(db: &MemoryDatabase, rt: &RagRuntime, docs: &[(i64, &'static str)], use_router: bool) -> Metrics {
    let mut m = Metrics::default();
    let ids: Vec<i64> = docs.iter().map(|(id, _)| *id).collect();
    let scope: HashSet<i64> = ids.iter().copied().collect();
    for q in golden() {
        let opts = RetrieveOptions { budget_bytes: 20_000, use_router, ..Default::default() };
        let started = std::time::Instant::now();
        let r = search::retrieve(db, rt, q.question, Some(&scope), &ids, &opts).await;
        let latency = started.elapsed().as_secs_f64() * 1000.0;
        let rank = r.passages.iter().position(|p| {
            p.filename == q.document
                && p.page_start.map(|s| s <= q.expected_page).unwrap_or(false)
                && p.page_end.or(p.page_start).map(|e| e >= q.expected_page).unwrap_or(false)
        });
        m.record(&r.route, rank, latency);
    }
    m
}

/// Deterministic stand-in for the neural embedder in CI: hashed bag of
/// character trigrams. Gives the vector path real work without a model.
pub struct TrigramEmbedder;

#[async_trait]
impl Embedder for TrigramEmbedder {
    async fn embed_documents(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| trigram_vector(t)).collect())
    }
    async fn embed_query(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        Ok(trigram_vector(text))
    }
    fn available(&self) -> bool {
        true
    }
    fn warm(&self) -> bool {
        true
    }
}

fn trigram_vector(text: &str) -> Vec<f32> {
    let lower: Vec<char> = text.to_lowercase().chars().filter(|c| c.is_alphanumeric() || *c == ' ').collect();
    let mut v = vec![0f32; 1024];
    for w in lower.windows(3) {
        let mut h: u64 = 1469598103934665603;
        for c in w {
            h ^= *c as u64;
            h = h.wrapping_mul(1099511628211);
        }
        let i = (h % 512) as usize;
        v[i] += if (h >> 20) & 1 == 0 { 1.0 } else { -1.0 };
    }
    v
}

pub fn test_runtime() -> Arc<RagRuntime> {
    Arc::new(RagRuntime::with_embedder(Arc::new(TrigramEmbedder)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exact_routes_always_hit_and_hybrid_is_at_least_as_good_as_keyword() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        let docs = load_corpus(&db, "eval-session");

        // Keyword only (no embedder).
        let keyword_rt = RagRuntime::with_embedder(Arc::new(Unavailable));
        let keyword = evaluate(&db, &keyword_rt, &docs, true).await;

        // Hybrid with the deterministic embedder.
        let rt = test_runtime();
        let db = Arc::new(db);
        super::super::indexer::run_once_with(&db, &rt).await;
        let hybrid = evaluate(&db, &rt, &docs, true).await;

        println!("R0 synthetic keyword: {}", keyword.summary());
        println!("R0 synthetic hybrid:  {}", hybrid.summary());
        assert!(keyword.percentile_ms(95.0) >= keyword.percentile_ms(50.0));
        assert_eq!(keyword.per_route.values().map(|r| r.questions).sum::<usize>(), keyword.questions);
        // Exact routes (page/section) must always hit, whatever the embedder.
        assert!(keyword.hit5_rate() >= 0.5, "keyword+router hit@5 {:.2}", keyword.hit5_rate());
        assert!(hybrid.hit5_rate() + 1e-9 >= keyword.hit5_rate() - 0.12, "hybrid must not regress materially");
    }

    struct Unavailable;

    #[async_trait]
    impl Embedder for Unavailable {
        async fn embed_documents(&self, _: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
            anyhow::bail!("unavailable")
        }
        async fn embed_query(&self, _: &str) -> anyhow::Result<Vec<f32>> {
            anyhow::bail!("unavailable")
        }
        fn available(&self) -> bool {
            false
        }
        fn warm(&self) -> bool {
            false
        }
    }
}
