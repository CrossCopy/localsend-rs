//! Where a received file lands — the port, and the default that fills it.
//!
//! An embedder almost always has its own idea of what "received" means. A file
//! manager wants a staging tree it can publish atomically; a daemon wants the
//! bytes to arrive under an operation it can resume, sweep and report on. This
//! crate should not know about any of that, and until now it did: it wrote
//! through one specific filesystem crate belonging to one specific embedder,
//! which is also why it did not build outside that embedder's tree.
//!
//! So materialization is a port. [`ReceiveSink`] creates a destination,
//! [`PendingReceive`] owns it until somebody says publish or discard, and the
//! HTTP layer above holds neither a path nor a file handle of its own.
//!
//! [`AtomicFileSink`] is what a standalone receiver uses, and it keeps both
//! properties the previous implementation was here for. **A reader never sees a
//! partial file**: the name is reserved on create so two senders cannot pick it
//! at the same moment, the bytes go to a sibling temporary, and publication is a
//! rename. **An upload cannot leave the receive root**: every parent component
//! is checked with `symlink_metadata` before it is entered, so a planted
//! `linked-parent/` refuses rather than redirects.
//!
//! What it does *not* attempt is the rest of what a hostile filesystem can do,
//! and an embedder supplies that through the port: descriptor pinning instead of
//! path re-resolution, so a component cannot be swapped between the check and
//! the open, and an inode identity check between create and publish.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::io::{AsyncWrite, AsyncWriteExt};

/// The longest single path component the common filesystems accept.
const MAX_COMPONENT_BYTES: usize = 255;

/// How many ` (n)` variants to try before giving up on a colliding name.
const MAX_COLLISION_ATTEMPTS: usize = 1024;

/// Why a destination could not be created, kept, or published.
///
/// The split is not cosmetic. LocalSend's upload endpoint answers **400** for a
/// request the sender got wrong and **500** for a failure on this side, and a
/// port that returned one opaque error would collapse the two — a sender that
/// asked for `../../etc/passwd` would be told the receiver had broken, and
/// would reasonably retry.
#[derive(Debug)]
pub enum SinkError {
    /// The caller's fault: an unusable name, a path that escapes the root, an
    /// offer the host refuses. Answered with 400.
    Rejected(String),
    /// This side's fault: the disk is full, the directory vanished, a rename
    /// failed. Answered with 500.
    Failed(String),
}

impl std::fmt::Display for SinkError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(reason) => write!(formatter, "rejected: {reason}"),
            Self::Failed(reason) => write!(formatter, "failed: {reason}"),
        }
    }
}

impl std::error::Error for SinkError {}

/// Creates one destination per received file.
///
/// Implementations are shared across concurrent uploads, so this takes `&self`.
#[async_trait]
pub trait ReceiveSink: Send + Sync + 'static {
    /// Reserve a destination for `file_name` beneath `save_dir`.
    ///
    /// `file_name` is whatever the sender put on the wire and is not to be
    /// trusted; an implementation that cannot make it safe returns
    /// [`SinkError::Rejected`] rather than sanitizing it into something else.
    async fn create(
        &self,
        save_dir: &Path,
        file_name: &str,
    ) -> Result<Box<dyn PendingReceive>, SinkError>;
}

/// A destination that exists but is not yet the file anybody can see.
///
/// Exactly one of [`commit`](PendingReceive::commit) or
/// [`abort`](PendingReceive::abort) is called, and both consume it, so a
/// pending receive cannot be published twice or published after discard.
#[async_trait]
pub trait PendingReceive: Send {
    /// Where the bytes go. Called once per chunk for the life of the upload.
    fn writer(&mut self) -> &mut (dyn AsyncWrite + Unpin + Send);

    /// A path for logs and progress events. **Never filesystem authority** —
    /// the host chose it, and for a staging implementation it may name
    /// something no sender would recognise.
    fn display_path(&self) -> &Path;

    /// Make the file visible, durably. Returns the path a reader would use.
    async fn commit(self: Box<Self>) -> Result<PathBuf, SinkError>;

    /// Throw the partial away and release the reserved name.
    async fn abort(self: Box<Self>) -> Result<(), SinkError>;
}

/// The default sink: reserve the name, write beside it, publish by rename.
#[derive(Debug, Default, Clone, Copy)]
pub struct AtomicFileSink;

