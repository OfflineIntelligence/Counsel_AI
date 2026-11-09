//! Throwaway diagnostic for `rag::chunker::tests::spreadsheet_rows_are_small_
//! numbered_chunks_carrying_the_header`, which asserts that row numbers
//! restart at 1 on each sheet and gets 2.
//!
//! Prints every chunk of a two-sheet workbook with its row range, so the
//! off-by-one can be read off rather than reasoned about.

use offline_intelligence::rag::chunker;

fn main() {
    let mut text = String::from("[[sheet:Fees]]\nParty\tService\tAmount\tDue");
    for i in 2..=8 {
        text.push_str(&format!("\n\nClient {}\tConsulting\t{}\t2026-0{}-15", i, i * 1000, 1 + i % 9));
    }
    text.push_str("\n\n[[sheet:Summary]]\nTotal\tValue\n\nGrand total\t7259000");

    for (i, c) in chunker::chunk_document(&text).iter().enumerate() {
        let body = &text[c.byte_start..c.byte_end];
        let prefix = c
            .prefix_start
            .zip(c.prefix_end)
            .map(|(s, e)| text[s..e].to_string())
            .unwrap_or_else(|| "-".into());
        println!(
            "{:>2}  {:<16} rows {:>3}..{:<3}  prefix={:?}\n      body={:?}",
            i,
            c.location_label.as_deref().unwrap_or("-"),
            c.para_start,
            c.para_end,
            prefix.replace('\t', "|"),
            body.replace('\n', " / ").replace('\t', "|"),
        );
    }
}
