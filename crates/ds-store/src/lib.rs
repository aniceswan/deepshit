//! ds-store: arsip pesan pluggable. Default JSONL file (tanpa server DB).
//! Copy pola wa-store/archive.rs (trait Archive + JsonlArchive).

pub mod archive;
pub use archive::{Archive, JsonlArchive, StoredMsg};