#[async_trait]
impl ReceiveSink for AtomicFileSink {
    async fn create(
        &self,
        save_dir: &Path,
        file_name: &str,
    ) -> Result<Box<dyn PendingReceive>, SinkError> {
        // The wire name is checked before it is used for anything, including
        // being joined onto a path — `safe_join` is where LocalSend's own
        // cross-platform name restrictions live.
        let joined = crate::path_safety::safe_join(save_dir, file_name)
            .map_err(|error| SinkError::Rejected(error.to_string()))?;
        let directory = joined
            .parent()
            .ok_or_else(|| SinkError::Rejected("a file name with no parent".to_string()))?
            .to_path_buf();
        let leaf = joined
            .file_name()
            .and_then(|leaf| leaf.to_str())
            .ok_or_else(|| SinkError::Rejected("a file name that is not text".to_string()))?
            .to_string();

        walk_or_create_directory(save_dir, &directory).await?;

        // Reserving the final name here, rather than at publish, is what makes
        // the collision suffix safe under concurrency: two senders offering
        // `photo.jpg` at the same moment take `photo.jpg` and `photo (1).jpg`
        // because the first one already exists by the time the second looks.
        let (final_path, _) = reserve_free_name(&directory, &leaf).await?;

        let temp_path = directory.join(format!(".localsend-{}.part", uuid::Uuid::new_v4()));
        let temp = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .await
            .map_err(|error| SinkError::Failed(error.to_string()))?;

        Ok(Box::new(AtomicPendingFile {
            temp: Some(temp),
            temp_path,
            final_path,
            directory,
        }))
    }
}

struct AtomicPendingFile {
    temp: Option<tokio::fs::File>,
    temp_path: PathBuf,
    final_path: PathBuf,
    directory: PathBuf,
}

#[async_trait]
impl PendingReceive for AtomicPendingFile {
    fn writer(&mut self) -> &mut (dyn AsyncWrite + Unpin + Send) {
        self.temp
            .as_mut()
            .expect("a pending receive owns its temporary until it is consumed")
    }

    fn display_path(&self) -> &Path {
        &self.final_path
    }

    async fn commit(mut self: Box<Self>) -> Result<PathBuf, SinkError> {
        let mut temp = self
            .temp
            .take()
            .expect("a pending receive owns its temporary until it is consumed");
        temp.flush()
            .await
            .map_err(|error| SinkError::Failed(error.to_string()))?;
        // The rename below is only atomic with respect to *visibility*. Without
        // this the bytes may still be in the page cache when the rename lands,
        // and a crash leaves a correctly-named file full of zeroes — which is
        // worse than no file at all, because nothing looks wrong.
        temp.sync_all()
            .await
            .map_err(|error| SinkError::Failed(error.to_string()))?;
        drop(temp);

        tokio::fs::rename(&self.temp_path, &self.final_path)
            .await
            .map_err(|error| SinkError::Failed(error.to_string()))?;
        sync_directory(&self.directory).await;
        Ok(self.final_path.clone())
    }

    async fn abort(mut self: Box<Self>) -> Result<(), SinkError> {
        drop(self.temp.take());
        let temp = tokio::fs::remove_file(&self.temp_path).await;
        // The reservation goes too. Leaving it would make the *next* sender's
        // `photo.jpg` become `photo (1).jpg` because of a transfer that failed.
        let reservation = tokio::fs::remove_file(&self.final_path).await;
        temp.and(reservation)
            .map_err(|error| SinkError::Failed(error.to_string()))
    }
}

impl Drop for AtomicPendingFile {
    fn drop(&mut self) {
        // Neither `commit` nor `abort` ran — the task was cancelled between
        // them, or a caller dropped the box. Both files are ours and nobody
        // else can be looking at either, so remove them without waiting for a
        // runtime that may already be gone.
        if self.temp.is_some() {
            let _ = std::fs::remove_file(&self.temp_path);
            let _ = std::fs::remove_file(&self.final_path);
        }
    }
}

