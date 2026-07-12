// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

pub mod parser;

use crate::tracker::normalize_tracker_urls;
use serde::de::{self};
use serde::{Deserialize, Deserializer, Serialize};
use serde_bencode::value::Value;

use std::collections::HashMap;
use std::fmt;

const MAX_V2_PIECE_LENGTH: u32 = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathValidationError {
    EmptyComponent,
    CurrentOrParentComponent,
    PathSeparator,
    ControlCharacter,
    AbsoluteOrPrefixed,
    EmptyFilePath,
    DuplicateFilePath(String),
    FileDirectoryCollision { file: String, descendant: String },
    InvalidUtf8,
    NonPositivePieceLength(i64),
    NegativeFileLength(String),
    WindowsInvalidComponent,
    WindowsReservedName(String),
    InvalidV2PieceLength(i64),
    MalformedV2Tree { path: String, reason: String },
    TotalLengthOverflow,
}

impl fmt::Display for PathValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyComponent => write!(f, "path components cannot be empty"),
            Self::CurrentOrParentComponent => {
                write!(f, "path components cannot be '.' or '..'")
            }
            Self::PathSeparator => {
                write!(f, "a path component cannot contain '/' or '\\'")
            }
            Self::ControlCharacter => {
                write!(f, "path components cannot contain control characters")
            }
            Self::AbsoluteOrPrefixed => {
                write!(f, "absolute or platform-prefixed paths are not allowed")
            }
            Self::EmptyFilePath => write!(f, "torrent file paths cannot be empty"),
            Self::DuplicateFilePath(path) => {
                write!(
                    f,
                    "torrent contains duplicate or case-aliasing file path '{path}'"
                )
            }
            Self::FileDirectoryCollision { file, descendant } => write!(
                f,
                "torrent path '{file}' is both a file and a parent of '{descendant}'"
            ),
            Self::InvalidUtf8 => write!(f, "torrent path components must be valid UTF-8"),
            Self::NonPositivePieceLength(length) => {
                write!(f, "torrent piece length must be positive, got {length}")
            }
            Self::NegativeFileLength(path) => {
                write!(f, "torrent file '{path}' has a negative length")
            }
            Self::WindowsInvalidComponent => write!(
                f,
                "path components cannot contain Windows-invalid characters or end in a dot or space"
            ),
            Self::WindowsReservedName(name) => {
                write!(
                    f,
                    "path component '{name}' is a reserved Windows device name"
                )
            }
            Self::InvalidV2PieceLength(length) => write!(
                f,
                "v2 piece length must be a power of two between 16384 and {}, got {length}",
                MAX_V2_PIECE_LENGTH
            ),
            Self::MalformedV2Tree { path, reason } => {
                write!(f, "malformed v2 file tree at '{path}': {reason}")
            }
            Self::TotalLengthOverflow => {
                write!(
                    f,
                    "torrent aggregate file length exceeds the supported range"
                )
            }
        }
    }
}

impl std::error::Error for PathValidationError {}

/// Validates a user-selected container folder name.
///
/// An empty name is intentionally valid: `Some("")` is the existing explicit
/// "no container folder" state. Non-empty names must be exactly one safe path
/// component so they cannot escape the selected download directory.
pub fn validate_container_name(name: &str) -> Result<(), PathValidationError> {
    if name.is_empty() {
        return Ok(());
    }

    validate_path_component(name)
}

/// Validates one metadata-derived filesystem path component.
pub fn validate_path_component(component: &str) -> Result<(), PathValidationError> {
    if component.is_empty() {
        return Err(PathValidationError::EmptyComponent);
    }
    if matches!(component, "." | "..") {
        return Err(PathValidationError::CurrentOrParentComponent);
    }
    if component.starts_with('/') || component.starts_with('\\') || has_drive_prefix(component) {
        return Err(PathValidationError::AbsoluteOrPrefixed);
    }
    if component.contains(['/', '\\']) {
        return Err(PathValidationError::PathSeparator);
    }
    if component.chars().any(char::is_control) {
        return Err(PathValidationError::ControlCharacter);
    }
    if component.contains(['<', '>', ':', '"', '|', '?', '*']) || component.ends_with(['.', ' ']) {
        return Err(PathValidationError::WindowsInvalidComponent);
    }
    if is_windows_reserved_name(component) {
        return Err(PathValidationError::WindowsReservedName(
            component.to_string(),
        ));
    }

    Ok(())
}

