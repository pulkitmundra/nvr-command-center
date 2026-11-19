# Changelog

All notable changes to **NVR Command Center** are documented in this file.

## [3.0.0] - 2026-11-19

### Added
- **Interactive "What Happened" NVR Playback & AI Event Timeline (`src/playback.rs`, `src/timeline_ui.rs`, `src/nvr_api.rs`, `src/app.rs`):**
  - Press **`T`** (or click **`History`** in the Top Bar / **`Hist`** on any camera tile) to open `1920x1080` Main-Stream NVR playback with a **`98px` Obsidian Timeline Dock**, resizable **`200-380px` Event Drawer**, and a movable/hideable **`196x110` Live Picture-in-Picture (PiP)** corner window for `0ms` 1-click return to Live.
  - Added **`TTT` (`2x2 Sync`)** 4-camera synchronized playback grid on Home LAN (with per-slot camera replace/swap), plus automatic live-stream pause/resume (`NvrResourceGovernor`) when scrubbing playback over remote Cloud P2P.
  - Added **Multi-Scale Magnetic Timeline (`24h <-> 6h <-> 1h <-> 15m <-> 5m <-> 2m`)** with cursor-centered Wayland touchpad pinch + scroll-wheel zoom, `1h` preset menu & double-click reset, sub-pixel event dilation (`>= 3.5px` minimum visual width so `2s` triggers remain visible and clickable even at `24h` view), throttled live drag scrubbing, RAM-cache segment underline, `NOW` live-edge divider, editable Playhead Clock (`Ctrl+G`), calendar date picker with recorded-day badges & keyboard navigation, and semantic Obsidian color lanes (`Human` `#38BDF8`, `Vehicle` `#A78BFA`, `Intrusion/Alarm` `#EF4444`, `Motion` `#F59E0B`, and `Continuous` `#10B981`).
  - Added **1-Click Event Drawer Actions & Stacked Toast Queue**: auto-scroll to active/nearest event, inline per-type filter counts, 1-click `[Trim]` and `[MP4]` on event cards, live export progress bar + `[X]` cancel button, background PNG encoding (capped at 3 concurrent workers) with tile shutter flash, and clickable `[Open Folder]` toasts.
- **Smart Skip Dead-Air Engine, Zero-Glitch Trick-Play (`-4x .. 16x`) & Frame-Step (`src/playback.rs`):**
  - Added **`Smart Skip`** with 1-click mode toggle between **`8x Fast-Glide`** (glides across empty recording at `8x`, automatically dropping to `1.0x` `3s` before matching events) and **`Instant Event Jump`** (`event.start - 3s`), filterable by `[All]`, `[Human]`, `[Vehicle]`, or `[Motion]`.
  - Added rolling `60s` `.dav` chunk prefetching (`/cgi-bin/mediaFileFind.cgi` + `/RPC2_Login` JSON-RPC fallback + `/cgi-bin/loadfile.cgi`, Hikvision ISAPI `/ISAPI/ContentMgmt/search`, and universal RTSP `/cam/playback` fallback) backed by unlinked Linux RAM fds (`memfd_create` with `128 MiB` LRU cap and zero `/tmp` disk trace), `24`-byte `DHAV` header keyframe/RTC scanning, `-4x .. 16x` trick-play (`J` / `K` / `L`), and `0ms` `,` / `.` single-frame step via an in-RAM decoded RGBA ring buffer.
- **Dual-Port Cloud P2P Tunneling (`RTSP :554` + `HTTP :80`), Diagnostics & `.MP4` Clip Export (`src/p2p.rs`, `src/playback.rs`, `src/app.rs`):**
  - Upgraded `P2PTunnelManager` to multiplex both RTSP (`:554`) and HTTP CGI / `/RPC2` (`:80`) over a single authenticated `easy4ipcloud.com` UDP PTCP session so full AI timeline search, Smart Skip, and `.MP4` clip export work remotely with zero port forwarding.
  - Added **`I` (Mark In) -> `O` (Mark Out) -> `Export MP4` (`Ctrl+E`)** lossless `-c:v copy -movflags +faststart` MP4 clip export into `~/Videos/NVR-Recordings` (`0700`/`0600`), historical-timestamped PNG snapshots (`C` / `Ctrl+S`) preserving `1.0x - 8.0x` Area Box Zoom, Settings **`Test Connection`** + inline `validate_for_save()` + cancellable `Auto-Scan (1-N)`, and `"auto"` default grid layout.

### Security
- **Zero-Leak Loopback `AuthGateway` & Canary Circuit Breaker (`src/auth_gateway.rs`, `src/stream.rs`):**
  - Eliminated NVR credentials from `ffmpeg`/`ffprobe` CLI arguments (`/proc/<pid>/cmdline` and `ps aux`) via an in-process `127.0.0.1:0` loopback RTSP `AuthGateway` using 1-time 128-bit `OsRng` tokens, `SO_PEERCRED` UID verification, and Basic-auth downgrade blocking.
  - Added a Channel 1 **Canary Auth Probe** and shared `AuthCircuitBreaker` so mistyped passwords halt immediately before parallel workers can trigger 12-attempt NVR account lockouts, plus background `stderr` drain threads preventing 64 KiB pipe deadlocks.
