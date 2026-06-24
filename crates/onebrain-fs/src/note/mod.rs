//! `note` resource group — vault Markdown note operations (v3.2.0).
//!
//! Business logic for the `onebrain note <verb>` commands. The CLI layer
//! (`onebrain-cli`) parses args + emits the envelope; this module is pure
//! filesystem/text logic over an already-resolved vault root.

mod append;
mod archive;
mod backlinks;
mod delete;
mod find;
mod folder;
mod io;
mod list;
mod r#move;
mod new;
mod orphans;
mod path_out;
mod read;
mod search;
mod stat;
mod walker;
mod write;

pub use append::{append_note, AppendResult};
pub use archive::{archive_note, ArchiveResult};
pub use backlinks::{backlinks, BacklinkEntry, BacklinksData};
pub use delete::{delete_note, DeleteResult};
pub use find::{find_notes, FindEntry, FindOptions, FindResult, FindType};
pub use folder::{create_folder, delete_folder, FolderResult};
pub use list::{list_notes, ListOptions, ListResult, ListSort, NoteEntry};
pub use new::{new_note, NewResult};
pub use orphans::{orphans, OrphansData};
pub use path_out::to_slash;
pub use r#move::{move_note, MoveResult};
pub use read::{read_note, ReadOptions, ReadResult};
pub use search::{search_notes, NoteMatch, SearchMode, SearchOptions, SearchResult};
pub use stat::{stat_note, NoteStatData};
pub use walker::{is_tooling_dir, walk_notes, TOOLING_DIRS};
pub use write::{write_note, WriteResult};
