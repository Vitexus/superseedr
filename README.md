# superseedr - A BitTorrent Client in Rust

`superseedr` is a modern, terminal-based BitTorrent client and TUI written in Rust.

![Feature Demo](https://github.com/Jagalite/superseedr-assets/blob/main/superseedr_landing.webp)

**Status: Distribution TBD**
The final build and installation methods are under review. To run the application now, you must build from source.

### Preview / Running Locally / Tested on M1 Macbook
Run the application directly using Cargo:
Loading torrents via Paste (crtl + v) or by setting a torrent watch directory.
```bash
cargo run
```
Configuration files are located in the user's Application Support folder:
`~/Library/Application Support/com.torrent.superseedr/`

## Current Status & Features
The client is currently in a late-alpha stage. Most of the core BitTorrent protocol features are implemented and functional.

-   **Multi-Torrent Support:** Download and seed multiple torrents simultaneously.
-   **Peer Discovery:**
    -   ✅ **Trackers:** Fully supported.
    -   ✅ **DHT (Distributed Hash Table):** Find peers on trackerless torrents.
    -   ✅ **PEX (Peer Exchange):** Discover peers from other connected peers.
    -   ✅ **Magnet Links:** Add torrents using magnet links, with metadata download.
-   **Core Protocol:**
    -   ✅ **Piece Selection:** Implements a Rarest-First strategy for swarm health, transitioning to Endgame Mode for the final pieces.
    -   ✅ **Choking Algorithm:** A tit-for-tat based choking algorithm with optimistic unchoking to efficiently manage upload slots.
    -   ✅ **Partial Downloads:** Resumes downloads from partially completed files.
-   **User Interface (TUI):**
    -   ✅ **Real-time Dashboard:** A terminal UI built with `ratatui` showing overall status, individual torrent progress, peer lists, and network graphs.
    -   ✅ **Torrent Management:** Pause, resume, and delete torrents (with or without deleting files on disk).
    -   ✅ **Interactive Settings:** An in-app screen to view and edit all client settings.
    -   ✅ **Help Screen:** A popup with all available keyboard commands.
    -   ✅ **Footer Commands:** A footer bar showing common commands.
    -   ✅ **Announce Timers:** The TUI now shows a countdown for the next tracker announce.
-   **Configuration:**
    -   ✅ **Persistent State:** Saves torrent list, progress, and lifetime stats to a configuration file.
    -   ✅ **Cross-Platform:** Automatically stores configuration in the appropriate user data directory. 
    -   ✅ **Upload/Download Limits:** Set global upload and download speed limits.

---

## Roadmap to V1.0
This is the punch list of features and fixes required for a stable and feature-complete "Version 1.0" release.
- Building and distribution WIP.
- Testing in various platforms and terminals.
- Handling magnet links via registry, app, ...etc
- CI/CD
- Unit testing

## Future (V2.0 and Beyond)
- Persist to disk network history.
- RSS.
- Headless + tui.
- Advance tuning algorithm for disk penalties.