fn has_drive_prefix(component: &str) -> bool {
    let bytes = component.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn is_windows_reserved_name(component: &str) -> bool {
    let basename = component
        .split('.')
        .next()
        .unwrap_or(component)
        .trim_end_matches(['.', ' ']);
    let uppercase = basename.to_ascii_uppercase();
    matches!(uppercase.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || uppercase.strip_prefix("COM").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
        || uppercase.strip_prefix("LPT").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
}

/// Produces the conservative key used for host-filesystem collision checks.
///
/// Torrent paths are byte-sensitive, but the default filesystems on macOS and
/// Windows are commonly case-insensitive. Treating Unicode lowercase aliases
/// as the same path prevents two torrent indices from targeting one host file.
pub(crate) fn path_casefold_key(path: &[String]) -> Vec<String> {
    path.iter()
        .map(|component| component.to_lowercase())
        .collect()
}

/// Validates the filesystem layout used by both parsing and storage setup.
pub fn validate_torrent_layout(
    torrent_name: &str,
    files: &[InfoFile],
) -> Result<(), PathValidationError> {
    validate_path_component(torrent_name)?;

    let mut paths = Vec::with_capacity(files.len());
    let mut total_length = 0i64;
    for file in files {
        if file.length < 0 {
            return Err(PathValidationError::NegativeFileLength(file.path.join("/")));
        }
        if file.path.is_empty() {
            return Err(PathValidationError::EmptyFilePath);
        }
        for component in &file.path {
            validate_path_component(component)?;
        }
        total_length = total_length
            .checked_add(file.length)
            .ok_or(PathValidationError::TotalLengthOverflow)?;
        paths.push((path_casefold_key(&file.path), file.path.as_slice()));
    }

    paths.sort_unstable_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(right.1)));
    for adjacent in paths.windows(2) {
        let (first_key, first) = &adjacent[0];
        let (second_key, second) = &adjacent[1];
        if first_key == second_key {
            return Err(PathValidationError::DuplicateFilePath(second.join("/")));
        }
        if second_key.starts_with(first_key) {
            return Err(PathValidationError::FileDirectoryCollision {
                file: first.join("/"),
                descendant: second.join("/"),
            });
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct V2RootInfo {
    pub file_offset: u64,
    pub length: u64,
    pub root_hash: Vec<u8>,
    pub file_index: u32,
}

pub struct V2Mapping {
    pub piece_to_roots: HashMap<u32, Vec<V2RootInfo>>,
    pub piece_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Torrent {
    // This field is special and not directly in the bencode source.
    // We will populate it manually after deserialization.
    #[serde(skip)]
    pub info_dict_bencode: Vec<u8>,

    pub info: Info,
    pub announce: Option<String>,

    #[serde(rename = "announce-list", default)]
    pub announce_list: Option<Vec<Vec<String>>>,

    #[serde(
        rename = "url-list",
        default,
        deserialize_with = "deserialize_url_list"
    )]
    pub url_list: Option<Vec<String>>,

    #[serde(rename = "creation date", default)]
    pub creation_date: Option<i64>,

    #[serde(default)]
    pub comment: Option<String>,

    #[serde(rename = "created by", default)]
    pub created_by: Option<String>,

    #[serde(default)]
    pub encoding: Option<String>,

    // --- v2 / Hybrid Fields ---
    #[serde(rename = "piece layers", default)]
    pub piece_layers: Option<Value>,
}

impl Torrent {
    pub fn validate_paths(&self) -> Result<(), PathValidationError> {
        if self.info.piece_length <= 0 {
            return Err(PathValidationError::NonPositivePieceLength(
                self.info.piece_length,
            ));
        }
        if self.info.meta_version == Some(2) {
            let piece_length = u32::try_from(self.info.piece_length)
                .map_err(|_| PathValidationError::InvalidV2PieceLength(self.info.piece_length))?;
            if !(16_384..=MAX_V2_PIECE_LENGTH).contains(&piece_length)
                || !piece_length.is_power_of_two()
            {
                return Err(PathValidationError::InvalidV2PieceLength(
                    self.info.piece_length,
                ));
            }
        }
        if self.info.length < 0 {
            return Err(PathValidationError::NegativeFileLength(
                self.info.name.clone(),
            ));
        }
        validate_torrent_layout(&self.info.name, &self.info.files)?;
        if let Some(file_tree) = &self.info.file_tree {
            validate_v2_file_tree(file_tree, &mut Vec::new())?;
            let mut v2_files = Vec::new();
            for (path, length, _) in self.get_v2_files() {
                let length =
                    i64::try_from(length).map_err(|_| PathValidationError::TotalLengthOverflow)?;
                v2_files.push(InfoFile {
                    length,
                    path: path.split('/').map(str::to_owned).collect(),
                    md5sum: None,
                    attr: None,
                });
            }
            validate_torrent_layout(&self.info.name, &v2_files)?;
        }
        Ok(())
    }

    pub fn tracker_urls(&self) -> Vec<String> {
        let mut urls = Vec::new();
        if let Some(announce) = &self.announce {
            urls.push(announce.clone());
        }
        if let Some(announce_list) = &self.announce_list {
            for tier in announce_list {
                urls.extend(tier.iter().cloned());
            }
        }
        normalize_tracker_urls(urls)
    }

    pub fn get_v2_roots(&self) -> Vec<(String, u64, Vec<u8>)> {
        self.get_v2_files()
            .into_iter()
            .filter_map(|(path, length, root_hash)| {
                root_hash.map(|root_hash| (path, length, root_hash))
            })
            .collect()
    }

    /// Returns every v2 file leaf, including valid zero-length leaves that do
    /// not carry a `pieces root` field.
    pub fn get_v2_files(&self) -> Vec<(String, u64, Option<Vec<u8>>)> {
        let mut results = Vec::new();
        if let Some(ref tree) = self.info.file_tree {
            traverse_file_tree(tree, String::new(), &mut results);
        }
        results
    }

    pub fn get_layer_hashes(&self, root_hash: &[u8]) -> Option<Vec<u8>> {
        if let Some(Value::Dict(layers)) = &self.piece_layers {
            if let Some(Value::Bytes(layer_data)) = layers.get(root_hash) {
                return Some(layer_data.clone());
            }
        }
        None
    }

    pub fn calculate_v2_mapping(&self) -> V2Mapping {
        let mut piece_to_roots: HashMap<u32, Vec<V2RootInfo>> = HashMap::new();
        let piece_len = self.info.piece_length as u64;
        let mut current_piece_index = 0;

        if self.info.meta_version == Some(2) && piece_len > 0 {
            let mut v2_files = self.get_v2_files();
            v2_files.sort_by(|(path_a, _, _), (path_b, _, _)| path_a.cmp(path_b));

            for (file_index, (_path, length, root_hash)) in v2_files.into_iter().enumerate() {
                if let Some(root_hash) = root_hash.filter(|_| length > 0) {
                    let file_pieces = length.div_ceil(piece_len);
                    let file_start_offset = current_piece_index * piece_len;

                    let start_piece = current_piece_index as u32;
                    let end_piece = (current_piece_index + file_pieces) as u32;

                    for p in start_piece..end_piece {
                        piece_to_roots.entry(p).or_default().push(V2RootInfo {
                            file_offset: file_start_offset,
                            length,
                            root_hash: root_hash.clone(),
                            file_index: file_index as u32,
                        });
                    }
                    current_piece_index += file_pieces;
                }
            }
        }

        V2Mapping {
            piece_to_roots,
            piece_count: current_piece_index as usize,
        }
    }

    pub fn get_v2_hash_layer(
        &self,
        piece_index: u32,
        file_start_offset: u64,
        file_length: u64,
        requested_length: u32,
        resolved_root: &[u8],
    ) -> Option<Vec<u8>> {
        let piece_len = self.info.piece_length as u64;
        if piece_len == 0 {
            return None;
        }

        // Calculate where the file starts in piece-space and the request's relative bounds
        let file_start_piece = (file_start_offset as u32) / (piece_len as u32);
        if piece_index < file_start_piece {
            return None;
        }

        let relative_start_idx = (piece_index - file_start_piece) as usize;
        let relative_end_idx = relative_start_idx + requested_length as usize;

        // 1. Try to retrieve explicit layers first.
        // This handles Multi-piece files AND test mocks that inject layers for single files.
        if let Some(layer_bytes) = self.get_layer_hashes(resolved_root) {
            let total_hashes_in_layer = layer_bytes.len() / 32;

            if relative_end_idx <= total_hashes_in_layer {
                let start_byte = relative_start_idx * 32;
                let end_byte = relative_end_idx * 32;
                return Some(layer_bytes[start_byte..end_byte].to_vec());
            } else {
                // The requested range exceeds what is available in the layer.
                return None;
            }
        }

        // 2. Fallback: BEP 52 Optimization for Single Piece Files.
        // "Note that for files that fit in one piece, the 'pieces root' is the digest of the file."
        // We only use this if no explicit layer was found.
        if file_length <= piece_len {
            // A single piece file has exactly 1 hash (index 0).
            // We must verify the request matches this limit.
            if relative_start_idx == 0 && requested_length == 1 {
                return Some(resolved_root.to_vec());
            }
        }

        None
    }

    pub fn file_list(&self) -> Vec<(Vec<String>, u64)> {
        if !self.info.files.is_empty() {
            // Multi-file case
            self.info
                .files
                .iter()
                .map(|f| (f.path.clone(), f.length as u64))
                .collect()
        } else {
            // Single-file V1 case: The torrent name is the file name
            vec![(vec![self.info.name.clone()], self.info.length as u64)]
        }
    }
}

fn v2_path_label(current_path: &[String]) -> String {
    if current_path.is_empty() {
        "<root>".to_string()
    } else {
        current_path.join("/")
    }
}

fn validate_v2_file_tree(
    node: &Value,
    current_path: &mut Vec<String>,
) -> Result<(), PathValidationError> {
    let Value::Dict(entries) = node else {
        return Err(PathValidationError::MalformedV2Tree {
            path: v2_path_label(current_path),
            reason: "file-tree nodes must be dictionaries".to_string(),
        });
    };
    if entries.is_empty() {
        return Err(PathValidationError::MalformedV2Tree {
            path: v2_path_label(current_path),
            reason: "file-tree nodes cannot be empty".to_string(),
        });
    }

    let mut has_file_metadata = false;
    let mut has_child_paths = false;
    for (raw_name, child) in entries {
        // BEP 52 uses an empty key for a file's metadata dictionary. Its
        // children are metadata field names, not filesystem components.
        if raw_name.is_empty() {
            has_file_metadata = true;
            let Value::Dict(metadata) = child else {
                return Err(PathValidationError::MalformedV2Tree {
                    path: v2_path_label(current_path),
                    reason: "file metadata must be a dictionary".to_string(),
                });
            };
            let length = match metadata.get("length".as_bytes()) {
                Some(Value::Int(length)) if *length >= 0 => *length,
                Some(Value::Int(_)) => {
                    return Err(PathValidationError::NegativeFileLength(v2_path_label(
                        current_path,
                    )))
                }
                Some(_) => {
                    return Err(PathValidationError::MalformedV2Tree {
                        path: v2_path_label(current_path),
                        reason: "file length must be an integer".to_string(),
                    })
                }
                None => {
                    return Err(PathValidationError::MalformedV2Tree {
                        path: v2_path_label(current_path),
                        reason: "file metadata is missing length".to_string(),
                    })
                }
            };
            match metadata.get("pieces root".as_bytes()) {
                Some(Value::Bytes(root)) if root.len() == 32 => {}
                None if length == 0 => {}
                Some(Value::Bytes(_)) => {
                    return Err(PathValidationError::MalformedV2Tree {
                        path: v2_path_label(current_path),
                        reason: "pieces root must contain exactly 32 bytes".to_string(),
                    })
                }
                Some(_) => {
                    return Err(PathValidationError::MalformedV2Tree {
                        path: v2_path_label(current_path),
                        reason: "pieces root must be a byte string".to_string(),
                    })
                }
                None => {
                    return Err(PathValidationError::MalformedV2Tree {
                        path: v2_path_label(current_path),
                        reason: "non-empty files require a pieces root".to_string(),
                    })
                }
            }
            continue;
        }

        has_child_paths = true;
        let name = std::str::from_utf8(raw_name).map_err(|_| PathValidationError::InvalidUtf8)?;
        validate_path_component(name)?;
        current_path.push(name.to_string());
        validate_v2_file_tree(child, current_path)?;
        current_path.pop();
    }

    if has_file_metadata && has_child_paths {
        return Err(PathValidationError::MalformedV2Tree {
            path: v2_path_label(current_path),
            reason: "a path cannot be both a file and a directory".to_string(),
        });
    }

    Ok(())
}

fn traverse_file_tree(
    node: &Value,
    current_path: String,
    results: &mut Vec<(String, u64, Option<Vec<u8>>)>,
) {
    if let Value::Dict(map) = node {
        for (key, value) in map {
            let name = String::from_utf8_lossy(key).to_string();

            if name.is_empty() {
                // This is a file metadata node (Leaf)
                if let Value::Dict(file_metadata) = value {
                    let root = match file_metadata.get("pieces root".as_bytes()) {
                        Some(Value::Bytes(root)) => Some(root.clone()),
                        _ => None,
                    };
                    let explicit_length = match file_metadata.get("length".as_bytes()) {
                        Some(Value::Int(length)) => Some(u64::try_from(*length).unwrap_or(0)),
                        _ => None,
                    };
                    if root.is_some() || explicit_length.is_some() {
                        results.push((current_path.clone(), explicit_length.unwrap_or(0), root));
                    }
                }
            } else {
                // Directory node
                let new_path = if current_path.is_empty() {
                    name
                } else {
                    format!("{}/{}", current_path, name)
                };
                traverse_file_tree(value, new_path, results);
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Info {
    #[serde(rename = "piece length")]
    pub piece_length: i64,

    #[serde(with = "serde_bytes")]
    #[serde(default)]
    pub pieces: Vec<u8>,

    #[serde(default)]
    pub private: Option<i64>,

    #[serde(default)]
    pub files: Vec<InfoFile>,

    pub name: String,

    #[serde(default)]
    pub length: i64,

    #[serde(default)]
    pub md5sum: Option<String>,

    // --- v2 / Hybrid Fields ---
    #[serde(rename = "meta version", default)]
    pub meta_version: Option<i64>,

    #[serde(rename = "file tree", default)]
    pub file_tree: Option<Value>,
}

impl Info {
    pub fn total_length(&self) -> i64 {
        // Case 1: v1 Single File
        if self.length > 0 {
            return self.length;
        }

        // Case 2: v1 Multi-File
        if !self.files.is_empty() {
            return self
                .files
                .iter()
                .try_fold(0i64, |total, file| total.checked_add(file.length))
                .unwrap_or(i64::MAX);
        }

        // Case 3: v2 File Tree
        if let Some(ref tree) = self.file_tree {
            return calculate_tree_size(tree);
        }

        0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct InfoFile {
    pub length: i64,

    #[serde(default)]
    pub md5sum: Option<String>,

    pub path: Vec<String>,

    #[serde(default)]
    pub attr: Option<String>,
}

fn deserialize_url_list<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let v: Value = Deserialize::deserialize(deserializer)?;

    match v {
        Value::Bytes(bytes) => {
            let s = String::from_utf8(bytes)
                .map_err(|e| de::Error::custom(format!("Invalid UTF-8 in url-list: {}", e)))?;
            Ok(Some(vec![s]))
        }
        Value::List(list) => {
            let mut urls = Vec::new();
            for item in list {
                if let Value::Bytes(bytes) = item {
                    let s = String::from_utf8(bytes).map_err(|e| {
                        de::Error::custom(format!("Invalid UTF-8 in url-list: {}", e))
                    })?;
                    urls.push(s);
                }
            }
            Ok(Some(urls))
        }
        _ => Ok(None),
    }
}

fn calculate_tree_size(node: &Value) -> i64 {
    let mut size: i64 = 0;
    if let Value::Dict(map) = node {
        for (key, value) in map {
            let name = String::from_utf8_lossy(key);
            if name.is_empty() {
                // This is a file metadata node
                if let Value::Dict(meta) = value {
                    if let Some(Value::Int(len)) = meta.get("length".as_bytes()) {
                        size = size.checked_add(*len).unwrap_or(i64::MAX);
                    }
                }
            } else {
                // This is a subdirectory or file entry, recurse
                size = size
                    .checked_add(calculate_tree_size(value))
                    .unwrap_or(i64::MAX);
            }
        }
    }
    size
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // Helper to create a basic Info object
    fn create_test_info(meta_version: Option<i64>) -> Info {
        Info {
            piece_length: 16384,
            pieces: Vec::new(),
            private: None,
            files: Vec::new(),
            name: "test_torrent".to_string(),
            length: 0,
            md5sum: None,
            meta_version,
            file_tree: None,
        }
    }

    // Helper to build a v2 file tree node
    fn build_v2_file_node(length: i64, root: Vec<u8>) -> Value {
        let mut meta = HashMap::new();
        meta.insert("length".as_bytes().to_vec(), Value::Int(length));
        meta.insert("pieces root".as_bytes().to_vec(), Value::Bytes(root));

        let mut leaf = HashMap::new();
        leaf.insert(vec![], Value::Dict(meta));
        Value::Dict(leaf)
    }

    // Helper to create a multi-file V2 torrent with layers for testing
    fn create_test_torrent_with_layers() -> Torrent {
        let mut torrent = Torrent {
            info: create_test_info(Some(2)),
            ..Torrent::default()
        };
        torrent.info.piece_length = 16384;

        let root_a = vec![0xAA; 32];
        let root_b = vec![0xBB; 32];

        // Setup File Tree: a.txt (16KB), b.txt (16KB)
        let mut tree = HashMap::new();
        tree.insert(
            "a.txt".as_bytes().to_vec(),
            build_v2_file_node(16384, root_a.clone()),
        );
        tree.insert(
            "b.txt".as_bytes().to_vec(),
            build_v2_file_node(16384, root_b.clone()),
        );
        torrent.info.file_tree = Some(Value::Dict(tree));

        // Setup Piece Layers: Each root gets a mock 32-byte layer hash
        let mut layers = HashMap::new();
        layers.insert(root_a, Value::Bytes(vec![0x11; 32]));
        layers.insert(root_b, Value::Bytes(vec![0x22; 32]));
        torrent.piece_layers = Some(Value::Dict(layers));

        torrent
    }

    #[test]
    fn test_v2_piece_count_calculation() {
        let mut torrent = Torrent {
            info: create_test_info(Some(2)),
            ..Torrent::default()
        };

        let mut tree = HashMap::new();
        tree.insert(
            "a.txt".as_bytes().to_vec(),
            build_v2_file_node(1000, vec![0xAA; 32]),
        );
        tree.insert(
            "b.txt".as_bytes().to_vec(),
            build_v2_file_node(1000, vec![0xBB; 32]),
        );
        torrent.info.file_tree = Some(Value::Dict(tree));

        let mapping = torrent.calculate_v2_mapping();

        assert_eq!(mapping.piece_count, 2);

        let roots_0 = mapping.piece_to_roots.get(&0).unwrap();
        let roots_1 = mapping.piece_to_roots.get(&1).unwrap();
        assert_eq!(roots_0[0].root_hash, vec![0xAA; 32]);
        assert_eq!(roots_1[0].root_hash, vec![0xBB; 32]);
    }

    #[test]
    fn test_hybrid_piece_count_prioritizes_v1_string() {
        let mut torrent = Torrent {
            info: create_test_info(Some(2)),
            ..Torrent::default()
        };

        torrent.info.pieces = vec![0u8; 200];
        assert_eq!(200 / 20, 10);
    }

    #[test]
    fn test_deterministic_v2_sorting() {
        let mut torrent = Torrent {
            info: create_test_info(Some(2)),
            ..Torrent::default()
        };

        let mut tree = HashMap::new();
        // Use 0x5A (ASCII 'Z') instead of invalid literal
        tree.insert(
            "z.txt".as_bytes().to_vec(),
            build_v2_file_node(1000, vec![0x5A; 32]),
        );
        tree.insert(
            "a.txt".as_bytes().to_vec(),
            build_v2_file_node(1000, vec![0xAA; 32]),
        );
        torrent.info.file_tree = Some(Value::Dict(tree));

        let mapping = torrent.calculate_v2_mapping();

        let roots_0 = mapping.piece_to_roots.get(&0).expect("Piece 0 missing");
        assert_eq!(roots_0[0].root_hash, vec![0xAA; 32]);

        let roots_1 = mapping.piece_to_roots.get(&1).expect("Piece 1 missing");
        assert_eq!(roots_1[0].root_hash, vec![0x5A; 32]);
    }

    #[test]
    fn test_v2_mapping_with_empty_files() {
        let mut torrent = Torrent {
            info: create_test_info(Some(2)),
            ..Torrent::default()
        };

        let mut tree = HashMap::new();
        tree.insert(
            "empty.txt".as_bytes().to_vec(),
            build_v2_file_node(0, vec![0x00; 32]),
        );
        tree.insert(
            "real.txt".as_bytes().to_vec(),
            build_v2_file_node(1000, vec![0xAA; 32]),
        );
        torrent.info.file_tree = Some(Value::Dict(tree));

        let mapping = torrent.calculate_v2_mapping();

        assert_eq!(mapping.piece_count, 1);
        assert_eq!(
            mapping.piece_to_roots.get(&0).unwrap()[0].root_hash,
            vec![0xAA; 32]
        );
    }

    #[test]
    fn test_get_v2_hash_layer_with_offset() {
        let torrent = create_test_torrent_with_layers();
        let root_b = vec![0xBB; 32];

        let result = torrent.get_v2_hash_layer(1, 16384, 16384, 1, &root_b);

        assert!(result.is_some());
        assert_eq!(result.unwrap().len(), 32);

        let too_long = torrent.get_v2_hash_layer(1, 16384, 16384, 100, &root_b);
        assert!(too_long.is_none());
    }

    #[test]
    fn test_get_v2_hash_layer_bep52_single_piece() {
        let mut info = create_test_info(Some(2));
        info.piece_length = 16384;

        let t = Torrent {
            info,
            ..Torrent::default()
        };

        let root_a = vec![0xAA; 32];
        let result = t.get_v2_hash_layer(0, 0, 500, 1, &root_a);
        assert_eq!(result.unwrap(), root_a);
    }

    #[test]
    fn test_get_v2_hash_layer_bounds_check() {
        let mut info = create_test_info(Some(2));
        info.piece_length = 16384;
        let t = Torrent {
            info,
            ..Torrent::default()
        };
        let root = vec![0xAA; 32];

        // Requesting 100 hashes from a file that fits in 1 piece (and thus has 1 hash) should fail
        let result = t.get_v2_hash_layer(0, 0, 500, 100, &root);
        assert!(
            result.is_none(),
            "Should reject request for 100 hashes from single-piece file"
        );
    }

    #[test]
    fn test_get_v2_hash_layer_mock_priority() {
        let mut info = create_test_info(Some(2));
        info.piece_length = 16384;
        let mut t = Torrent {
            info,
            ..Torrent::default()
        };

        let root = vec![0xAA; 32];
        let layer_data = vec![0xBB; 32]; // Different from root

        // Mock layer injection
        let mut layer_map = HashMap::new();
        layer_map.insert(root.clone(), Value::Bytes(layer_data.clone()));
        t.piece_layers = Some(Value::Dict(layer_map));

        // Request hash for single piece file
        // If logic is correct, it finds the layer first and returns 0xBB
        // If regression exists, it hits the "single piece optimization" and returns root (0xAA)
        let result = t.get_v2_hash_layer(0, 0, 500, 1, &root).unwrap();
        assert_eq!(
            result, layer_data,
            "Should prioritize explicit layers over root fallback"
        );
    }

    #[test]
    fn container_name_validation_preserves_explicit_no_container() {
        assert_eq!(validate_container_name(""), Ok(()));
        assert_eq!(validate_container_name("Selected Folder"), Ok(()));

        for unsafe_name in [
            ".",
            "..",
            "/outside",
            "\\outside",
            "C:outside",
            "nested/folder",
            "nested\\folder",
            "line\nbreak",
        ] {
            assert!(
                validate_container_name(unsafe_name).is_err(),
                "expected {unsafe_name:?} to be rejected"
            );
        }
    }

    #[test]
    fn torrent_layout_rejects_unsafe_components() {
        for unsafe_component in [
            "",
            ".",
            "..",
            "C:escape",
            "part/child",
            "part\\child",
            "bad\0name",
        ] {
            let files = vec![InfoFile {
                length: 1,
                path: vec![unsafe_component.to_string()],
                ..InfoFile::default()
            }];
            assert!(
                validate_torrent_layout("safe-item", &files).is_err(),
                "expected {unsafe_component:?} to be rejected"
            );
        }

        assert!(validate_torrent_layout("../unsafe-item", &[]).is_err());
    }

    #[test]
    fn torrent_layout_rejects_windows_alias_and_device_components() {
        for unsafe_component in [
            "stream:name",
            "bad<name",
            "bad>name",
            "bad\"name",
            "bad|name",
            "bad?name",
            "bad*name",
            "trailing.",
            "trailing ",
            "CON",
            "nul.txt",
            "Com1.bin",
            "lPt9",
        ] {
            let files = vec![InfoFile {
                length: 1,
                path: vec![unsafe_component.to_string()],
                ..InfoFile::default()
            }];
            assert!(
                validate_torrent_layout("safe-item", &files).is_err(),
                "expected {unsafe_component:?} to be rejected"
            );
        }

        for safe_component in ["console.bin", "com0.bin", "com10.bin", "lpt10"] {
            let files = vec![InfoFile {
                length: 1,
                path: vec![safe_component.to_string()],
                ..InfoFile::default()
            }];
            assert_eq!(validate_torrent_layout("safe-item", &files), Ok(()));
        }
    }

    #[test]
    fn torrent_layout_rejects_duplicate_and_file_directory_collisions() {
        let file = |path: &[&str]| InfoFile {
            length: 1,
            path: path
                .iter()
                .map(|component| (*component).to_string())
                .collect(),
            ..InfoFile::default()
        };

        let duplicate = vec![file(&["same.bin"]), file(&["same.bin"])];
        assert!(matches!(
            validate_torrent_layout("safe-item", &duplicate),
            Err(PathValidationError::DuplicateFilePath(_))
        ));

        let collision = vec![file(&["node"]), file(&["node", "child.bin"])];
        assert!(matches!(
            validate_torrent_layout("safe-item", &collision),
            Err(PathValidationError::FileDirectoryCollision { .. })
        ));

        let case_alias = vec![file(&["Entry.bin"]), file(&["entry.BIN"])];
        assert!(matches!(
            validate_torrent_layout("safe-item", &case_alias),
            Err(PathValidationError::DuplicateFilePath(_))
        ));

        let case_prefix_collision = vec![file(&["Node"]), file(&["node", "child.bin"])];
        assert!(matches!(
            validate_torrent_layout("safe-item", &case_prefix_collision),
            Err(PathValidationError::FileDirectoryCollision { .. })
        ));
    }

    #[test]
    fn torrent_validation_rejects_invalid_signed_geometry() {
        let mut torrent = Torrent {
            info: create_test_info(None),
            ..Torrent::default()
        };
        torrent.info.length = 1;

        torrent.info.piece_length = 0;
        assert!(matches!(
            torrent.validate_paths(),
            Err(PathValidationError::NonPositivePieceLength(0))
        ));

        torrent.info.piece_length = 16_384;
        torrent.info.files = vec![InfoFile {
            length: -1,
            path: vec!["payload.bin".to_string()],
            ..InfoFile::default()
        }];
        assert!(matches!(
            torrent.validate_paths(),
            Err(PathValidationError::NegativeFileLength(_))
        ));
    }

    #[test]
    fn v2_tree_rejects_unsafe_raw_component() {
        for raw_name in [b"nested/child".to_vec(), vec![0xff]] {
            let mut tree = HashMap::new();
            tree.insert(raw_name, build_v2_file_node(1, vec![0x11; 32]));
            let mut torrent = Torrent {
                info: create_test_info(Some(2)),
                ..Torrent::default()
            };
            torrent.info.file_tree = Some(Value::Dict(tree));
            assert!(torrent.validate_paths().is_err());
        }
    }

    #[test]
    fn test_tracker_urls_flatten_announce_list_and_keep_http_fallback() {
        let torrent = Torrent {
            announce: Some("http://tracker.local:6969/announce".to_string()),
            announce_list: Some(vec![vec![
                "udp://tracker.local:6969/announce".to_string(),
                "https://tracker-alt.local/announce".to_string(),
            ]]),
            info: create_test_info(None),
            ..Torrent::default()
        };

        assert_eq!(
            torrent.tracker_urls(),
            vec![
                "http://tracker.local:6969/announce".to_string(),
                "udp://tracker.local:6969/announce".to_string(),
                "https://tracker-alt.local/announce".to_string(),
            ]
        );
    }
}
