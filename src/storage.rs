// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::errors::StorageError;
use std::ffi::OsString;
use std::fs as std_fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::fs::{self, try_exists, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};

use crate::torrent_file::{validate_torrent_layout, InfoFile};
use crate::tui::tree::RawNode;

use crate::app::{FileMetadata, FilePriority};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct FileInfo {
    pub path: PathBuf,            // The full path to the file on the disk.
    pub length: u64,              // The length of the file in bytes.
    pub global_start_offset: u64, // The starting offset of this file within the torrent's complete data stream.
    pub is_padding: bool,         // Indicates if this is a BEP 47 padding file.
    pub is_skipped: bool,         // NEW: Indicates if the user set this file to Skip priority.
}

/// Manages the file layout for a torrent, abstracting away the difference
/// between single and multi-file torrents.
#[derive(Debug, Clone)]
pub struct MultiFileInfo {
    pub files: Vec<FileInfo>,
    pub total_size: u64,
    /// The user-selected download root. All file paths must resolve below this
    /// boundary, even when existing path components are symbolic links.
    pub download_root: PathBuf,
    /// Result of the first complete filesystem containment check performed by
    /// block I/O. Clones share this result so validating a torrent's immutable
    /// file map does not add synchronous canonicalization to every block.
    containment_validation: Arc<OnceLock<Result<(), CachedContainmentError>>>,
}

#[derive(Debug, Clone)]
struct CachedContainmentError {
    kind: ErrorKind,
    message: String,
}

impl CachedContainmentError {
    fn from_io(error: std::io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    fn to_io(&self) -> std::io::Error {
        std::io::Error::new(self.kind, self.message.clone())
    }
}

impl MultiFileInfo {
    pub(crate) fn from_parts(
        files: Vec<FileInfo>,
        total_size: u64,
        download_root: PathBuf,
    ) -> Self {
        Self {
            files,
            total_size,
            download_root,
            containment_validation: Arc::new(OnceLock::new()),
        }
    }

    /// Creates a new MultiFileInfo map. This is the central point of unification.
    /// It intelligently handles both single and multi-file torrent metadata.
    #[cfg(test)]
    pub fn new(
        root_dir: &Path,
        torrent_name: &str,
        files: Option<&Vec<InfoFile>>,
        length: Option<u64>,
        file_priorities: &HashMap<usize, FilePriority>, // NEW ARGUMENT
    ) -> std::io::Result<Self> {
        Self::new_with_download_root(
            root_dir,
            root_dir,
            torrent_name,
            files,
            length,
            file_priorities,
        )
    }

    /// Creates a file map while keeping the selected download directory as a
    /// separate containment boundary from an optional torrent container.
    pub fn new_with_download_root(
        download_root: &Path,
        root_dir: &Path,
        torrent_name: &str,
        files: Option<&Vec<InfoFile>>,
        length: Option<u64>,
        file_priorities: &HashMap<usize, FilePriority>,
    ) -> std::io::Result<Self> {
        validate_torrent_layout(torrent_name, files.map(Vec::as_slice).unwrap_or_default())
            .map_err(|error| std::io::Error::new(ErrorKind::InvalidInput, error))?;
        ensure_path_within_root(download_root, root_dir)?;

        if let Some(torrent_files) = files {
            let mut files_vec = Vec::new();
            let mut current_offset = 0;

            for (idx, f) in torrent_files.iter().enumerate() {
                let mut full_path = root_dir.to_path_buf();
                // The path in the torrent metadata can contain subdirectories.
                for component in &f.path {
                    full_path.push(component);
                }
                ensure_path_within_root(download_root, &full_path)?;

                // BEP 47: Check 'attr' string. If it contains 'p', it is a padding file.
                let is_padding = f.attr.as_deref().map(|s| s.contains('p')).unwrap_or(false);

                // NEW: Check priority
                let priority = file_priorities.get(&idx).unwrap_or(&FilePriority::Normal);
                let is_skipped = *priority == FilePriority::Skip;

                files_vec.push(FileInfo {
                    path: full_path,
                    length: f.length as u64,
                    global_start_offset: current_offset,
                    is_padding,
                    is_skipped,
                });

                current_offset = current_offset.checked_add(f.length as u64).ok_or_else(|| {
                    std::io::Error::new(
                        ErrorKind::InvalidInput,
                        "torrent aggregate file length exceeds the supported range",
                    )
                })?;
            }
            Ok(Self::from_parts(
                files_vec,
                current_offset,
                download_root.to_path_buf(),
            ))
        } else {
            let total_size = length.unwrap_or(0);
            let file_path = root_dir.join(torrent_name);
            ensure_path_within_root(download_root, &file_path)?;

            // Single file torrents: Index 0
            let priority = file_priorities.get(&0).unwrap_or(&FilePriority::Normal);
            let is_skipped = *priority == FilePriority::Skip;

            let single_file = FileInfo {
                path: file_path,
                length: total_size,
                global_start_offset: 0,
                is_padding: false,
                is_skipped,
            };
            Ok(Self::from_parts(
                vec![single_file],
                total_size,
                download_root.to_path_buf(),
            ))
        }
    }

