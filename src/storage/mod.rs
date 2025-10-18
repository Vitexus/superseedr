// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::errors::StorageError;
use std::path::Path;
use std::path::PathBuf;
use tokio::fs::{self, try_exists, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};

use crate::torrent_file::InfoFile;

#[derive(Debug, Clone)]
pub struct FileInfo {
    pub path: PathBuf, // The full path to the file on the disk.
    pub length: u64, // The length of the file in bytes.
    pub global_start_offset: u64, // The starting offset of this file within the torrent's complete data stream.
}

/// Manages the file layout for a torrent, abstracting away the difference
/// between single and multi-file torrents.
#[derive(Debug, Clone)]
pub struct MultiFileInfo {
    pub files: Vec<FileInfo>,
    pub total_size: u64,
}

impl MultiFileInfo {
    /// Creates a new MultiFileInfo map. This is the central point of unification.
    /// It intelligently handles both single and multi-file torrent metadata.
    pub fn new(
        root_dir: &Path,
        torrent_name: &str,
        files: Option<&Vec<InfoFile>>,
        length: Option<u64>,
    ) -> Result<Self, std::io::Error> {
        if let Some(torrent_files) = files {
            let mut files_vec = Vec::new();
            let mut current_offset = 0;

            for f in torrent_files {
                let mut full_path = root_dir.to_path_buf();
                // The path in the torrent metadata can contain subdirectories.
                for component in &f.path {
                    full_path.push(component);
                }

                files_vec.push(FileInfo {
                    path: full_path,
                    length: f.length as u64,
                    global_start_offset: current_offset,
                });

                current_offset += f.length as u64;
            }
            Ok(Self {
                files: files_vec,
                total_size: current_offset,
            })
        } else {
            let total_size = length.unwrap_or(0);
            let file_path = root_dir.join(torrent_name);
            let single_file = FileInfo {
                path: file_path,
                length: total_size,
                global_start_offset: 0,
            };
            Ok(Self {
                files: vec![single_file],
                total_size,
            })
        }
    }
}

/// Creates all necessary directories and pre-allocates all files for a torrent.
/// This function works for both single and multi-file torrents.
pub async fn create_and_allocate_files(
    multi_file_info: &MultiFileInfo,
) -> Result<(), StorageError> {
    for file_info in &multi_file_info.files {
        // Ensure the parent directory for the file exists.
        if let Some(parent_dir) = file_info.path.parent() {
            if !try_exists(parent_dir).await? {
                fs::create_dir_all(parent_dir).await?;
            }
        }

        // Create and set the length of the file if it doesn't exist.
        if !try_exists(&file_info.path).await? {
            let file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&file_info.path)
                .await?;
            file.set_len(file_info.length).await?;
        }
    }
    Ok(())
}

pub async fn read_data_from_disk(
    multi_file_info: &MultiFileInfo,
    global_offset: u64,
    bytes_to_read: usize,
) -> Result<Vec<u8>, StorageError> {
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
                let mut file = File::open(&file_info.path).await?;
                file.seek(SeekFrom::Start(local_offset)).await?;

                let mut temp_buf = vec![0; bytes_to_read_in_this_file];
                file.read_exact(&mut temp_buf).await?;
                buffer.extend_from_slice(&temp_buf);

                bytes_read += bytes_to_read_in_this_file;
            }

            if bytes_read == bytes_to_read {
                return Ok(buffer);
            }
        }
    }

    Err(StorageError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "Failed to read all data, offset likely out of bounds",
    )))
}

pub async fn write_data_to_disk(
    multi_file_info: &MultiFileInfo,
    global_offset: u64,
    data_to_write: &[u8],
) -> Result<(), StorageError> {
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
                let mut file = OpenOptions::new().write(true).open(&file_info.path).await?;
                file.seek(SeekFrom::Start(local_offset)).await?;

                let data_slice =
                    &data_to_write[bytes_written..bytes_written + bytes_to_write_in_this_file];
                file.write_all(data_slice).await?;

                bytes_written += bytes_to_write_in_this_file;
            }

            if bytes_written == data_len {
                return Ok(());
            }
        }
    }

    Err(StorageError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "Failed to write all data, offset likely out of bounds",
    )))
}
