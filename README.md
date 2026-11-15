# NVR Command Center v2.2 (Native Rust + GPU + Cloud P2P)

[![Version](https://img.shields.io/badge/version-2.2.0-blue.svg)](https://github.com/pulkitmundra/nvr-command-center/releases)
[![Snap Store](https://img.shields.io/badge/Snap_Store-nvr--command--center-82BEA0?logo=snapcraft&logoColor=white)](https://snapcraft.io/nvr-command-center)
[![Case Study](https://img.shields.io/badge/Case_Study-Interactive_Demo-10B981?logo=vercel&logoColor=white)](https://pulkit-mundra-portfolio.vercel.app/projects/nvr-command-center)
[![Architecture Deep Dive](https://img.shields.io/badge/Deep_Dive-64KB_Linux_Pipe_Trap-0EA5E9?logo=linux&logoColor=white)](https://pulkit-mundra-portfolio.vercel.app/engineering/python-to-rust-12-camera-nvr-linux-pipe-trap)
[![Portfolio](https://img.shields.io/badge/Portfolio-pulkit--mundra-2563EB?logo=vercel&logoColor=white)](https://pulkit-mundra-portfolio.vercel.app)
[![Rust](https://img.shields.io/badge/Rust-Native_GPU-orange?logo=rust)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/Platform-Linux%20%7C%20Ubuntu-E95420?logo=ubuntu&logoColor=white)](https://github.com/pulkitmundra/nvr-command-center)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)
[![Buy Me A Coffee](https://img.shields.io/badge/Buy%20Me%20A%20Coffee-ffdd00?logo=buy-me-a-coffee&logoColor=black)](https://buymeacoffee.com/pulkitmundra)
[![GitHub Sponsors](https://img.shields.io/badge/Sponsor-%E2%9D%A4-ea4aaa?logo=github-sponsors)](https://github.com/sponsors/pulkitmundra)

An ultra-low-latency, GPU-accelerated multi-camera NVR/DVR surveillance command center built in **Native Rust (`eframe` / `egui` + OpenGL `glow`)** with parallel zero-probe FFmpeg workers, kernel pipe burst draining, a distraction-free **Obsidian Studio** security wall UI, and built-in **Cloud P2P remote tunneling**.

Designed to stream **12+ concurrent cameras (`1 - 32` channels)** 24/7 with **`< 0.9s` parallel startup**, **`< 80ms` glass-to-glass latency**, **zero stream drift**, and **`~65 - 110 MB` total RAM** - on both local LAN and any remote internet connection.

> **Interactive Simulator & Systems Writeup:** Try the live browser-based 12-camera pipe-drain simulator in the [**Case Study**](https://pulkit-mundra-portfolio.vercel.app/projects/nvr-command-center) or read the engineering postmortem: [**650 MB to 70 MB RAM: Porting a 12-Camera Python CCTV Wall & Solving the 64 KB Linux Pipe Trap ->**](https://pulkit-mundra-portfolio.vercel.app/engineering/python-to-rust-12-camera-nvr-linux-pipe-trap)

---

## Why Native Rust v2.2?

| Metric | Legacy Python (`v1.x` Tkinter + OpenCV) | **Native Rust (`v2.2` `eframe/egui` + GPU + Obsidian Studio)** |
| :--- | :--- | :--- |
| **Cold Binary Launch** | `~1.2s` (Python + NumPy + OpenCV import) | **`< 40ms` (Standalone native ELF binary)** |
| **12-Camera Parallel Startup** | `12s - 20s` (Sequential 5 MB FFmpeg probe lock) | **`< 0.9s` (`probesize=32768`, `analyzeduration=0`, `fpsprobesize=0`)** |
| **Live Stream Latency & Sync** | `2.0s - 5.0s` lag & inter-camera drift | **`< 80ms` synchronized live edge (`ioctl(FIONREAD)` burst drain)** |
| **Area Box Zoom & GPU e-PTZ (`1x-8x`)** | CPU NumPy crop + `cv2.resize` on every frame | **`0.0%` CPU cost (Click-drag marquee box zoom + Wayland pinch + interactive radar)** |
| **Per-Camera `SD <-> HD` & `Single` View** | `1.5s - 3.0s` black screen reconnect | **`0ms` black flash (Floating glass dock + `s`/`sss` & `h`/`hhh` chords)** |
| **24/7 Memory Footprint** | `550 MB - 900 MB` | **`~65 MB - 110 MB` (Pre-allocated buffers via `std::mem::swap`)** |

---

## Architecture Highlights

1. **Zero-Lag Kernel Pipe Burst Drain (`ioctl(FIONREAD)`):**
   - Traditional RTSP viewers sleep between `read()` calls to throttle frame rate, causing frames from `25 - 30 FPS` NVR streams to pile up inside the OS TCP receive buffer and FFmpeg ring buffer (`2s - 5s` lag).
   - Each Rust worker thread drains its stream at **100% native wire rate** and queries `libc::ioctl(fd, libc::FIONREAD)` on the pipe after every frame. If a burst of packets arrived simultaneously, intermediate frames are discarded in microseconds before locking the shared `FrameSlot`, guaranteeing **`0` buffered frames** across all 12 cameras.
2. **Instant Parallel Startup (`-fpsprobesize 0` + Hybrid Priority):**
   - Embedded XVRs/NVRs often advertise `r_frame_rate=100/1` in RTSP SDP headers, causing FFmpeg to wait for 20 frames (`3.5s`) before emitting video. Passing `-probesize 32768 -analyzeduration 0 -fpsprobesize 0 -fflags nobuffer+discardcorrupt -flags low_delay` forces immediate first-keyframe decoding in `< 0.9s`.
3. **4.5-Second Frozen-Socket Watchdog:**
   - A dedicated Rust supervisor monitors atomic `last_frame_epoch_ms` timestamps across all channels and automatically terminates/reconnects any RTSP socket that stalls mid-stream for `> 4.5s`.
4. **Smart Area Box Zoom (`e-PTZ`), Native Wayland Pinch & Interactive Radar Minimap:**
   - Each camera maps to a persistent OpenGL texture (`egui::TextureHandle`) that only uploads pixel data when the camera's atomic `frame_seq` counter advances.
   - Click-drag a rectangle around any region (`1.0x - 8.0x`, plus `Shift+Drag` or dock `Box` button for nested box-zoom and `Backspace` to step back), or use **native Linux Wayland two-finger touchpad pinch (`zwp_pointer_gestures_v1`)**, **zero-lag trackpad/wheel scroll**, **on-tile `-` / `1.0x` / `+` glass dock**, and **keyboard `+` / `-` / `z` / `zzz`**. Includes an interactive bottom-left wireframe radar minimap (click or drag inside the radar to pan across zoomed views) and zoomed-region PNG snapshots.
5. **Minimal Obsidian Studio UI, 16:9 Fit Framing & Borderless Zen Wall:**
   - Pure `#09090B` Obsidian canvas with a compact 38px 3-zone command bar, progressive-disclosure tile HUD (idle tiles show only a whisper-quiet status dot + camera name; hovering reveals FPS, zoom dock, and `SD`/`HD`/`Expand` micro-controls), undistorted **`16:9` Fit vs `Fill`** toggle (`A`), and **Borderless Zen Wall** mode (`B` or `F11` auto-hide with top-edge pointer peek).
6. **Zero-Config Cloud P2P Remote Access (Works on Any Internet Connection):**
   - Built-in native Rust Dahua / CP Plus / Amcrest Cloud P2P (`easy4ipcloud.com`) authenticated PTCP tunnel (`src/p2p.rs`). When away from home Wi-Fi (or when clicking the Top Bar `LAN / Cloud P2P` status capsule), the viewer automatically tunnels all 12 camera streams using your NVR's Serial Number (`RTSP_P2P_SERIAL`) with zero router port forwarding and zero extra home hardware.

---

## Quick Start

### Option A: Install from Ubuntu App Center / Snap Store (Recommended)

```bash
sudo snap install nvr-command-center
```

On first launch, **NVR Command Center** automatically opens the built-in **Quick Setup Guide** in `Settings -> 1. NVR & Cameras` where you can enter your NVR LAN IP, Cloud P2P Serial Number (`SN`), username, and password directly from the GUI.

### Option B: One-Command Source Install & Build

```bash
git clone https://github.com/pulkitmundra/nvr-command-center.git
cd nvr-command-center
./install.sh
```

### 2. Configure Your NVR / Cameras (In-App GUI or `.env`)

Click **Settings** inside the app (saved with `0600` permissions), or copy `.env.example` to `.env`:

```bash
cp .env.example .env
chmod 600 .env
```

```ini
RTSP_BRAND="dahua"             # "dahua" (Dahua/CP Plus/Amcrest), "hikvision", or "custom"
RTSP_HOST="192.168.1.100"      # Primary LAN NVR IP
RTSP_P2P_SERIAL="YOUR_NVR_SN"  # Optional Cloud P2P Serial Number (works on any internet connection!)
RTSP_USE_CLOUD_P2P="false"     # Auto-activates when LAN NVR IP is unreachable
RTSP_TAILSCALE_HOST="100.x.y.z"# Optional Tailscale remote IP for 1-click switching
RTSP_USE_TAILSCALE="false"
RTSP_PORT="554"
RTSP_USER="admin"
RTSP_PASSWORD="your_password_here"
NUM_CAMERAS="12"               # Dynamic 1-32 channels (default: 12 for 3x4 grid)
IP_CAMERA_START_CHANNEL="10"
DEFAULT_SUBTYPE="1"            # 1 = Sub-Stream (SD), 0 = Main-Stream (HD)
HWACCEL_MODE="auto"            # "auto" (VAAPI/NVDEC with CPU fallback) or "software"
```

### 3. Launch or Toggle the Viewer

```bash
./launch_viewer.sh start     # Start viewer in background
./launch_viewer.sh toggle    # Toggle viewer on/off (ideal for global keyboard shortcuts)
./launch_viewer.sh restart   # Restart viewer
./launch_viewer.sh scan      # Probe NVR channels 1..16 in parallel from CLI
```

To bind a global Ubuntu/GNOME keyboard shortcut (e.g. `Ctrl+Alt+C` or `Super+C`):

```bash
./setup_keyboard_shortcut.sh
```

---

## Keyboard Shortcuts & Controls

Single-tap shortcuts target the **active** (selected, Single View, or hovered) camera. Triple-tapping within `450ms` applies the action across the entire camera wall. Press **`?`** anywhere in the app to open the interactive Keyboard Shortcuts Cheat-Sheet.

| Shortcut / Action | Function |
| :--- | :--- |
| **`S` / `S S S` (or `Shift+S`)** | Switch active camera to **SD** (`S`) or switch **ALL** cameras to **SD** (`SSS` / `Shift+S`) |
| **`H` / `H H H` (or `Shift+H`)** | Switch active camera to **HD** (`H`) or switch **ALL** cameras to **HD** (`HHH` / `Shift+H`) |
| **`Drag Box` / `Shift+Drag` / `Box`** | **Smart Area Box Zoom (`e-PTZ`, `1.0x - 8.0x`)**: Drag a rectangle around any region to zoom smoothly into it (`Shift+Drag` or dock `Box` button for nested box-zoom) |
| **`Scroll` / `Pinch` / `+` / `-`** | **Cursor-Anchored GPU Zoom** (`1.0x - 8.0x`, including native Wayland 2-finger pinch) + **Click-Drag** pan or **Radar Minimap** click/drag |
| **`Backspace` / `Z` / `Z Z Z` / `Mid-Click`** | **Step back** to previous box zoom (`Backspace`), smooth reset active camera zoom (`Z` / `Mid-Click`), or reset **ALL** cameras (`ZZZ` / `Shift+Z`) |
| **`Click` / `Arrow Keys`** | Select camera across the 2D grid (or cycle cameras when in Single View) |
| **`Enter` / `F` / `Double-Click`** | Toggle **Single View** on the active camera (with optional `SD -> HD` warm-swap) |
| **`[` / `]`** | Cycle to Previous / Next camera |
| **`1` - `9`, `0`, `F1` - `F10`, `F12`** | Direct **Single View** for Cameras 1 - 12 (`0` resets zoom first if zoomed, else Cam 10; `Shift+F11` = Cam 11) |
| **`B`** | Toggle **Borderless Zen Wall** (hides top bar for 100% edge-to-edge video; hover top edge to peek) |
| **`F11`** | Toggle **OS Monitor Fullscreen** (auto-hides command bar after `2.8s` of pointer idle) |
| **`A`** | Toggle **`16:9` Fit** (undistorted letterbox/pillarbox) vs **`Fill` Tile** |
| **`C` or `Ctrl+S`** | **Save PNG Snapshot**: Stitched wall in Grid View, or full-res / zoomed-region crop in Single View |
| **`M`** | **Patrol Mode**: Automatically rotate Single View across cameras every `N` seconds |
| **`P` or `Space`** | **Pause / Resume** all camera streams |
| **`R`** | **Instant Re-Sync**: Flush all camera buffers and snap to live edge |
| **`?` / `Esc`** | Toggle **Keyboard Shortcuts Cheat-Sheet** (`?`) / Progressive **Escape** (`Reset Zoom -> Single -> Zen -> F11`) |
| **`Right-Click Any Tile`** | Context menu: Reset/Step-Back Zoom, Nested Box Zoom, Expand/Grid, `HD`/`SD`, Zoomed/Full Snapshot, Re-Sync, Rename, Swap |

---

## Multi-Brand NVR, DVR, XVR & IP Camera Support (Native Linux Alternative)

**NVR Command Center** serves as a fast, native Linux desktop replacement for proprietary Windows/Android CCTV & NVR surveillance clients:

| Hardware Brand / Ecosystem | Proprietary Windows / Mobile Apps Replaced | Preset (`RTSP_BRAND`) & RTSP / Cloud P2P Support |
| :--- | :--- | :--- |
| **Dahua / IMOU / Lorex** | **SmartPSS** (`Smart PSS`), **DMSS** (`gDMSS` / `iDMSS`), **Easy4ip** (`Easy4ipCloud`), **Imou Life** | `RTSP_BRAND="dahua"` + Built-in **Cloud P2P (`RTSP_P2P_SERIAL`)** (`rtsp://{user}:{password}@{host}:{port}/cam/realmonitor?channel={channel}&subtype={subtype}`) |
| **CP Plus (`CPPlus`)** | **KVMS Pro** (`KVMS`), **gCMOB**, **iCMOB**, **InstaOn**, **EzyKam** | `RTSP_BRAND="dahua"` + Built-in **Cloud P2P (`RTSP_P2P_SERIAL`)** |
| **Amcrest** | **Amcrest Surveillance Pro**, **Amcrest View Pro**, **Amcrest Cloud** | `RTSP_BRAND="dahua"` + Built-in **Cloud P2P (`RTSP_P2P_SERIAL`)** |
| **Hikvision / HiLook / EZVIZ** | **iVMS-4200** (`iVMS`), **Hik-Connect** (`HikConnect`), **SADP** | `RTSP_BRAND="hikvision"` (`rtsp://{user}:{password}@{host}:{port}/Streaming/Channels/{channel}0{1\|2}`) + LAN / Tailscale |
| **Reolink / TP-Link Tapo & VIGI / Uniview (`UNV`) / Axis / Foscam / Ubiquiti UniFi / ONVIF** | Reolink Client, EZView, UNV Guard, Frigate, ZoneMinder, Shinobi, Blue Iris, Agent DVR | `RTSP_BRAND="custom"` via custom `RTSP_CUSTOM_URL_TEMPLATE` (`H.265` / `HEVC` & `H.264`) |

---

## Frequently Asked Questions (Linux CCTV, NVR & Cloud P2P Viewer)

### How do I run Dahua SmartPSS / DMSS / Easy4ip or CP Plus KVMS Pro / gCMOB on Ubuntu & Linux?
Proprietary Windows/mobile CCTV apps like **Dahua SmartPSS**, **DMSS**, **CP Plus KVMS Pro (`gCMOB`)**, **Amcrest Surveillance Pro**, and **Hikvision iVMS-4200** do not provide native Linux desktop clients and often crash or lag under Wine. **NVR Command Center** is a native Rust (`eframe`/`egui` OpenGL `glow`) Linux replacement (`sudo snap install nvr-command-center`) that connects directly via LAN RTSP or remote **Cloud P2P Device Serial Number (`RTSP_P2P_SERIAL`)** with zero port-forwarding required.

### How do I view RTSP IP cameras with hardware acceleration (`VAAPI` / `NVDEC`) on Linux?
Set `HWACCEL_MODE="auto"` (default) in the in-app **Settings** modal or `.env`. Each camera worker automatically negotiates GPU hardware decoding (`VAAPI` on Intel/AMD or `NVDEC` on NVIDIA) with seamless fallback to multi-threaded software decoding, rendering up to 32 `H.265` (`HEVC`) / `H.264` camera channels at 60 FPS on a single `eframe`/`egui` OpenGL (`glow`) shader surface.

---

## More Open-Source Linux Apps by Pulkit Mundra

Explore the full portfolio, interactive systems simulators, and engineering deep-dives at **[pulkit-mundra-portfolio.vercel.app](https://pulkit-mundra-portfolio.vercel.app)**:

| Project | Description | Links |
| :--- | :--- | :--- |
| **[NVR Command Center](https://pulkit-mundra-portfolio.vercel.app/projects/nvr-command-center)** | Native Rust + OpenGL 12-32 camera RTSP & Cloud P2P surveillance wall (`< 80ms` latency, `~70 MB` RAM, `ioctl(FIONREAD)` burst drain) | [Case Study & Simulator](https://pulkit-mundra-portfolio.vercel.app/projects/nvr-command-center) \| [64 KB Pipe Trap Write-Up](https://pulkit-mundra-portfolio.vercel.app/engineering/python-to-rust-12-camera-nvr-linux-pipe-trap) \| [Snap Store](https://snapcraft.io/nvr-command-center) |
| **[Crunchyroll Ubuntu Desktop](https://github.com/pulkitmundra/crunchyroll-ubuntu-desktop)** | Unofficial Linux Desktop & HTPC client for Crunchyroll with Widevine DRM, Custom Lists, Auto-Skip HUD, In-Player Quality Selector, Sleep Timer & Auto-PiP Mini-Player | [GitHub](https://github.com/pulkitmundra/crunchyroll-ubuntu-desktop) \| [Snap Store](https://snapcraft.io/crunchyroll-ubuntu-desktop) \| [Case Study](https://pulkit-mundra-portfolio.vercel.app/projects/crunchyroll-ubuntu-desktop) \| [Wayland Deep Dive](https://pulkit-mundra-portfolio.vercel.app/engineering/wayland-fullscreen-black-curtain-translucent-linux-player) |
| **[Galaxy Buds Ubuntu Switcher](https://github.com/pulkitmundra/galaxy-buds-ubuntu)** | Universal Samsung Galaxy Buds (`Buds3 Pro`, `Buds2 Pro`, `Buds FE`, `Buds2`, `Buds Pro`, `Buds Live`, `Buds+`) GNOME 45-48 Quick Settings Switcher, Live Battery %, A2DP/HFP Toggle & Android Call Auto-Handoff | [GitHub](https://github.com/pulkitmundra/galaxy-buds-ubuntu) \| [GNOME Extensions](https://extensions.gnome.org/extension/11611/samsung-galaxy-buds-quick-settings-switcher/) \| [Case Study](https://pulkit-mundra-portfolio.vercel.app/projects/galaxy-buds-ubuntu-switcher) |

---

## Support & Author Links

If **NVR Command Center** helps power your home lab, office security wall, or NVR setup, consider supporting ongoing development:

- **Developer Portfolio & Interactive Architecture Lab:** [https://pulkit-mundra-portfolio.vercel.app](https://pulkit-mundra-portfolio.vercel.app) ([NVR Case Study](https://pulkit-mundra-portfolio.vercel.app/projects/nvr-command-center) | [Engineering Deep Dives](https://pulkit-mundra-portfolio.vercel.app/engineering))
- **Buy Me a Coffee:** [https://buymeacoffee.com/pulkitmundra](https://buymeacoffee.com/pulkitmundra)
- **GitHub Sponsors:** [https://github.com/sponsors/pulkitmundra](https://github.com/sponsors/pulkitmundra)

---

## License

Released under the [MIT License](LICENSE).
