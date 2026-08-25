//! Throwaway diagnostic: feed a real PDF file to the exact same extraction
//! path production uses (utils::pdf_text::extract_pdf_text) and print the
//! result verbatim, to empirically test whether a permissions-only-encrypted
//! (empty user password) PDF opens successfully via PDFium + pdfium-render.

#[tokio::main]
async fn main() {
    let path = std::env::args().nth(1).expect("usage: pdf_diag <path-to-pdf>");
    let bytes = std::fs::read(&path).expect("failed to read file");
    let filename = std::path::Path::new(&path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown.pdf")
        .to_string();

    println!("=== bind_pdfium() ===");
    match offline_intelligence::utils::pdf_text::extract_pdf_text(bytes, &filename).await {
        Ok(text) => {
            println!("=== extract_pdf_text OK, {} chars ===", text.len());
            println!("{}", text);
        }
        Err(e) => {
            println!("=== extract_pdf_text ERR ===");
            println!("{:?}", e);
        }
    }
}
