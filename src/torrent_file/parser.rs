// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::torrent_file::{path_casefold_key, PathValidationError, Torrent};
use serde_bencode::de;
use serde_bencode::value::Value;

use std::collections::HashSet;
use std::fmt;

#[derive(Debug)]
pub enum ParseError {
    Bencode(serde_bencode::Error),
    MissingInfoDict,
    InvalidPath(PathValidationError),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ParseError::Bencode(e) => write!(f, "Bencode parsing error: {}", e),
            ParseError::MissingInfoDict => write!(f, "Missing 'info' dictionary in torrent file"),
            ParseError::InvalidPath(e) => write!(f, "Invalid torrent metadata: {e}"),
        }
    }
}

impl std::error::Error for ParseError {}

impl From<serde_bencode::Error> for ParseError {
    fn from(e: serde_bencode::Error) -> Self {
        ParseError::Bencode(e)
    }
}

impl From<PathValidationError> for ParseError {
    fn from(error: PathValidationError) -> Self {
        ParseError::InvalidPath(error)
    }
}

pub fn polyfill_v2_files(torrent: &mut Torrent) {
    if torrent.info.files.is_empty() && torrent.info.file_tree.is_some() {
        let mut v2_files = torrent.get_v2_files();

        // Critical: Sort to match PieceManager's deterministic order
        v2_files.sort_by(|(path_a, _, _), (path_b, _, _)| path_a.cmp(path_b));

        let mut new_files = Vec::new();
        let mut used_paths: HashSet<Vec<String>> = v2_files
            .iter()
            .map(|(path, _, _)| {
                let components: Vec<String> = path.split('/').map(str::to_owned).collect();
                path_casefold_key(&components)
            })
            .collect();
        let piece_len = torrent.info.piece_length as u64;

        for (path_str, length, _root) in v2_files {
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
                    // Padding entries are real file-list entries, so their paths must also be
                    // unique. Including the eventual file index keeps equal-length padding
                    // entries stable without collapsing them in the preview tree.
                    let padding_index = new_files.len();
                    let mut disambiguator = 0usize;
                    let padding_path = loop {
                        let directory = if disambiguator == 0 {
                            ".pad".to_string()
                        } else {
                            format!(".pad.{disambiguator}")
                        };
                        let candidate = vec![directory, format!("{padding_len}.{padding_index}")];
                        let candidate_key = path_casefold_key(&candidate);
                        let conflicts = used_paths.iter().any(|existing| {
                            existing.starts_with(&candidate_key)
                                || candidate_key.starts_with(existing)
                        });
                        if !conflicts {
                            used_paths.insert(candidate_key);
                            break candidate;
                        }
                        disambiguator += 1;
                    };
                    new_files.push(crate::torrent_file::InfoFile {
                        length: padding_len as i64,
                        path: padding_path,
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

    // Validate signed geometry before v2 hydration casts any lengths to u64.
    torrent.validate_paths()?;

    // 3. UNIFIED LOGIC: Hydrate V2 files
    polyfill_v2_files(&mut torrent);

    // Revalidate the hydrated file list, including synthetic padding, before
    // computing its aggregate length.
    torrent.validate_paths()?;

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

    // Validate signed geometry before v2 hydration casts any lengths to u64.
    torrent.validate_paths()?;

    polyfill_v2_files(&mut torrent);

    // Revalidate the hydrated file list, including synthetic padding, before
    // computing its aggregate length.
    torrent.validate_paths()?;

    if torrent.info.length == 0 {
        torrent.info.length = torrent.info.total_length();
    }

    torrent.info_dict_bencode = info_dict_bencode;

    Ok(torrent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent_file::{Info, InfoFile, PathValidationError};
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

    #[test]
    fn v2_padding_paths_are_unique_for_equal_remainders() {
        let mut tree_root = HashMap::new();
        for (name, root_byte) in [("alpha.bin", 0xAA), ("beta.bin", 0xBB)] {
            let mut metadata = HashMap::new();
            metadata.insert(
                "pieces root".as_bytes().to_vec(),
                Value::Bytes(vec![root_byte; 32]),
            );
            metadata.insert("length".as_bytes().to_vec(), Value::Int(1));

            let mut leaf = HashMap::new();
            leaf.insert(vec![], Value::Dict(metadata));
            tree_root.insert(name.as_bytes().to_vec(), Value::Dict(leaf));
        }

        let mut torrent = Torrent {
            info: Info {
                name: "equal-remainder-sample".to_string(),
                piece_length: 16_384,
                pieces: vec![],
                length: 0,
                files: vec![],
                private: None,
                md5sum: None,
                meta_version: Some(2),
                file_tree: Some(Value::Dict(tree_root)),
            },
            ..Torrent::default()
        };

        polyfill_v2_files(&mut torrent);

        let paths: Vec<_> = torrent
            .info
            .files
            .iter()
            .map(|file| file.path.clone())
            .collect();
        assert_eq!(
            paths,
            vec![
                vec!["alpha.bin".to_string()],
                vec![".pad".to_string(), "16383.1".to_string()],
                vec!["beta.bin".to_string()],
                vec![".pad".to_string(), "16383.3".to_string()],
            ]
        );
        assert_eq!(paths.iter().collect::<HashSet<_>>().len(), 4);
    }

    #[test]
    fn v2_padding_path_avoids_file_directory_collision() {
        let mut metadata = HashMap::new();
        metadata.insert(
            "pieces root".as_bytes().to_vec(),
            Value::Bytes(vec![0x33; 32]),
        );
        metadata.insert("length".as_bytes().to_vec(), Value::Int(1));
        let mut leaf = HashMap::new();
        leaf.insert(vec![], Value::Dict(metadata));
        let mut tree = HashMap::new();
        tree.insert(b".pad".to_vec(), Value::Dict(leaf));

        let mut torrent = Torrent {
            info: Info {
                name: "padding-prefix-sample".to_string(),
                piece_length: 16_384,
                meta_version: Some(2),
                file_tree: Some(Value::Dict(tree)),
                ..Info::default()
            },
            ..Torrent::default()
        };

        polyfill_v2_files(&mut torrent);

        assert_eq!(torrent.info.files[0].path, vec![".pad".to_string()]);
        assert_eq!(
            torrent.info.files[1].path,
            vec![".pad.1".to_string(), "16383.1".to_string()]
        );
        assert_eq!(torrent.validate_paths(), Ok(()));
    }

    #[test]
    fn v2_padding_namespace_avoids_case_aliases() {
        let mut metadata = HashMap::new();
        metadata.insert(
            "pieces root".as_bytes().to_vec(),
            Value::Bytes(vec![0x44; 32]),
        );
        metadata.insert("length".as_bytes().to_vec(), Value::Int(1));
        let mut leaf = HashMap::new();
        leaf.insert(vec![], Value::Dict(metadata));
        let mut pad_directory = HashMap::new();
        pad_directory.insert(b"16383.1".to_vec(), Value::Dict(leaf));
        let mut tree = HashMap::new();
        tree.insert(b".PAD".to_vec(), Value::Dict(pad_directory));

        let mut torrent = Torrent {
            info: Info {
                name: "padding-alias-sample".to_string(),
                piece_length: 16_384,
                meta_version: Some(2),
                file_tree: Some(Value::Dict(tree)),
                ..Info::default()
            },
            ..Torrent::default()
        };

        torrent.validate_paths().expect("validate raw v2 tree");
        polyfill_v2_files(&mut torrent);

        assert_eq!(
            torrent.info.files[0].path,
            vec![".PAD".to_string(), "16383.1".to_string()]
        );
        assert_eq!(
            torrent.info.files[1].path,
            vec![".pad.1".to_string(), "16383.1".to_string()]
        );
        assert_eq!(torrent.validate_paths(), Ok(()));
    }

    #[test]
    fn pure_v2_zero_length_leaf_without_root_keeps_its_file_index() {
        let mut empty_metadata = HashMap::new();
        empty_metadata.insert("length".as_bytes().to_vec(), Value::Int(0));
        let mut empty_leaf = HashMap::new();
        empty_leaf.insert(vec![], Value::Dict(empty_metadata));

        let mut real_metadata = HashMap::new();
        real_metadata.insert("length".as_bytes().to_vec(), Value::Int(1));
        real_metadata.insert(
            "pieces root".as_bytes().to_vec(),
            Value::Bytes(vec![0x55; 32]),
        );
        let mut real_leaf = HashMap::new();
        real_leaf.insert(vec![], Value::Dict(real_metadata));

        let mut tree = HashMap::new();
        tree.insert(b"empty.bin".to_vec(), Value::Dict(empty_leaf));
        tree.insert(b"real.bin".to_vec(), Value::Dict(real_leaf));
        let info = Info {
            name: "zero-length-sample".to_string(),
            piece_length: 16_384,
            meta_version: Some(2),
            file_tree: Some(Value::Dict(tree)),
            ..Info::default()
        };

        let parsed = from_info_bytes(&serde_bencode::to_bytes(&info).unwrap()).unwrap();

        assert_eq!(parsed.info.files[0].path, vec!["empty.bin".to_string()]);
        assert_eq!(parsed.info.files[0].length, 0);
        assert_eq!(parsed.info.files[1].path, vec!["real.bin".to_string()]);
        assert_eq!(parsed.info.files[2].attr.as_deref(), Some("p"));
        let mapping = parsed.calculate_v2_mapping();
        assert_eq!(mapping.piece_to_roots[&0][0].file_index, 1);
    }

    #[test]
    fn parsers_reject_unsafe_metadata_paths() {
        let unsafe_info = Info {
            name: "../outside".to_string(),
            piece_length: 16_384,
            pieces: vec![0; 20],
            length: 1,
            ..Info::default()
        };

        let info_bytes = serde_bencode::to_bytes(&unsafe_info).unwrap();
        assert!(matches!(
            from_info_bytes(&info_bytes),
            Err(ParseError::InvalidPath(
                PathValidationError::PathSeparator | PathValidationError::AbsoluteOrPrefixed
            ))
        ));

        let duplicate_info = Info {
            name: "safe-item".to_string(),
            piece_length: 16,
            pieces: vec![0; 20],
            files: vec![
                InfoFile {
                    length: 1,
                    path: vec!["same.bin".to_string()],
                    ..InfoFile::default()
                },
                InfoFile {
                    length: 1,
                    path: vec!["same.bin".to_string()],
                    ..InfoFile::default()
                },
            ],
            ..Info::default()
        };
        let torrent_bytes = serde_bencode::to_bytes(&Torrent {
            info: duplicate_info,
            ..Torrent::default()
        })
        .unwrap();
        assert!(matches!(
            from_bytes(&torrent_bytes),
            Err(ParseError::InvalidPath(
                PathValidationError::DuplicateFilePath(_)
            ))
        ));
    }

    #[test]
    fn parsers_reject_nonpositive_piece_and_negative_file_lengths() {
        let invalid_piece_length = Info {
            name: "invalid-geometry".to_string(),
            piece_length: 0,
            length: 1,
            ..Info::default()
        };
        assert!(matches!(
            from_info_bytes(&serde_bencode::to_bytes(&invalid_piece_length).unwrap()),
            Err(ParseError::InvalidPath(
                PathValidationError::NonPositivePieceLength(0)
            ))
        ));

        let negative_file = Info {
            name: "invalid-geometry".to_string(),
            piece_length: 16,
            files: vec![InfoFile {
                length: -1,
                path: vec!["payload.bin".to_string()],
                ..InfoFile::default()
            }],
            ..Info::default()
        };
        assert!(matches!(
            from_info_bytes(&serde_bencode::to_bytes(&negative_file).unwrap()),
            Err(ParseError::InvalidPath(
                PathValidationError::NegativeFileLength(_)
            ))
        ));

        let mut negative_v2_metadata = HashMap::new();
        negative_v2_metadata.insert("length".as_bytes().to_vec(), Value::Int(-1));
        negative_v2_metadata.insert(
            "pieces root".as_bytes().to_vec(),
            Value::Bytes(vec![0x66; 32]),
        );
        let mut negative_v2_leaf = HashMap::new();
        negative_v2_leaf.insert(vec![], Value::Dict(negative_v2_metadata));
        let mut negative_v2_tree = HashMap::new();
        negative_v2_tree.insert(b"payload.bin".to_vec(), Value::Dict(negative_v2_leaf));
        let negative_v2 = Info {
            name: "invalid-v2-geometry".to_string(),
            piece_length: 16_384,
            meta_version: Some(2),
            file_tree: Some(Value::Dict(negative_v2_tree)),
            ..Info::default()
        };
        assert!(matches!(
            from_info_bytes(&serde_bencode::to_bytes(&negative_v2).unwrap()),
            Err(ParseError::InvalidPath(
                PathValidationError::NegativeFileLength(_)
            ))
        ));
    }

    #[test]
    fn parser_rejects_malformed_v2_leaf_geometry_and_piece_lengths() {
        let malformed_nodes = {
            let mut missing_length_leaf = HashMap::new();
            missing_length_leaf.insert(vec![], Value::Dict(HashMap::new()));

            let mut non_integer_metadata = HashMap::new();
            non_integer_metadata
                .insert("length".as_bytes().to_vec(), Value::Bytes(b"one".to_vec()));
            let mut non_integer_leaf = HashMap::new();
            non_integer_leaf.insert(vec![], Value::Dict(non_integer_metadata));

            let mut missing_root_metadata = HashMap::new();
            missing_root_metadata.insert("length".as_bytes().to_vec(), Value::Int(1));
            let mut missing_root_leaf = HashMap::new();
            missing_root_leaf.insert(vec![], Value::Dict(missing_root_metadata));

            let mut short_root_metadata = HashMap::new();
            short_root_metadata.insert("length".as_bytes().to_vec(), Value::Int(1));
            short_root_metadata.insert(
                "pieces root".as_bytes().to_vec(),
                Value::Bytes(vec![0x77; 31]),
            );
            let mut short_root_leaf = HashMap::new();
            short_root_leaf.insert(vec![], Value::Dict(short_root_metadata));

            vec![
                Value::Int(1),
                Value::Dict(HashMap::new()),
                Value::Dict(missing_length_leaf),
                Value::Dict(non_integer_leaf),
                Value::Dict(missing_root_leaf),
                Value::Dict(short_root_leaf),
            ]
        };

        for malformed_node in malformed_nodes {
            let mut tree = HashMap::new();
            tree.insert(b"payload.bin".to_vec(), malformed_node);
            let info = Info {
                name: "malformed-v2-sample".to_string(),
                piece_length: 16_384,
                meta_version: Some(2),
                file_tree: Some(Value::Dict(tree)),
                ..Info::default()
            };
            assert!(matches!(
                from_info_bytes(&serde_bencode::to_bytes(&info).unwrap()),
                Err(ParseError::InvalidPath(
                    PathValidationError::MalformedV2Tree { .. }
                ))
            ));
        }

        let mut zero_metadata = HashMap::new();
        zero_metadata.insert("length".as_bytes().to_vec(), Value::Int(0));
        let mut zero_leaf = HashMap::new();
        zero_leaf.insert(vec![], Value::Dict(zero_metadata));
        let mut tree = HashMap::new();
        tree.insert(b"empty.bin".to_vec(), Value::Dict(zero_leaf));

        for invalid_piece_length in [1_000, 32 * 1024 * 1024, i64::from(u32::MAX) + 1] {
            let info = Info {
                name: "invalid-v2-piece-size".to_string(),
                piece_length: invalid_piece_length,
                meta_version: Some(2),
                file_tree: Some(Value::Dict(tree.clone())),
                ..Info::default()
            };
            assert!(matches!(
                from_info_bytes(&serde_bencode::to_bytes(&info).unwrap()),
                Err(ParseError::InvalidPath(
                    PathValidationError::InvalidV2PieceLength(_)
                ))
            ));
        }
    }

    #[test]
    fn parser_rejects_aggregate_file_length_overflow() {
        let info = Info {
            name: "overflow-sample".to_string(),
            piece_length: 16_384,
            files: vec![
                InfoFile {
                    length: i64::MAX,
                    path: vec!["alpha.bin".to_string()],
                    ..InfoFile::default()
                },
                InfoFile {
                    length: 1,
                    path: vec!["beta.bin".to_string()],
                    ..InfoFile::default()
                },
            ],
            ..Info::default()
        };

        assert!(matches!(
            from_info_bytes(&serde_bencode::to_bytes(&info).unwrap()),
            Err(ParseError::InvalidPath(
                PathValidationError::TotalLengthOverflow
            ))
        ));
    }
}