- **Process, Tunnel & Filesystem Hardening (`src/config.rs`, `src/p2p.rs`, `src/app.rs`, `launch_viewer.sh`, `.github/workflows/snap.yml`):**
  - Added `prctl(PR_SET_DUMPABLE, 0)`, `Zeroize` on credential drop & `.env` map cleanup, UID ownership checks on `.env` load, full ancestor + leaf symlink rejection (`reject_any_symlink_component`), `O_CREAT | O_EXCL | O_NOFOLLOW` (`0600`) snapshot/export creation with explicit `fsync`, isolated `python3 -I` + `https://` allowlist + trusted binary resolution for external openers, public-IP SSRF validation on P2P stun/relay peers, DTD-rejecting `quick-xml` parsing, `512 KiB` PTCP reorder window cap, `--locked` Cargo builds, and Amber `Cloud P2P` top-bar indicator.

## [2.2.0] - 2026-11-15

### Added
- **Minimal Obsidian Studio UI & Borderless Zen Wall (`src/app.rs`, `src/config.rs`):**
  - Redesigned the workspace around a `#09090B` Obsidian canvas, `#111114` matte graphite command bar (`38px` 3-zone layout), and progressive-disclosure camera tiles that show only a whisper-quiet status dot + camera name when idle.
  - Added **Borderless Zen Wall (`B`)** and **OS Monitor Fullscreen (`F11`)** with `2.8s` pointer-idle auto-hide and top-edge (`12px`) hover peek.
  - Added undistorted **`16:9` Fit vs `Fill`** aspect framing (`A`, persisted via `aspect_fit_16_9` in `config.json`) and an interactive **Keyboard Shortcuts Cheat-Sheet (`?`)** modal.
- **Smart Area Box Zoom (`e-PTZ` `1.0x - 8.0x`), Native Wayland Pinch & Interactive Radar (`src/app.rs`, `src/wayland_pinch.rs`):**
  - Added click-drag optical viewfinder **Area Box Zoom (`1.0x - 8.0x`)** with smooth glide animation, `Shift+Drag` / dock `Box` nested box-zoom, `Backspace` zoom-history step-back, smart progressive `Esc` unwind, and zoomed-region cropped PNG snapshots (`C` / right-click menu).
  - Added a native Linux Wayland `zwp_pointer_gestures_v1` guest listener (`src/wayland_pinch.rs`) for two-finger touchpad pinch-to-zoom plus zero-lag symmetric trackpad scroll zoom (`Event::MouseWheel`), tight pan clamping, and an interactive click/drag bottom-left wireframe radar minimap.
- **2D Grid Selection & Multi-Tap Chord Engine (`450ms` Window):**
  - Added active/selected tile borders (`#38BDF8`), 2D grid arrow-key navigation, and single-tap vs triple-tap chords: `S` / `SSS` (`Shift+S`) for SD, `H` / `HHH` (`Shift+H`) for HD, and `Z` / `ZZZ` (`Shift+Z`) for zoom reset.
- **Portfolio & Architecture Deep-Dive Links (`snap/snapcraft.yaml`, `.github/workflows/snap.yml`, `src/app.rs`, `README.md`):**
  - Added direct links to the interactive project case study, engineering deep-dive write-up, and developer portfolio in `Settings -> 3. About & Shortcuts`, Snap Store metadata, and documentation.

## [2.1.1] - 2026-11-11

### Fixed
- **Safe `.env` Discovery & Inline Comment Parsing (`src/config.rs`):** Prioritized `SNAP_USER_COMMON/.env` and `~/.rtsp_viewer/.env` over unrelated CWD `.env` files, enforced atomic `0600`/`0700` saves, stripped inline `#` comments outside quotes, and bracketed IPv6 hosts.
- **Cloud P2P Tunnel Concurrency & Isolation (`src/p2p.rs`):** Eliminated thread-generation status clobbering with monotonic generation guards + `< 5ms` `watch` shutdown, added per-camera write timeouts and non-blocking `try_send` so a single stalled client cannot block other cameras, rejected truncated PTCP datagrams, and halted retries on `401` auth failures to prevent NVR account lockouts.
- **Active Kernel Pipe Burst Drain & Process Lifecycle (`src/stream.rs`, `src/app.rs`):** Expanded Linux pipe buffers via `fcntl(F_SETPIPE_SZ, 1_048_576)` so `ioctl(FIONREAD)` holds multiple SD frames, added `PR_SET_PDEATHSIG(SIGKILL)` + FFmpeg socket timeouts, tracked Patrol Mode auto-HD promotions so prior cameras return to SD, guarded narrow-window `f32::clamp`, and added `gpu-2404` (`mesa-2404`) support to the `core24` Snap.

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
