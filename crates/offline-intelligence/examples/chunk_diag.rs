//! Diagnostic for the chunker's running-header removal.
//!
//! It prints two sweeps side by side, because the difference between them is
//! the thing worth understanding:
//!
//!   PATHOLOGICAL - every page is one line differing only by a number
//!                  ("Page 1 text…", "Page 2 text…"). `boilerplate_key`
//!                  normalises digits to `#`, so those collapse to a single
//!                  key, recur on every page, and are removed as a running
//!                  header. The document disappears. This is CORRECT: a page
//!                  consisting solely of "Page 4 of 40" has no content.
//!
//!                  Three tests were written this way and failed for exactly
//!                  this reason. They now use `eval::distinct_page_text`.
//!
//!   REALISTIC    - each page has its own wording. Nothing is removed.
//!
//! The jump at ten pages in the pathological sweep is where page numbers
//! become two digits: `page ## text` is a different key from `page # text`,
//! so for a while it sits under the recurrence threshold and survives.

use offline_intelligence::rag::chunker;
use offline_intelligence::rag::eval::distinct_page_text;

fn pathological(n: usize) -> String {
    (1..=n)
        .map(|p| format!("[[page:{}]]\nPage {} text about obligations of the parties and remedies.", p, p))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn realistic(n: usize) -> String {
    (1..=n)
        .map(|p| format!("[[page:{}]]\nPage {} text. {}", p, p, distinct_page_text(p)))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn pages_present(text: &str, chunks: &[chunker::Chunk], n: usize) -> usize {
    (1..=n)
        .filter(|p| {
            chunks
                .iter()
                .any(|c| text[c.byte_start..c.byte_end].contains(&format!("Page {} text", p)))
        })
        .count()
}

fn sweep(label: &str, build: fn(usize) -> String) {
    println!("=== {label} ===");
    println!("  {:>5}  {:>7}  {:>7}  {:>14}", "pages", "bytes", "chunks", "pages present");
    for n in [3usize, 5, 9, 10, 11, 12, 20, 40, 120] {
        let t = build(n);
        let cs = chunker::chunk_document(&t);
        let present = pages_present(&t, &cs, n);
        println!(
            "  {:>5}  {:>7}  {:>7}  {:>14}{}",
            n,
            t.len(),
            cs.len(),
            present,
            if present < n { "   <- removed as a running header" } else { "" }
        );
    }
    println!();
}

fn main() {
    sweep("PATHOLOGICAL: pages differing only by a number (expected to vanish)", pathological);
    sweep("REALISTIC: pages with their own wording (expected to survive intact)", realistic);

    // A real running footer over real content: what an attached PDF looks
    // like, and the case retrieval actually depends on.
    const FOOTER: &str = "Email: intl.stu@example.edu. P: 903.886.5097\n2200 Campbell st, Commerce, TX 75428";
    let text = (1..=5)
        .map(|p| format!("[[page:{}]]\n{}\n\n{}\n\n{}", p, FOOTER, distinct_page_text(p), FOOTER))
        .collect::<Vec<_>>()
        .join("\n\n");
    let chunks = chunker::chunk_document(&text);
    let indexed: String = chunks
        .iter()
        .map(|c| &text[c.byte_start..c.byte_end])
        .collect::<Vec<_>>()
        .join("\n");

    println!("=== REAL SHAPE: 5 pages, a 2-line footer top and bottom of each ===");
    println!("  source {} bytes -> {} chunks, {} bytes indexed ({:.1}% kept)",
        text.len(), chunks.len(), indexed.len(), 100.0 * indexed.len() as f64 / text.len() as f64);
    for line in FOOTER.lines() {
        println!("  footer line removed: {:<5} {:?}", !indexed.contains(line), line);
    }
    let kept = (1..=5).filter(|p| indexed.contains(&distinct_page_text(*p))).count();
    println!("  body sentences kept: {}/5", kept);
}
