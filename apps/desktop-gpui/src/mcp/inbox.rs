//! What arrived while an MCP session was listening, on disk.
//!
//! An MCP server is a child process of whatever AI client started it, so its
//! memory dies with the client. The inbox therefore lives in an append-only
//! journal next to the node's TLS identity: a file that landed during one
//! session is still discoverable in the next one, and a model can answer "what
//! did my laptop send me last night?" without the sender being online.
//!
//! One JSON object per line. Appending is a single `write` of one line, so a
//! process killed mid-write at worst leaves a truncated final line, which
//! [`Journal::read`] skips rather than failing the whole file.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// One thing that arrived: a file, or a text message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReceivedFile {
    /// Stable within this machine's journal: `"<millis>-<line number>"`.
    pub id: String,
    /// A file name, or `"(text message)"`.
    pub name: String,
    /// Where it landed. Absent for a text message, which is never written to disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub size: u64,
    /// The sending device's alias, as it named itself on the wire.
    pub sender: String,
    /// The sending device's fingerprint — its identity across address changes.
    pub sender_id: String,
    pub received_at_ms: u64,
    /// Set for a text message, which travels as a zero-length file with a `preview`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl ReceivedFile {
    /// A received file, stamped now.
    pub fn file(name: String, path: PathBuf, size: u64, sender: String, sender_id: String) -> Self {
        Self {
            id: String::new(),
            name,
            path: Some(path),
            size,
            sender,
            sender_id,
            received_at_ms: now_millis(),
            text: None,
        }
    }

    /// A received text message. No path: nothing was written.
    pub fn text(text: String, sender: String) -> Self {
        Self {
            id: String::new(),
            name: "(text message)".to_string(),
            path: None,
            size: text.len() as u64,
            sender,
            sender_id: String::new(),
            received_at_ms: now_millis(),
            text: Some(text),
        }
    }
}

/// The append-only file the inbox is read from.
#[derive(Clone, Debug)]
pub struct Journal {
    path: PathBuf,
}

impl Journal {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every entry, oldest first. Blank and unparsable lines are skipped: one
    /// torn write must not cost the whole history.
    pub fn read(&self) -> Vec<ReceivedFile> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<ReceivedFile>(line).ok())
            .collect()
    }

    /// Append one entry, numbering it so `id` is stable and sortable.
    ///
    /// `existing` is the count of entries already in the journal, which is what
    /// makes the id unique without a second pass over the file. The entry is
    /// numbered in place, so a caller can still show it when the write fails.
    pub fn append(&self, entry: &mut ReceivedFile, existing: usize) -> std::io::Result<()> {
        entry.id = format!("{}-{}", entry.received_at_ms, existing + 1);
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut line = serde_json::to_string(&entry).map_err(std::io::Error::other)?;
        line.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line.as_bytes())?;
        Ok(())
    }
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn an_entry_survives_a_round_trip_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::new(dir.path().join("nested/inbox.jsonl"));

        let mut written = ReceivedFile::file(
            "holiday.jpg".into(),
            PathBuf::from("/tmp/holiday.jpg"),
            42,
            "Pixel".into(),
            "fingerprint-of-the-pixel".into(),
        );
        journal.append(&mut written, 0).unwrap();
        assert_eq!(written.id, format!("{}-1", written.received_at_ms));

        let read = journal.read();
        assert_eq!(read, vec![written]);
        assert_eq!(read[0].path.as_deref(), Some(Path::new("/tmp/holiday.jpg")));
        assert_eq!(read[0].sender_id, "fingerprint-of-the-pixel");
    }

    #[::core::prelude::v1::test]
    fn ids_do_not_collide_when_two_files_land_in_the_same_millisecond() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::new(dir.path().join("inbox.jsonl"));
        let entry = || {
            ReceivedFile::file(
                "a.txt".into(),
                PathBuf::from("/tmp/a.txt"),
                1,
                "s".into(),
                "f".into(),
            )
        };

        let mut first = entry();
        journal.append(&mut first, 0).unwrap();
        let mut second = entry();
        journal.append(&mut second, 1).unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(journal.read().len(), 2, "appending never rewrites history");
    }

    #[::core::prelude::v1::test]
    fn a_torn_last_line_is_skipped_rather_than_losing_the_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.jsonl");
        let journal = Journal::new(&path);
        let mut message = ReceivedFile::text("hello".into(), "Laptop".into());
        journal.append(&mut message, 0).unwrap();
        // A crash mid-append leaves half an object and no newline.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"id":"2-2","name":"tru"#).unwrap();

        let read = journal.read();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].text.as_deref(), Some("hello"));
        assert!(read[0].path.is_none(), "a text message is never a file");
    }

    #[::core::prelude::v1::test]
    fn a_missing_journal_is_an_empty_inbox_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Journal::new(dir.path().join("absent.jsonl"))
            .read()
            .is_empty());
    }
}