/// Walk `save_dir` down to `directory`, creating what is missing, and refuse
/// the moment a component is anything other than a real directory.
///
/// `create_dir_all` is not usable here. A sender offering
/// `linked-parent/payload.bin`, where `linked-parent` is a symlink somebody
/// planted in the save directory, would have `create_dir_all` succeed and the
/// bytes land wherever the link points — outside the receiver-chosen root. That
/// is an escape, not merely weaker hardening, and this crate refused it before
/// materialization was a port (`tests/receive_path_safety.rs`).
///
/// `symlink_metadata` does not traverse, so a symlink is reported as a symlink
/// and fails the `is_dir` test. What this does **not** buy is the stronger
/// guarantee an `O_NOFOLLOW` walk holding descriptors gives: an attacker who
/// can already write inside the save directory could swap a real directory for
/// a symlink between the check here and the open below. Closing that needs
/// held descriptors, which is what an embedder's sink brings — see the module
/// docs.
async fn walk_or_create_directory(root: &Path, directory: &Path) -> Result<(), SinkError> {
    let relative = directory
        .strip_prefix(root)
        .map_err(|_| SinkError::Rejected("a destination outside the receive root".to_string()))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(SinkError::Rejected(
                "a path component that is not a plain name".to_string(),
            ));
        };
        current.push(name);
        match tokio::fs::symlink_metadata(&current).await {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(SinkError::Rejected(format!(
                    "{} is not a directory",
                    current.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tokio::fs::create_dir(&current)
                    .await
                    .map_err(|error| SinkError::Failed(error.to_string()))?;
            }
            Err(error) => return Err(SinkError::Failed(error.to_string())),
        }
    }
    Ok(())
}

/// Create the first free name in the `name`, `name (1)`, `name (2)` … series,
/// exclusively, and hand back the path that now exists.
async fn reserve_free_name(directory: &Path, leaf: &str) -> Result<(PathBuf, String), SinkError> {
    for attempt in 0..=MAX_COLLISION_ATTEMPTS {
        let candidate = if attempt == 0 {
            leaf.to_string()
        } else {
            collision_leaf(leaf, attempt)
        };
        let path = directory.join(&candidate);
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(_) => return Ok((path, candidate)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(SinkError::Failed(error.to_string())),
        }
    }
    Err(SinkError::Failed(format!(
        "{MAX_COLLISION_ATTEMPTS} names beginning {leaf:?} are all taken"
    )))
}