    fn ensure_cached_containment(&self) -> std::io::Result<()> {
        // This preserves the construction/allocation-time symlink checks while
        // keeping canonicalization out of the per-block hot path. Like the old
        // check-before-open sequence, it cannot eliminate a concurrent symlink
        // swap; doing that requires descriptor-relative, no-follow filesystem
        // operations on each supported platform.
        let validation = self.containment_validation.get_or_init(|| {
            self.files
                .iter()
                .try_for_each(|file_info| {
                    ensure_path_within_root(&self.download_root, &file_info.path)
                })
                .map_err(CachedContainmentError::from_io)
        });

        validation.as_ref().map_err(CachedContainmentError::to_io)?;
        Ok(())
    }
}

/// Rejects a path when its lexical or existing-filesystem resolution escapes
/// the selected download root. The deepest existing ancestor is canonicalized
/// so the check also works before a new file or directory has been created.
pub(crate) fn ensure_path_within_root(download_root: &Path, path: &Path) -> std::io::Result<()> {
    let absolute_root = absolute_path(download_root)?;
    let absolute_target = absolute_path(path)?;
    let lexical_root = lexical_normalize(&absolute_root);
    let lexical_path = lexical_normalize(&absolute_target);
    if !lexical_path.starts_with(&lexical_root) {
        return Err(containment_error(download_root, path));
    }

    // Resolve the original absolute spellings rather than their lexical forms.
    // On Unix, `link/..` is interpreted after following `link`, so erasing `..`
    // first can otherwise check a different location from the one I/O will use.
    let resolved_root = resolve_existing_ancestor(&absolute_root)?;
    let resolved_path = resolve_existing_ancestor(&absolute_target)?;
    if !resolved_path.starts_with(&resolved_root) {
        return Err(containment_error(download_root, path));
    }

    Ok(())
}

fn containment_error(download_root: &Path, path: &Path) -> std::io::Error {
    std::io::Error::new(
        ErrorKind::PermissionDenied,
        format!(
            "storage path '{}' resolves outside download root '{}'",
            path.display(),
            download_root.display()
        ),
    )
}

fn absolute_path(path: &Path) -> std::io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::Normal(part) => normalized.push(part),
        }
    }

    normalized
}

