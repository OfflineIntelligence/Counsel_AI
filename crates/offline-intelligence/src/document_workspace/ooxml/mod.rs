//! Lossless OOXML (docx/xlsx/pptx) reading and surgical patching.
//!
//! See `package.rs` for the byte-preservation guarantee that everything here
//! is built on: a part we did not target comes out exactly as it went in.

//! Two layers, and the separation is what makes the guarantee hold:
//!
//! - `package` owns the ZIP: a part nobody asked to change comes out
//!   byte-identical.
//! - `spans` owns the XML: edits are byte splices into the original part text,
//!   never a re-serialised parse tree, so untouched markup keeps the exact form
//!   Word/Excel/PowerPoint wrote it in.
//!
//! The format modules sit on top and only ever say "replace these bytes".

pub mod docx;
pub mod package;
pub mod pptx;
pub mod spans;
pub mod xlsx;
