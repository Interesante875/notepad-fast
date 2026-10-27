//! UI-independent text engine: indexing, piece table, search and layout.

pub mod document;
pub mod index;
pub mod layout;
pub mod source;

pub use document::{load_source, Document, EditKind, FindStep, SaveSnapshot, Sel, WrittenFile};
pub use source::Source;