fn resolve_existing_ancestor(path: &Path) -> std::io::Result<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut missing_parts = Vec::<OsString>::new();

    loop {
        match std_fs::symlink_metadata(&existing) {
            Ok(_) => {
                let mut resolved = std_fs::canonicalize(&existing)?;
                for part in missing_parts.iter().rev() {
                    resolved.push(part);
                }
                return Ok(lexical_normalize(&resolved));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let Some(part) = existing.file_name().map(ToOwned::to_owned) else {
                    return Err(error);
                };
                missing_parts.push(part);
                if !existing.pop() {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn ensure_multi_file_paths(multi_file_info: &MultiFileInfo) -> Result<(), StorageError> {
    multi_file_info.ensure_cached_containment()?;
    Ok(())
}

/// Creates all necessary directories and pre-allocates all files for a torrent.
/// This function works for both single and multi-file torrents.
pub async fn create_and_allocate_files(
    multi_file_info: &MultiFileInfo,
) -> Result<bool, StorageError> {
    // Validate the whole layout before making any filesystem changes so one
    // unsafe entry cannot leave a partially allocated torrent behind.
    ensure_multi_file_paths(multi_file_info)?;
    let mut is_fresh_download = true;

    for file_info in &multi_file_info.files {
        if file_info.is_padding {
            continue;
        }

        ensure_path_within_root(&multi_file_info.download_root, &file_info.path)?;
        let exists = try_exists(&file_info.path).await?;
        let existing_metadata = if exists {
            ensure_path_within_root(&multi_file_info.download_root, &file_info.path)?;
            Some(fs::metadata(&file_info.path).await?)
        } else {
            None
        };
        if existing_metadata
            .as_ref()
            .is_some_and(|metadata| metadata.is_file() && metadata.len() > 0)
        {
            is_fresh_download = false;
        }
    }

    for file_info in &multi_file_info.files {
        // Optimization: Don't allocate padding or skipped files
        if file_info.is_padding || file_info.is_skipped {
            continue;
        }

        let should_resize = |metadata: &std::fs::Metadata| {
            metadata.is_file()
                && metadata.len() != file_info.length
                && (!is_fresh_download || metadata.len() > 0)
        };

        // Ensure the parent directory for the file exists.
        if let Some(parent_dir) = file_info.path.parent() {
            ensure_path_within_root(&multi_file_info.download_root, parent_dir)?;
            if !try_exists(parent_dir).await? {
                ensure_path_within_root(&multi_file_info.download_root, parent_dir)?;
                fs::create_dir_all(parent_dir).await?;
            }
        }

        // Create fresh files without preallocating; some mounted filesystems can
        // block indefinitely when resizing sparse placeholders up front. Once a
        // download is known to be partial, however, zero-byte placeholders must
        // be sized before validation/uploads can read their sparse zeroes as
        // real in-span data.
        ensure_path_within_root(&multi_file_info.download_root, &file_info.path)?;
        match fs::metadata(&file_info.path).await {
            Ok(metadata) if should_resize(&metadata) => {
                ensure_path_within_root(&multi_file_info.download_root, &file_info.path)?;
                let file = OpenOptions::new()
                    .write(true)
                    .truncate(false)
                    .open(&file_info.path)
                    .await?;
                file.set_len(file_info.length).await?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {
                ensure_path_within_root(&multi_file_info.download_root, &file_info.path)?;
                let file = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&file_info.path)
                    .await?;
                let metadata = file.metadata().await?;
                if should_resize(&metadata) {
                    file.set_len(file_info.length).await?;
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(is_fresh_download)
}

pub async fn read_data_from_disk(
    multi_file_info: &MultiFileInfo,
    global_offset: u64,
    bytes_to_read: usize,
) -> Result<Vec<u8>, StorageError> {
    validate_io_span(multi_file_info, global_offset, bytes_to_read as u64, "read")?;
    multi_file_info.ensure_cached_containment()?;

    let mut buffer = Vec::with_capacity(bytes_to_read);
    let mut bytes_read = 0;

    for file_info in &multi_file_info.files {
        let file_start = file_info.global_start_offset;
        let file_end = file_start + file_info.length;
        let read_start = global_offset + bytes_read as u64;

        if read_start < file_end && global_offset < file_end {
            let local_offset = read_start.saturating_sub(file_start);
            let bytes_to_read_in_this_file = std::cmp::min(
                (bytes_to_read - bytes_read) as u64,
                file_info.length - local_offset,
            ) as usize;

            if bytes_to_read_in_this_file > 0 {
                if file_info.is_padding {
                    // This maintains offset integrity without requiring a file on disk.
                    let zeros = vec![0u8; bytes_to_read_in_this_file];
                    buffer.extend_from_slice(&zeros);
                } else {
                    // NEW: Fast Validation for Skipped Files
                    // If the file is skipped and MISSING, return zeros immediately.
                    // This simulates "Missing Data" without raising an IO error.
                    let should_fake_read = if file_info.is_skipped {
                        !try_exists(&file_info.path).await?
                    } else {
                        false
                    };

                    if should_fake_read {
                        let zeros = vec![0u8; bytes_to_read_in_this_file];
                        buffer.extend_from_slice(&zeros);
                    } else {
                        // Normal read from existing skipped files or normal files.
                        // Fresh downloads use zero-length placeholders instead of
                        // preallocating, so in-span reads past the physical EOF are
                        // treated as sparse zeroes.
                        let mut file = File::open(&file_info.path).await?;
                        let physical_len = file.metadata().await?.len();
                        let readable_bytes = physical_len
                            .saturating_sub(local_offset)
                            .min(bytes_to_read_in_this_file as u64)
                            as usize;
                        let mut temp_buf = vec![0; bytes_to_read_in_this_file];
                        if readable_bytes > 0 {
                            file.seek(SeekFrom::Start(local_offset)).await?;
                            file.read_exact(&mut temp_buf[..readable_bytes]).await?;
                        }
                        buffer.extend_from_slice(&temp_buf);
                    }
                }

                bytes_read += bytes_to_read_in_this_file;
            }

            if bytes_read == bytes_to_read {
                return Ok(buffer);
            }
        }
    }

    Err(StorageError::from(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "Failed to read all data, offset likely out of bounds",
    )))
}

pub async fn write_data_to_disk(
    multi_file_info: &MultiFileInfo,
    global_offset: u64,
    data_to_write: &[u8],
) -> Result<(), StorageError> {
    validate_io_span(
        multi_file_info,
        global_offset,
        data_to_write.len() as u64,
        "write",
    )?;
    multi_file_info.ensure_cached_containment()?;

    let mut bytes_written = 0;
    let data_len = data_to_write.len();

    for file_info in &multi_file_info.files {
        let file_start = file_info.global_start_offset;
        let file_end = file_start + file_info.length;
        let write_start = global_offset + bytes_written as u64;

        if write_start < file_end && global_offset < file_end {
            let local_offset = write_start.saturating_sub(file_start);
            let bytes_to_write_in_this_file = std::cmp::min(
                (data_len - bytes_written) as u64,
                file_info.length - local_offset,
            ) as usize;

            if bytes_to_write_in_this_file > 0 {
                if !file_info.is_padding {
                    // Note: We ALLOW writing to skipped files if necessary (e.g. boundary pieces).
                    // This will create them lazily if they were skipped during allocation.

                    // Ensure directory exists (lazy creation for skipped boundary files)
                    if file_info.is_skipped {
                        if let Some(parent) = file_info.path.parent() {
                            fs::create_dir_all(parent).await?;
                        }
                    }

                    let mut file = OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(&file_info.path)
                        .await?;

                    file.seek(SeekFrom::Start(local_offset)).await?;

                    let data_slice =
                        &data_to_write[bytes_written..bytes_written + bytes_to_write_in_this_file];

                    file.write_all(data_slice).await?;
                }

                bytes_written += bytes_to_write_in_this_file;
            }

            if bytes_written == data_len {
                return Ok(());
            }
        }
    }

    tracing::error!(
        "💾 [Storage] ERROR: Write incomplete! Written: {}/{}. Global Offset: {}",
        bytes_written,
        data_len,
        global_offset
    );

    Err(StorageError::from(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "Failed to write all data, offset likely out of bounds",
    )))
}

fn validate_io_span(
    multi_file_info: &MultiFileInfo,
    global_offset: u64,
    byte_count: u64,
    operation: &str,
) -> Result<(), StorageError> {
    let Some(end_offset) = global_offset.checked_add(byte_count) else {
        return Err(StorageError::from(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{operation} offset overflows torrent data span"),
        )));
    };

    if end_offset > multi_file_info.total_size {
        return Err(StorageError::from(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{operation} extends past torrent data span"),
        )));
    }

    Ok(())
}

pub async fn build_fs_tree(
    path: &Path,
    depth: usize,
) -> Result<Vec<RawNode<FileMetadata>>, std::io::Error> {
    let mut nodes = Vec::new();
    let mut entries = fs::read_dir(path).await?;

    while let Some(entry) = entries.next_entry().await? {
        let meta = entry.metadata().await?;
        let is_dir = meta.is_dir();
        let name = entry.file_name().to_string_lossy().into_owned();
        let full_path = entry.path();
        let size = meta.len();

        let modified = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);

        let children = if is_dir {
            if depth > 0 {
                Box::pin(build_fs_tree(&entry.path(), depth - 1)).await?
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        nodes.push(RawNode {
            name,
            full_path,
            is_dir,
            payload: FileMetadata { size, modified },
            children,
        });
    }

    nodes.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    Ok(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::FilePriority;
    use crate::torrent_file::InfoFile;

    use std::collections::HashMap;
    use tempfile::tempdir;
    use tokio::fs::File;
    use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};

    // --- HELPER FUNCTIONS ---

    /// Helper to create a single-file setup
    fn setup_single_file() -> (tempfile::TempDir, MultiFileInfo) {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let torrent_name = "single_file.txt";
        let length = 100;
        // FIX: Pass empty map for default priorities
        let mfi =
            MultiFileInfo::new(root, torrent_name, None, Some(length), &HashMap::new()).unwrap();
        (dir, mfi)
    }

    /// Helper to create a multi-file setup
    fn setup_multi_file() -> (tempfile::TempDir, MultiFileInfo) {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let torrent_name = "multi_file_torrent";
        let files = vec![
            InfoFile {
                path: vec!["file_a.txt".to_string()],
                length: 50, // Ends at 49
                md5sum: None,
                attr: None, // Standard file
            },
            InfoFile {
                path: vec!["subdir".to_string(), "file_b.txt".to_string()],
                length: 70, // Starts at 50
                md5sum: None,
                attr: None, // Standard file
            },
        ];
        // Total size 120
        // FIX: Pass empty map
        let mfi =
            MultiFileInfo::new(root, torrent_name, Some(&files), None, &HashMap::new()).unwrap();
        (dir, mfi)
    }

    /// Helper to create a setup with a padding file in the middle
    fn setup_padding_file_scenario() -> (tempfile::TempDir, MultiFileInfo) {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let torrent_name = "padding_test";
        // Scenario:
        // File 1: 10 bytes (Offset 0-9)
        // Padding: 5 bytes (Offset 10-14) - Should NOT be created on disk
        // File 2: 10 bytes (Offset 15-24)
        let files = vec![
            InfoFile {
                path: vec!["real_1.txt".to_string()],
                length: 10,
                md5sum: None,
                attr: None,
            },
            InfoFile {
                path: vec![".pad".to_string(), "10".to_string()],
                length: 5,
                md5sum: None,
                attr: Some("p".to_string()), // Attribute marking padding
            },
            InfoFile {
                path: vec!["real_2.txt".to_string()],
                length: 10,
                md5sum: None,
                attr: None,
            },
        ];
        // FIX: Pass empty map
        let mfi =
            MultiFileInfo::new(root, torrent_name, Some(&files), None, &HashMap::new()).unwrap();
        (dir, mfi)
    }

    #[test]
    fn multi_file_info_rejects_unsafe_paths_before_allocation() {
        let dir = tempdir().unwrap();
        let unsafe_files = vec![InfoFile {
            path: vec!["..".to_string(), "outside.bin".to_string()],
            length: 1,
            md5sum: None,
            attr: None,
        }];

        let error = MultiFileInfo::new(
            dir.path(),
            "safe-item",
            Some(&unsafe_files),
            None,
            &HashMap::new(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);

        let error = MultiFileInfo::new(dir.path(), "/outside.bin", None, Some(1), &HashMap::new())
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn storage_io_rejects_existing_symlink_escape() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let download_root = dir.path().join("downloads");
        let outside_root = dir.path().join("outside");
        std::fs::create_dir_all(&download_root).unwrap();
        std::fs::create_dir_all(&outside_root).unwrap();

        let files = vec![InfoFile {
            path: vec!["nested".to_string(), "payload.bin".to_string()],
            length: 4,
            md5sum: None,
            attr: None,
        }];
        let mfi = MultiFileInfo::new_with_download_root(
            &download_root,
            &download_root,
            "sample-data",
            Some(&files),
            None,
            &HashMap::new(),
        )
        .unwrap();

        symlink(&outside_root, download_root.join("nested")).unwrap();

        let allocation_error = create_and_allocate_files(&mfi).await.unwrap_err();
        assert!(matches!(
            allocation_error,
            StorageError::Io {
                kind: ErrorKind::PermissionDenied,
                ..
            }
        ));
        assert!(!outside_root.join("payload.bin").exists());

        std::fs::write(outside_root.join("payload.bin"), b"safe").unwrap();
        let read_error = read_data_from_disk(&mfi, 0, 4).await.unwrap_err();
        assert!(matches!(
            read_error,
            StorageError::Io {
                kind: ErrorKind::PermissionDenied,
                ..
            }
        ));

        let write_error = write_data_to_disk(&mfi, 0, b"risk").await.unwrap_err();
        assert!(matches!(
            write_error,
            StorageError::Io {
                kind: ErrorKind::PermissionDenied,
                ..
            }
        ));
        assert_eq!(
            std::fs::read(outside_root.join("payload.bin")).unwrap(),
            b"safe"
        );
    }

    #[tokio::test]
    async fn block_io_reuses_cached_containment_validation() {
        let (_dir, mfi) = setup_single_file();
        assert!(mfi.containment_validation.get().is_none());

        create_and_allocate_files(&mfi).await.unwrap();
        assert!(mfi.containment_validation.get().is_some());

        let cloned = mfi.clone();
        assert!(Arc::ptr_eq(
            &mfi.containment_validation,
            &cloned.containment_validation
        ));
        read_data_from_disk(&cloned, 0, 16).await.unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn multi_file_info_rejects_symlinked_container_outside_download_root() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let download_root = dir.path().join("downloads");
        let outside_root = dir.path().join("outside");
        std::fs::create_dir_all(&download_root).unwrap();
        std::fs::create_dir_all(&outside_root).unwrap();
        let container = download_root.join("renamed-container");
        symlink(&outside_root, &container).unwrap();

        let files = vec![InfoFile {
            path: vec!["payload.bin".to_string()],
            length: 1,
            md5sum: None,
            attr: None,
        }];
        let error = MultiFileInfo::new_with_download_root(
            &download_root,
            &container,
            "sample-data",
            Some(&files),
            None,
            &HashMap::new(),
        )
        .unwrap_err();

        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[test]
    fn configured_download_root_may_itself_be_a_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let physical_root = dir.path().join("physical");
        let selected_root = dir.path().join("selected");
        std::fs::create_dir_all(&physical_root).unwrap();
        symlink(&physical_root, &selected_root).unwrap();

        let mfi = MultiFileInfo::new(
            &selected_root,
            "payload.bin",
            None,
            Some(1),
            &HashMap::new(),
        )
        .unwrap();

        assert_eq!(mfi.download_root, selected_root);
    }

    #[tokio::test]
    async fn build_fs_tree_propagates_missing_root_error() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("missing");

        let error = build_fs_tree(&missing, 1).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotFound);
    }

    // --- STANDARD TESTS (Existing logic preserved) ---

    #[tokio::test]
    async fn test_multi_file_info_new_single() {
        let (dir, mfi) = setup_single_file();
        assert_eq!(mfi.files.len(), 1);
        assert_eq!(mfi.total_size, 100);
        assert_eq!(mfi.files[0].length, 100);
        assert_eq!(mfi.files[0].global_start_offset, 0);
        assert_eq!(mfi.files[0].path, dir.path().join("single_file.txt"));
        assert!(!mfi.files[0].is_padding);
    }

    #[tokio::test]
    async fn test_multi_file_info_new_multi() {
        let (dir, mfi) = setup_multi_file();
        assert_eq!(mfi.files.len(), 2);
        assert_eq!(mfi.total_size, 120);

        // File 1
        assert_eq!(mfi.files[0].length, 50);
        assert_eq!(mfi.files[0].global_start_offset, 0);
        assert_eq!(mfi.files[0].path, dir.path().join("file_a.txt"));
        assert!(!mfi.files[0].is_padding);

        // File 2
        assert_eq!(mfi.files[1].length, 70);
        assert_eq!(mfi.files[1].global_start_offset, 50);
        assert_eq!(
            mfi.files[1].path,
            dir.path().join("subdir").join("file_b.txt")
        );
        assert!(!mfi.files[1].is_padding);
    }

    #[tokio::test]
    async fn test_create_and_allocate_files_single() {
        let (_dir, mfi) = setup_single_file();
        create_and_allocate_files(&mfi).await.unwrap();

        let file_path = &mfi.files[0].path;
        assert!(tokio::fs::try_exists(file_path).await.unwrap());
        let metadata = tokio::fs::metadata(file_path).await.unwrap();
        assert_eq!(metadata.len(), 0);
    }

    #[tokio::test]
    async fn test_create_and_allocate_files_resizes_existing_short_file() {
        let (_dir, mfi) = setup_single_file();
        let file_path = &mfi.files[0].path;
        tokio::fs::write(file_path, b"partial").await.unwrap();

        let is_fresh = create_and_allocate_files(&mfi).await.unwrap();

        assert!(!is_fresh);
        let metadata = tokio::fs::metadata(file_path).await.unwrap();
        assert_eq!(metadata.len(), 100);
    }

    #[tokio::test]
    async fn test_create_and_allocate_treats_zero_byte_placeholder_as_fresh() {
        let (_dir, mfi) = setup_single_file();
        let file_path = &mfi.files[0].path;
        File::create(file_path).await.unwrap();

        let is_fresh = create_and_allocate_files(&mfi).await.unwrap();

        assert!(is_fresh);
        let metadata = tokio::fs::metadata(file_path).await.unwrap();
        assert_eq!(metadata.len(), 0);
    }

    #[tokio::test]
    async fn test_create_and_allocate_sizes_zero_placeholders_for_partial_download() {
        let (_dir, mfi) = setup_multi_file();
        tokio::fs::write(&mfi.files[0].path, b"partial")
            .await
            .unwrap();
        tokio::fs::create_dir_all(mfi.files[1].path.parent().unwrap())
            .await
            .unwrap();
        File::create(&mfi.files[1].path).await.unwrap();

        let is_fresh = create_and_allocate_files(&mfi).await.unwrap();

        assert!(!is_fresh);
        let metadata_a = tokio::fs::metadata(&mfi.files[0].path).await.unwrap();
        assert_eq!(metadata_a.len(), mfi.files[0].length);
        let metadata_b = tokio::fs::metadata(&mfi.files[1].path).await.unwrap();
        assert_eq!(metadata_b.len(), mfi.files[1].length);
    }

    #[tokio::test]
    async fn test_create_and_allocate_files_multi() {
        let (dir, mfi) = setup_multi_file();
        create_and_allocate_files(&mfi).await.unwrap();

        let file_a_path = &mfi.files[0].path;
        let file_b_path = &mfi.files[1].path;
        let subdir_path = dir.path().join("subdir");

        assert!(tokio::fs::try_exists(subdir_path).await.unwrap());
        assert!(tokio::fs::try_exists(file_a_path).await.unwrap());
        let metadata_a = tokio::fs::metadata(file_a_path).await.unwrap();
        assert_eq!(metadata_a.len(), 0);

        assert!(tokio::fs::try_exists(file_b_path).await.unwrap());
        let metadata_b = tokio::fs::metadata(file_b_path).await.unwrap();
        assert_eq!(metadata_b.len(), 0);
    }

    #[tokio::test]
    async fn test_padding_files_logic() {
        // This test verifies that padding files are correctly identified,
        // NOT created on disk, and I/O operations transparently skip them.
        let (_dir, mfi) = setup_padding_file_scenario();

        assert_eq!(mfi.files.len(), 3);
        assert!(!mfi.files[0].is_padding, "File 1 should not be padding");
        assert!(mfi.files[1].is_padding, "File 2 SHOULD be padding");
        assert!(!mfi.files[2].is_padding, "File 3 should not be padding");

        create_and_allocate_files(&mfi).await.unwrap();
        assert!(
            tokio::fs::try_exists(&mfi.files[0].path).await.unwrap(),
            "Real file 1 must exist"
        );
        assert!(
            !tokio::fs::try_exists(&mfi.files[1].path).await.unwrap(),
            "Padding file must NOT exist on disk"
        );
        assert!(
            tokio::fs::try_exists(&mfi.files[2].path).await.unwrap(),
            "Real file 2 must exist"
        );

        // We write 25 bytes starting at offset 0.
        // 0-9: Real File 1 (10 bytes)
        // 10-14: Padding (5 bytes) -> Discarded
        // 15-24: Real File 2 (10 bytes)
        let data: Vec<u8> = (0..25).collect();
        write_data_to_disk(&mfi, 0, &data).await.unwrap();

        // Read back the 25 bytes.
        // We expect: [Real Data] + [Zeros] + [Real Data]
        let read_back = read_data_from_disk(&mfi, 0, 25).await.unwrap();

        // Check first part (0-9)
        assert_eq!(read_back[0..10], data[0..10]);

        // Check padding part (10-14) - Should be Zeros, NOT the data we 'wrote'
        assert_eq!(read_back[10..15], vec![0, 0, 0, 0, 0]);

        // Check second part (15-24) - Should match original data from index 15
        assert_eq!(read_back[15..25], data[15..25]);
    }

    #[tokio::test]
    async fn test_write_read_single_file() {
        let (_dir, mfi) = setup_single_file();
        create_and_allocate_files(&mfi).await.unwrap();

        let data1: Vec<u8> = (0..20).collect(); // 20 bytes
        let data2: Vec<u8> = (20..50).collect(); // 30 bytes

        write_data_to_disk(&mfi, 10, &data1).await.unwrap();
        write_data_to_disk(&mfi, 50, &data2).await.unwrap();

        let read_data1 = read_data_from_disk(&mfi, 10, 20).await.unwrap();
        assert_eq!(data1, read_data1);

        let read_data2 = read_data_from_disk(&mfi, 50, 30).await.unwrap();
        assert_eq!(data2, read_data2);

        let empty_data = read_data_from_disk(&mfi, 0, 10).await.unwrap();
        assert_eq!(empty_data, vec![0; 10]);
    }

    #[tokio::test]
    async fn test_write_read_across_files() {
        let (_dir, mfi) = setup_multi_file(); // FileA: [0-49], FileB: [50-119]
        create_and_allocate_files(&mfi).await.unwrap();

        // Write 30 bytes starting at offset 40 (Spanning 40-69)
        let write_data: Vec<u8> = (0..30).collect();
        write_data_to_disk(&mfi, 40, &write_data).await.unwrap();

        let read_data = read_data_from_disk(&mfi, 40, 30).await.unwrap();
        assert_eq!(write_data, read_data);

        // Verify manually
        let mut file_a = File::open(&mfi.files[0].path).await.unwrap();
        file_a.seek(SeekFrom::Start(40)).await.unwrap();
        let mut buf_a = vec![0; 10];
        file_a.read_exact(&mut buf_a).await.unwrap();
        assert_eq!(buf_a, &write_data[0..10]);

        let mut file_b = File::open(&mfi.files[1].path).await.unwrap();
        let mut buf_b = vec![0; 20];
        file_b.read_exact(&mut buf_b).await.unwrap();
        assert_eq!(buf_b, &write_data[10..30]);
    }

    #[tokio::test]
    async fn test_read_out_of_bounds() {
        let (_dir, mfi) = setup_single_file(); // total_size = 100
        create_and_allocate_files(&mfi).await.unwrap();

        let res = read_data_from_disk(&mfi, 95, 10).await;
        assert!(res.is_err());
        if let Err(err) = res {
            assert!(matches!(
                err,
                StorageError::Io {
                    kind: std::io::ErrorKind::InvalidInput,
                    ..
                }
            ));
        } else {
            panic!("Expected Io Error");
        }

        let res_ok = read_data_from_disk(&mfi, 90, 10).await;
        assert!(res_ok.is_ok());
        assert_eq!(res_ok.unwrap().len(), 10);
    }

    #[tokio::test]
    async fn test_write_out_of_bounds() {
        let (_dir, mfi) = setup_single_file(); // total_size = 100
        create_and_allocate_files(&mfi).await.unwrap();

        let data = vec![1; 10];
        let res = write_data_to_disk(&mfi, 95, &data).await;
        assert!(res.is_err());
        if let Err(err) = res {
            assert!(matches!(
                err,
                StorageError::Io {
                    kind: std::io::ErrorKind::InvalidInput,
                    ..
                }
            ));
        } else {
            panic!("Expected Io Error");
        }

        let res_ok = write_data_to_disk(&mfi, 90, &data).await;
        assert!(res_ok.is_ok());

        let read_back = read_data_from_disk(&mfi, 90, 10).await.unwrap();
        assert_eq!(read_back, data);
    }

    // --- NEW PRIORITY & SKIPPING TESTS ---

    #[tokio::test]
    async fn test_create_and_allocate_skips_skipped_files() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let torrent_name = "skip_test";
        let files = vec![
            InfoFile {
                path: vec!["normal.txt".to_string()],
                length: 50,
                md5sum: None,
                attr: None,
            },
            InfoFile {
                path: vec!["skipped.txt".to_string()],
                length: 50,
                md5sum: None,
                attr: None,
            },
        ];

        // Skip index 1
        let mut priorities = HashMap::new();
        priorities.insert(1, FilePriority::Skip);

        let mfi = MultiFileInfo::new(root, torrent_name, Some(&files), None, &priorities).unwrap();

        assert!(!mfi.files[0].is_skipped);
        assert!(mfi.files[1].is_skipped);

        // WHEN: We allocate
        create_and_allocate_files(&mfi).await.unwrap();

        // THEN:
        assert!(
            tokio::fs::try_exists(&mfi.files[0].path).await.unwrap(),
            "Normal file should exist"
        );
        assert!(
            !tokio::fs::try_exists(&mfi.files[1].path).await.unwrap(),
            "Skipped file should NOT exist"
        );
    }

    #[tokio::test]
    async fn test_create_and_allocate_does_not_resize_existing_skipped_files() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let torrent_name = "skip_resize_test";
        let files = vec![InfoFile {
            path: vec!["skipped.txt".to_string()],
            length: 50,
            md5sum: None,
            attr: None,
        }];
        let mut priorities = HashMap::new();
        priorities.insert(0, FilePriority::Skip);
        let mfi = MultiFileInfo::new(root, torrent_name, Some(&files), None, &priorities).unwrap();
        tokio::fs::write(&mfi.files[0].path, b"keep").await.unwrap();

        let is_fresh = create_and_allocate_files(&mfi).await.unwrap();

        assert!(!is_fresh);
        let metadata = tokio::fs::metadata(&mfi.files[0].path).await.unwrap();
        assert_eq!(metadata.len(), 4);
        let bytes = tokio::fs::read(&mfi.files[0].path).await.unwrap();
        assert_eq!(bytes, b"keep");
    }

    #[tokio::test]
    async fn test_read_skipped_missing_file_returns_zeros() {
        // This simulates fast validation for skipped files (avoiding IO on missing files)
        let dir = tempdir().unwrap();
        let root = dir.path();
        let torrent_name = "skip_read_test";
        let files = vec![InfoFile {
            path: vec!["skipped.txt".to_string()],
            length: 100,
            md5sum: None,
            attr: None,
        }];

        let mut priorities = HashMap::new();
        priorities.insert(0, FilePriority::Skip);

        let mfi = MultiFileInfo::new(root, torrent_name, Some(&files), None, &priorities).unwrap();

        // Ensure not created
        create_and_allocate_files(&mfi).await.unwrap();
        assert!(!tokio::fs::try_exists(&mfi.files[0].path).await.unwrap());

        // WHEN: Read from missing skipped file
        let data = read_data_from_disk(&mfi, 0, 10).await.unwrap();

        // THEN: Return zeros (simulating missing data), NOT error
        assert_eq!(
            data,
            vec![0; 10],
            "Should return zeros for missing skipped file"
        );
    }

    #[tokio::test]
    async fn test_read_skipped_existing_file_returns_data() {
        // Scenario: User had file, then set Skip. We MUST read disk to know we have it.
        let dir = tempdir().unwrap();
        let root = dir.path();
        let torrent_name = "skip_exist_test";
        let files = vec![InfoFile {
            path: vec!["existing.txt".to_string()],
            length: 10,
            md5sum: None,
            attr: None,
        }];

        let mut priorities = HashMap::new();
        priorities.insert(0, FilePriority::Skip);

        let mfi = MultiFileInfo::new(root, torrent_name, Some(&files), None, &priorities).unwrap();

        // Setup: Manually create the file with data "11111..."
        {
            let mut file = File::create(&mfi.files[0].path).await.unwrap();
            file.write_all(&[1u8; 10]).await.unwrap();
        }

        // WHEN: Read from existing skipped file
        let data = read_data_from_disk(&mfi, 0, 10).await.unwrap();

        // THEN: Return actual data
        assert_eq!(
            data,
            vec![1u8; 10],
            "Should read actual data if skipped file exists"
        );
    }

    #[tokio::test]
    async fn test_write_skipped_missing_file_creates_it_lazily() {
        // Scenario: We skipped a file, so it wasn't allocated.
        // But a piece arrived that overlaps this file (boundary piece).
        // Writing to it should lazily create the file.
        let dir = tempdir().unwrap();
        let root = dir.path();
        let torrent_name = "lazy_write_test";
        let files = vec![InfoFile {
            path: vec!["lazy.txt".to_string()],
            length: 50,
            md5sum: None,
            attr: None,
        }];

        let mut priorities = HashMap::new();
        priorities.insert(0, FilePriority::Skip);

        let mfi = MultiFileInfo::new(root, torrent_name, Some(&files), None, &priorities).unwrap();

        // 1. Allocator skips it
        create_and_allocate_files(&mfi).await.unwrap();
        assert!(!tokio::fs::try_exists(&mfi.files[0].path).await.unwrap());

        // 2. We write to it (simulating boundary overlap write)
        let data = vec![0xFF; 10];
        write_data_to_disk(&mfi, 0, &data).await.unwrap();

        // 3. File should now exist and contain data
        assert!(
            tokio::fs::try_exists(&mfi.files[0].path).await.unwrap(),
            "Should lazy create skipped file on write"
        );

        let mut file = File::open(&mfi.files[0].path).await.unwrap();
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, data);
    }

    #[tokio::test]
    async fn test_mixed_priority_allocation_batch() {
        // Complex Scenario:
        // 0. Normal
        // 1. Skip
        // 2. Padding
        // 3. Normal
        let dir = tempdir().unwrap();
        let root = dir.path();
        let torrent_name = "mixed_batch";
        let files = vec![
            InfoFile {
                path: vec!["0_normal.txt".to_string()],
                length: 10,
                md5sum: None,
                attr: None,
            },
            InfoFile {
                path: vec!["1_skip.txt".to_string()],
                length: 10,
                md5sum: None,
                attr: None,
            },
            InfoFile {
                path: vec!["2_pad.txt".to_string()],
                length: 5,
                md5sum: None,
                attr: Some("p".into()),
            },
            InfoFile {
                path: vec!["3_normal.txt".to_string()],
                length: 10,
                md5sum: None,
                attr: None,
            },
        ];

        let mut priorities = HashMap::new();
        priorities.insert(1, FilePriority::Skip);

        let mfi = MultiFileInfo::new(root, torrent_name, Some(&files), None, &priorities).unwrap();

        create_and_allocate_files(&mfi).await.unwrap();

        // Checks
        assert!(
            tokio::fs::try_exists(&mfi.files[0].path).await.unwrap(),
            "Normal 0 missing"
        );
        assert!(
            !tokio::fs::try_exists(&mfi.files[1].path).await.unwrap(),
            "Skip 1 present (should be missing)"
        );
        assert!(
            !tokio::fs::try_exists(&mfi.files[2].path).await.unwrap(),
            "Padding 2 present (should be missing)"
        );
        assert!(
            tokio::fs::try_exists(&mfi.files[3].path).await.unwrap(),
            "Normal 3 missing"
        );
    }
}
