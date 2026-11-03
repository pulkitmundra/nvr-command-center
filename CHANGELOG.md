# Changelog

All notable changes to **NVR Command Center** are documented in this file.

## [2.1.0] - 2026-11-03

### Added
- **Zero-Config Cloud P2P Remote Access (`src/p2p.rs` - Works on Any Internet Connection)**:
  - Built-in native Rust Dahua / CP Plus / Amcrest Cloud P2P (`easy4ipcloud.com`) authenticated PTCP tunnel (`AES-256-OFB` device salt decryption + `PBKDF2-HMAC-SHA256` + `HMAC-SHA256` channel auth + sequence-window deduplication).
  - **Automatic LAN -> Cloud P2P Fallback**: On startup, probes the local LAN NVR IP (`500ms` timeout); when on home Wi-Fi, connects directly via LAN (`< 80ms` latency), and when on any other internet connection (mobile hotspot, coffee shop, travel Wi-Fi), automatically tunnels all 12 camera streams through Cloud P2P using `RTSP_P2P_SERIAL` with zero router port forwarding and zero extra home hardware.
  - **1-Click `LAN` / `Cloud P2P` / `Tailscale` Top Bar Switcher**: Switch between local LAN direct and remote Cloud P2P tunneling at any time with a single click.

## [2.0.0] - 2026-11-03

### Added
- **Complete Native Rust + OpenGL GPU Rewrite (`eframe` / `egui` + `glow`)**:
  - Replaced legacy Python/Tkinter/OpenCV interpreter stack with a standalone native Rust binary (`target/release/rtsp-camera-viewer`).
  - Persistent GPU texture pool (`egui::TextureHandle`) that only uploads RGBA frames when a camera's atomic `frame_seq` advances.
  - **Zero-CPU UV Texture Digital Zoom & Pan (`1.0x - 5.0x`)** computed directly on the GPU quad via UV coordinates at `0.0%` CPU resize cost.
- **Instantaneous Parallel Startup (`< 0.9s` First-Frame Lock)**:
  - All 12+ camera threads spawn concurrently with zero-probe FFmpeg demuxer flags (`-probesize 32768 -analyzeduration 0 -fpsprobesize 0 -fflags nobuffer+discardcorrupt -flags low_delay -max_delay 0`), prioritizing hybrid IP camera channels (`9..12`) so handshakes complete in parallel.
- **Zero-Lag Synchronized Live-Edge Pipeline (`< 80ms` Latency)**:
  - Linux kernel pipe `ioctl(FIONREAD)` burst drain consumes incoming frames at 100% native wire rate and discards intermediate buffered packets before `Mutex` swap, preventing the `2s - 5s` socket backlog and drift across cameras.
  - Pre-allocated double-buffered `Vec<u8>` slots via `std::mem::swap` (`0` heap allocations per frame for 24/7 operation).
  - **4.5-Second Frozen-Socket Watchdog** that automatically terminates and reconnects stalled RTSP TCP sockets.
- **Dynamic Camera Count (`1 - 32` Channels, Default `12`) & 1-Click Auto-Scanner**:
  - Removed hardcoded `11`-camera limits; defaults to `12` cameras (`3x4` grid) and scales dynamically from `1` to `32` channels (`3x4`, `4x3`, `2x6`, or `Auto` layouts).
  - Built-in parallel **1-Click Auto-Scan NVR Channels (`--scan`)** in the Settings modal.
- **Multi-Brand NVR Configurator & `.env` (`0600`) Security**:
  - Moved all NVR credentials into `.env` (`chmod 0600`, gitignored) and added a 3-tab in-app Settings dialog supporting **Dahua / CP Plus / Amcrest**, **Hikvision**, and **Custom ONVIF / RTSP URL templates**, plus a 1-click **LAN <-> Cloud P2P <-> Tailscale** remote switcher.
- **Seamless Per-Camera `[SD]/[HD]` & `[Single]/[Grid]` Controls, Patrol Mode & Composite Wall Snapshots**:
  - 1-click `[SD]`/`[HD]` and `[Single]`/`[Grid]` buttons on every camera tile.
  - Added **Patrol Mode (`M`)**, **Tile Position Swapping**, **Custom Camera Names**, and **Composite Multi-Camera Wall PNG Snapshots (`S`)**.

## [1.0.0] - 2025-11-24

### Added
- Initial Python / Tkinter / OpenCV multi-camera RTSP grid viewer with layout switching, zoom/pan, screenshots, and GNOME keyboard shortcut scripts.
