// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use thiserror::Error;

#[derive(Error, Debug)]
pub enum TrackerError {
    #[error("Request failed networking with tracker.")]
    Request(#[from] reqwest::Error),

    #[error("Failed to parse bencoded tracker response")]
    Bencode(#[from] serde_bencode::Error),

    #[error("Tracker returned a failure reason: {0}")]
    Tracker(String),
}

#[derive(Error, Debug)]
pub enum StorageError {
    #[error("An I/O error occurred")]
    Io(#[from] std::io::Error),
}
