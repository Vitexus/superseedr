// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::torrent_file::Torrent;
use serde_bencode::de;
use serde_bencode::value::Value;

use std::fmt;

#[derive(Debug)]
pub enum ParseError {
    Bencode(serde_bencode::Error),
    MissingInfoDict,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ParseError::Bencode(e) => write!(f, "Bencode parsing error: {}", e),
            ParseError::MissingInfoDict => write!(f, "Missing 'info' dictionary in torrent file"),
        }
    }
}

impl std::error::Error for ParseError {}

impl From<serde_bencode::Error> for ParseError {
    fn from(e: serde_bencode::Error) -> Self {
        ParseError::Bencode(e)
    }
}

pub fn polyfill_v2_files(torrent: &mut Torrent) {
    if torrent.info.files.is_empty() && torrent.info.file_tree.is_some() {
        let mut v2_roots = torrent.get_v2_roots();

        // Critical: Sort to match PieceManager's deterministic order
        v2_roots.sort_by(|(path_a, _, _), (path_b, _, _)| path_a.cmp(path_b));

        let mut new_files = Vec::new();
        let piece_len = torrent.info.piece_length as u64;

        for (path_str, length, _root) in v2_roots {
            let path_components: Vec<String> = path_str.split('/').map(|s| s.to_string()).collect();

            new_files.push(crate::torrent_file::InfoFile {
                length: length as i64,
                path: path_components,
                md5sum: None,
                attr: None,
            });

            // Insert BEP 52 Padding Files
            if piece_len > 0 {
                let remainder = length % piece_len;
                if remainder > 0 {
                    let padding_len = piece_len - remainder;
                    new_files.push(crate::torrent_file::InfoFile {
                        length: padding_len as i64,
                        path: vec![".pad".to_string(), padding_len.to_string()],
                        md5sum: None,
                        attr: Some("p".to_string()),
                    });
                }
            }
        }
        torrent.info.files = new_files;
    }
}

pub fn from_info_bytes(info_bytes: &[u8]) -> Result<Torrent, ParseError> {
    // 1. Deserialize the Info struct directly
    let info: crate::torrent_file::Info = serde_bencode::from_bytes(info_bytes)?;

    // 2. Wrap it in a Torrent struct with defaults
    let mut torrent = Torrent {
        info_dict_bencode: info_bytes.to_vec(),
        info,
        announce: None,
        announce_list: None,
        url_list: None,
        creation_date: None,
        comment: None,
        created_by: None,
        encoding: None,
        piece_layers: None,
    };

    // 3. UNIFIED LOGIC: Hydrate V2 files
    polyfill_v2_files(&mut torrent);

    // 4. Ensure total length is calculated
    if torrent.info.length == 0 {
        torrent.info.length = torrent.info.total_length();
    }

    Ok(torrent)
}

// [UPDATE EXISTING FUNCTION]
pub fn from_bytes(bencode_data: &[u8]) -> Result<Torrent, ParseError> {
    let generic_bencode: Value = de::from_bytes(bencode_data)?;

    let info_dict_value = if let Value::Dict(mut top_level_dict) = generic_bencode.clone() {
        top_level_dict
            .remove("info".as_bytes())
            .ok_or(ParseError::MissingInfoDict)?
    } else {
        return Err(ParseError::MissingInfoDict);
    };

    let info_dict_bencode = serde_bencode::to_bytes(&info_dict_value)?;
    let mut torrent: Torrent = de::from_bytes(bencode_data)?;

    polyfill_v2_files(&mut torrent);

    if torrent.info.length == 0 {
        torrent.info.length = torrent.info.total_length();
    }

    torrent.info_dict_bencode = info_dict_bencode;

    Ok(torrent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent_file::Info;
    use serde_bencode::value::Value;
    use std::collections::HashMap;

    #[test]
    fn test_parse_bittorrent_v2_hybrid_structure() {
        // --- 1. Construct Manual v2 Data Structures ---
        let root_hash_1 = vec![0xAA; 32];
        let root_hash_2 = vec![0xBB; 32];

        // Use HashMap for tree construction
        let mut file_a_metadata = HashMap::new();
        file_a_metadata.insert(
            "pieces root".as_bytes().to_vec(),
            Value::Bytes(root_hash_1.clone()),
        );
        file_a_metadata.insert("length".as_bytes().to_vec(), Value::Int(1000));

        let mut leaf_node_a = HashMap::new();
        leaf_node_a.insert(vec![], Value::Dict(file_a_metadata));

        let mut file_b_metadata = HashMap::new();
        file_b_metadata.insert(
            "pieces root".as_bytes().to_vec(),
            Value::Bytes(root_hash_2.clone()),
        );
        file_b_metadata.insert("length".as_bytes().to_vec(), Value::Int(2000));

        let mut leaf_node_b = HashMap::new();
        leaf_node_b.insert(vec![], Value::Dict(file_b_metadata));

        let mut folder_contents = HashMap::new();
        folder_contents.insert("file_a.txt".as_bytes().to_vec(), Value::Dict(leaf_node_a));

        let mut tree_root = HashMap::new();
        tree_root.insert("folder".as_bytes().to_vec(), Value::Dict(folder_contents));
        tree_root.insert("file_b.txt".as_bytes().to_vec(), Value::Dict(leaf_node_b));

        let mut layers = HashMap::new();
        layers.insert(root_hash_1.clone(), Value::Bytes(vec![0x11; 32]));
        layers.insert(root_hash_2.clone(), Value::Bytes(vec![0x22; 32]));

        let info = Info {
            name: "v2_test_torrent".to_string(),
            piece_length: 16384,
            pieces: vec![],
            length: 0,
            files: vec![], // Empty files list initially
            private: None,
            md5sum: None,
            meta_version: Some(2),
            file_tree: Some(Value::Dict(tree_root)),
        };

        let torrent_input = Torrent {
            info,
            announce: Some("http://tracker.test".to_string()),
            piece_layers: Some(Value::Dict(layers)),
            info_dict_bencode: vec![],
            announce_list: None,
            url_list: None,
            creation_date: None,
            comment: None,
            created_by: None,
            encoding: None,
        };

        let bencoded_data = serde_bencode::to_bytes(&torrent_input).expect("Serialization failed");

        // --- TEST: Parsing should automatically populate 'files' ---
        let parsed_torrent = super::from_bytes(&bencoded_data).expect("Parsing failed");

        // Expect 4 files (2 Real + 2 Padding)
        assert_eq!(
            parsed_torrent.info.files.len(),
            4,
            "Should have 2 real files + 2 padding files"
        );

        // Verify Paths
        let paths: Vec<Vec<String>> = parsed_torrent
            .info
            .files
            .iter()
            .map(|f| f.path.clone())
            .collect();
        assert!(paths.contains(&vec!["file_b.txt".to_string()]));
        assert!(paths.contains(&vec!["folder".to_string(), "file_a.txt".to_string()]));

        // Verify Lengths (Sum of files + padding must equal aligned size)
        let len_sum: i64 = parsed_torrent.info.files.iter().map(|f| f.length).sum();
        assert_eq!(len_sum, 32768); // 2 pieces * 16384
        assert_eq!(parsed_torrent.info.length, 32768);
    }
}
