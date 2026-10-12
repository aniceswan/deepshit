//! ds-chat: client newchat/list/get/delete/kirim/stream/stop/upload.
//!
//! Satu fungsi kirim fleksibel (tiru `send_text_ext` whatswrong):
//! upload SELALU dua tahap (arsip lokal + hash dulu, baru upload).

pub mod client;
pub use client::DsChatClient;
