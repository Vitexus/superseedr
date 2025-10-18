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
            // For the Bencode variant, we now use the contained error `e`
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

pub fn from_bytes(bencode_data: &[u8]) -> Result<Torrent, ParseError> {
    // 1. First, deserialize the data into a generic Bencode Value structure.
    //    This allows us to inspect the raw data before converting to our final struct.
    let generic_bencode: Value = de::from_bytes(bencode_data)?;

    // 2. Extract the raw 'info' dictionary value.
    let info_dict_value = if let Value::Dict(mut top_level_dict) = generic_bencode.clone() {
        top_level_dict
            .remove("info".as_bytes())
            .ok_or(ParseError::MissingInfoDict)?
    } else {
        return Err(ParseError::MissingInfoDict);
    };

    // 3. Re-encode just the 'info' dictionary to get the bytes needed for the info_hash.
    let info_dict_bencode = serde_bencode::to_bytes(&info_dict_value)?;

    // 4. Deserialize the original data AGAIN, but this time into our strongly-typed Torrent struct.
    //    Serde is fast, so this second pass is not a major performance issue.
    let mut torrent: Torrent = de::from_bytes(bencode_data)?;

    // 5. Manually set the `info_dict_bencode` field we created.
    torrent.info_dict_bencode = info_dict_bencode;

    Ok(torrent)
}
