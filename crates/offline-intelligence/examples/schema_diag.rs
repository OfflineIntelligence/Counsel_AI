//! Throwaway diagnostic: open a COPY of the real production database and
//! try DocumentsStore::initialize_schema() directly, surfacing the real
//! error (mod.rs's normal boot path only warn!()s this and swallows it).

use offline_intelligence::memory_db::{DocumentsStore, MemoryDatabase};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use std::sync::Arc;

fn main() {
    let path = std::env::args().nth(1).expect("usage: schema_diag <path-to-db-copy>");

    // First, exactly what MemoryDatabase::new does (full boot path), to see
    // if it errors overall.
    match MemoryDatabase::new(std::path::Path::new(&path)) {
        Ok(_) => println!("MemoryDatabase::new(...) => Ok"),
        Err(e) => println!("MemoryDatabase::new(...) => ERR: {:?}", e),
    }

    // Now isolate DocumentsStore::initialize_schema() specifically, against
    // a fresh pool pointed at the same file, to get its exact error text.
    let manager = SqliteConnectionManager::file(&path);
    let pool = Arc::new(Pool::builder().max_size(1).build(manager).expect("pool build"));
    let store = DocumentsStore::new(Arc::clone(&pool));
    match store.initialize_schema() {
        Ok(_) => println!("DocumentsStore::initialize_schema() => Ok"),
        Err(e) => println!("DocumentsStore::initialize_schema() => ERR: {:?}", e),
    }

    // Confirm post-state: does the documents table exist now?
    let conn = pool.get().unwrap();
    let mut stmt = conn.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='documents'").unwrap();
    let exists = stmt.exists([]).unwrap();
    println!("documents table exists after attempt: {}", exists);
}
