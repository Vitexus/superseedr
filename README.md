# superseedr - A BitTorrent Client in Rust

`superseedr` is a modern, terminal-based BitTorrent client written in Rust, focused on performance and a clean user experience.

![Client Screenshot](https://i.imgur.com/G5gQO0B.jpeg)

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
    -   ✅ **Cross-Platform:** Automatically stores configuration in the appropriate user data directory (e.g., `~/.config/superseedr` on Linux).
    -   ✅ **Upload/Download Limits:** Set global upload and download speed limits.

---

## Project Status
✅ Completed for V1.0
Multi-File Torrent Support: The storage layer now fully supports torrents containing multiple files, including creating directory structures and handling reads/writes that span across file boundaries.
Sortable Table Feature: The table is now sortable, allowing users to organize and browse content more efficiently.

## Roadmap to V1.0
This is the punch list of features and fixes required for a stable and feature-complete "Version 1.0" release.

### Tier 1: Critical V1 Features
✅ Graceful Shutdown: The session state is correctly saved on exit. A full shutdown sequence is needed to signal all torrent managers to stop cleanly, ensuring trackers are notified.

### Tier 2: Core Functionality & UX
✅ Restore Paused/Running State on Startup: The saved state is correctly restored for magnet links, but needs to be implemented for torrents added from .torrent files.

[✅] Use All Config Values: Remove remaining hardcoded values (e.g., client port, bootstrap nodes) from the codebase and use the values from the Settings object exclusively.

### Tier 3: Polish & Distribution
[✅] Command-Line Arguments: Integrate clap to allow adding torrents directly from the command line (superseedr add "magnet:...").

✅ Help Screen: Add a ? keybinding to show a popup with all available keyboard commands.

[ ] Testing & Cleanup: Run cargo clippy and cargo fmt, add unit tests for critical logic, and set up a CI pipeline.

[ ] Packaging: Publish to crates.io and provide pre-compiled binaries for major platforms.

[✅] Handle magnet links from the browser on all systems.

## Future (V2.0 and Beyond)
Selective Downloading: Now that multi-file support is implemented, the next major feature will be a TUI panel to allow users to select which files within a torrent to download.

### v1 needed features

- [✅] refactor tui to functions
- [✅] refactor message strucs
- [✅] graph refinement longer average

### V2 Features
- Persist to disk network history
- RSS
- Dynamic upload slots
- Persist to disk network history
- RSS
- Dynamic upload slots
- headless + tui

GNU GENERAL PUBLIC LICENSE
Version 3, 29 June 2007

Copyright (C) 2025 The superseedr Contributors

