// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

pub mod client;

use std::fmt;

use serde::Deserialize;

#[derive(Debug, Clone, Copy)]
pub enum TrackerEvent {
    Started,
    Completed,
    Stopped,
}
impl fmt::Display for TrackerEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrackerEvent::Started => write!(f, "started"),
            TrackerEvent::Completed => write!(f, "completed"),
            TrackerEvent::Stopped => write!(f, "stopped"),
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct TrackerResponse {
    pub failure_reason: Option<String>,
    pub warning_message: Option<String>,
    pub interval: i64,
    pub min_interval: Option<i64>,
    pub tracker_id: Option<String>,
    pub complete: i64,
    pub incomplete: i64,
    pub peers: Vec<Peer>,
}

#[derive(Debug, PartialEq, Clone)]
pub struct Peer {
    pub peer_id: Vec<u8>,
    pub ip: String,
    pub port: u16,
}

#[derive(Debug, Deserialize)]
struct PeerDictModel {
    ip: String,
    port: u16,
    #[serde(rename = "peer id")]
    #[serde(with = "serde_bytes")]
    peer_id: Vec<u8>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Peers {
    Compact(#[serde(with = "serde_bytes")] Vec<u8>),
    Dicts(Vec<PeerDictModel>),
}

#[derive(Debug, Deserialize)]
struct RawTrackerResponse {
    #[serde(rename = "failure reason", default)]
    failure_reason: Option<String>,
    #[serde(rename = "warning message", default)]
    warning_message: Option<String>,
    #[serde(default)]
    interval: i64,
    #[serde(rename = "min interval", default)]
    min_interval: Option<i64>,
    #[serde(rename = "tracker id", default)]
    tracker_id: Option<String>,
    #[serde(default)]
    complete: i64,
    #[serde(default)]
    incomplete: i64,
    peers: Peers,
}
