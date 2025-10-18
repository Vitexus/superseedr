// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

pub mod parser;

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Torrent {
    // This field is special and not directly in the bencode source.
    // We will populate it manually after deserialization.
    #[serde(skip)]
    pub info_dict_bencode: Vec<u8>,

    pub info: Info,
    pub announce: Option<String>,

    #[serde(rename = "announce-list", default)]
    pub announce_list: Option<Vec<Vec<String>>>, // Announce-list is a list of lists of strings

    #[serde(rename = "creation date", default)]
    pub creation_date: Option<i64>, // Creation date is an integer timestamp

    #[serde(default)]
    pub comment: Option<String>,

    #[serde(rename = "created by", default)]
    pub created_by: Option<String>,

    #[serde(default)]
    pub encoding: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Info {
    #[serde(rename = "piece length")]
    pub piece_length: i64,

    // Use serde_bytes to handle this as a raw byte vector
    #[serde(with = "serde_bytes")]
    pub pieces: Vec<u8>,

    #[serde(default)]
    pub private: Option<i64>,

    // `files` is optional (for single-file torrents)
    #[serde(default)]
    pub files: Vec<InfoFile>,

    pub name: String,

    // `length` is optional (for multi-file torrents)
    #[serde(default)]
    pub length: i64,

    #[serde(default)]
    pub md5sum: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InfoFile {
    pub length: i64,
    #[serde(default)]
    pub md5sum: Option<String>,
    // The path is actually a list of strings
    pub path: Vec<String>,
}
