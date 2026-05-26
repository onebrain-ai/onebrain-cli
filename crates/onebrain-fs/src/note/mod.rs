//! `note` resource group — vault Markdown note operations (v3.2.0).
//!
//! Business logic for the `onebrain note <verb>` commands. The CLI layer
//! (`onebrain-cli`) parses args + emits the envelope; this module is pure
//! filesystem/text logic over an already-resolved vault root.

mod backlinks;
mod find;
mod list;
mod orphans;
mod read;
mod search;
mod stat;
mod walker;

pub use backlinks::{backlinks, BacklinkEntry, BacklinksData};
pub use find::{find_notes, FindEntry, FindOptions, FindResult, FindType};
pub use list::{list_notes, ListOptions, ListResult, ListSort, NoteEntry};
pub use orphans::{orphans, OrphansData};
pub use read::{read_note, ReadOptions, ReadResult};
pub use search::{search_notes, NoteMatch, SearchMode, SearchOptions, SearchResult};
pub use stat::{stat_note, NoteStatData};
pub use walker::walk_notes;
