//! Throwaway diagnostic: compute the exact content-hash the production
//! DocumentsStore would use for a given file, so we can look it up directly
//! in the real SQLite database.

fn main() {
    let path = std::env::args().nth(1).expect("usage: hash_diag <path>");
    let bytes = std::fs::read(&path).expect("failed to read file");
    let hash = offline_intelligence::memory_db::DocumentsStore::hash_bytes(&bytes);
    println!("{}", hash);
}