/// `report.txt` at attempt 2 becomes `report (2).txt`.
///
/// A leading dot is not an extension — `.bashrc` is a whole name — which is why
/// the search for the separator ignores index 0.
fn collision_leaf(original: &str, attempt: usize) -> String {
    let suffix = format!(" ({attempt})");
    let split = original.rfind('.').filter(|index| *index > 0);
    let (mut stem, mut extension) = split
        .map(|index| original.split_at(index))
        .unwrap_or((original, ""));
    if extension.len() + suffix.len() >= MAX_COMPONENT_BYTES {
        stem = original;
        extension = "";
    }
    let budget = MAX_COMPONENT_BYTES - suffix.len() - extension.len();
    stem = truncate_utf8(stem, budget);
    format!("{stem}{suffix}{extension}")
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Fsync the directory so the rename itself survives a crash. Best effort:
/// Windows cannot open a directory as a file, and a receiver that has already
/// written and renamed the data should not fail because of it.
async fn sync_directory(directory: &Path) {
    if let Ok(handle) = tokio::fs::File::open(directory).await {
        let _ = handle.sync_all().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn write_and_commit(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let mut pending = AtomicFileSink
            .create(root, name)
            .await
            .expect("create a destination");
        pending.writer().write_all(bytes).await.expect("write");
        pending.commit().await.expect("commit")
    }

    #[tokio::test]
    async fn a_committed_file_is_the_bytes_that_were_written() {
        let root = tempfile::tempdir().expect("root");
        let path = write_and_commit(root.path(), "report.txt", b"hello").await;
        assert_eq!(path, root.path().join("report.txt"));
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"hello");
    }

    #[tokio::test]
    async fn a_second_file_with_one_name_is_renamed_and_the_first_is_untouched() {
        let root = tempfile::tempdir().expect("root");
        write_and_commit(root.path(), "report.txt", b"first").await;
        let second = write_and_commit(root.path(), "report.txt", b"second").await;

        assert_eq!(second, root.path().join("report (1).txt"));
        assert_eq!(
            tokio::fs::read(root.path().join("report.txt"))
                .await
                .unwrap(),
            b"first"
        );
        assert_eq!(tokio::fs::read(&second).await.unwrap(), b"second");
    }

    #[tokio::test]
    async fn the_name_is_reserved_before_a_single_byte_arrives() {
        // Two senders offering the same name at the same moment is the case
        // a rename-at-publish implementation gets wrong: both would pick
        // `report.txt`, and the second would overwrite the first.
        let root = tempfile::tempdir().expect("root");
        let first = AtomicFileSink
            .create(root.path(), "report.txt")
            .await
            .expect("first destination");
        let second = AtomicFileSink
            .create(root.path(), "report.txt")
            .await
            .expect("second destination");

        assert_eq!(first.display_path(), root.path().join("report.txt"));
        assert_eq!(second.display_path(), root.path().join("report (1).txt"));
    }

    #[tokio::test]
    async fn an_aborted_upload_leaves_nothing_behind_and_frees_its_name() {
        let root = tempfile::tempdir().expect("root");
        let mut pending = AtomicFileSink
            .create(root.path(), "report.txt")
            .await
            .expect("destination");
        pending.writer().write_all(b"partial").await.expect("write");
        pending.abort().await.expect("abort");

        let left: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(left.is_empty(), "abort left {left:?} behind");

        // And the freed name is the one the next sender gets, rather than
        // `report (1).txt` for a transfer that never happened.
        let next = write_and_commit(root.path(), "report.txt", b"next").await;
        assert_eq!(next, root.path().join("report.txt"));
    }

    #[tokio::test]
    async fn a_partial_upload_is_never_visible_under_the_name_it_will_take() {
        let root = tempfile::tempdir().expect("root");
        let mut pending = AtomicFileSink
            .create(root.path(), "report.txt")
            .await
            .expect("destination");
        pending.writer().write_all(b"half").await.expect("write");
        pending.writer().flush().await.expect("flush");

        // The reservation exists, and it is empty: a reader that opens it mid
        // transfer gets nothing rather than half a file.
        assert_eq!(
            tokio::fs::read(root.path().join("report.txt"))
                .await
                .unwrap(),
            b""
        );
        pending.commit().await.expect("commit");
        assert_eq!(
            tokio::fs::read(root.path().join("report.txt"))
                .await
                .unwrap(),
            b"half"
        );
    }

    #[tokio::test]
    async fn a_dropped_upload_cleans_up_after_itself() {
        let root = tempfile::tempdir().expect("root");
        {
            let mut pending = AtomicFileSink
                .create(root.path(), "report.txt")
                .await
                .expect("destination");
            pending
                .writer()
                .write_all(b"abandoned")
                .await
                .expect("write");
        }
        let left: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(left.is_empty(), "a dropped upload left {left:?} behind");
    }

    #[tokio::test]
    async fn a_name_that_is_a_path_is_rejected_rather_than_flattened() {
        let root = tempfile::tempdir().expect("root");
        for name in ["../escape.txt", "/etc/passwd", "..", "", "a/../../b.txt"] {
            let refusal = AtomicFileSink.create(root.path(), name).await;
            assert!(
                matches!(refusal, Err(SinkError::Rejected(_))),
                "{name:?} was not rejected"
            );
        }
    }

    #[tokio::test]
    async fn a_relative_subpath_is_kept_rather_than_flattened() {
        // LocalSend senders offer `holiday/day one.jpg` for a folder, and
        // `safe_join` allows a relative path with ordinary components. Turning
        // it into `holiday_day one.jpg` would silently change what the sender
        // asked for; refusing it would break the folder case outright.
        let root = tempfile::tempdir().expect("root");
        let path = write_and_commit(root.path(), "holiday/day one.jpg", b"jpeg").await;
        assert_eq!(path, root.path().join("holiday").join("day one.jpg"));
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"jpeg");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_planted_parent_symlink_refuses_rather_than_redirects() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::tempdir().expect("outside");
        symlink(outside.path(), root.path().join("linked")).expect("plant");

        let refusal = AtomicFileSink
            .create(root.path(), "linked/payload.bin")
            .await;

        assert!(
            matches!(refusal, Err(SinkError::Rejected(_))),
            "a parent symlink was followed instead of refused"
        );
        assert!(
            !outside.path().join("payload.bin").exists(),
            "the upload escaped the receive root"
        );
    }

    #[test]
    fn a_leading_dot_is_a_name_and_not_an_extension() {
        assert_eq!(collision_leaf("report.txt", 2), "report (2).txt");
        assert_eq!(collision_leaf(".bashrc", 1), ".bashrc (1)");
        assert_eq!(collision_leaf("archive.tar.gz", 1), "archive.tar (1).gz");
        assert_eq!(collision_leaf("noextension", 3), "noextension (3)");
    }

    #[test]
    fn a_name_at_the_component_limit_still_leaves_room_for_its_suffix() {
        let long = format!("{}.txt", "a".repeat(300));
        let renamed = collision_leaf(&long, 1);
        assert!(renamed.len() <= MAX_COMPONENT_BYTES, "{}", renamed.len());
        assert!(renamed.ends_with(" (1).txt"));
    }
}
