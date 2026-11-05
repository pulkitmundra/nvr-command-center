# NVR Command Center v2.1 (Native Rust + GPU + Cloud P2P)

[![Version](https://img.shields.io/badge/version-2.1.0-blue.svg)](https://github.com/pulkitmundra/nvr-command-center/releases)
[![Snap Store](https://img.shields.io/badge/Snap_Store-nvr--command--center-82BEA0?logo=snapcraft&logoColor=white)](https://snapcraft.io/nvr-command-center)
[![Rust](https://img.shields.io/badge/Rust-Native_GPU-orange?logo=rust)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/Platform-Linux%20%7C%20Ubuntu-E95420?logo=ubuntu&logoColor=white)](https://github.com/pulkitmundra/nvr-command-center)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)
[![Buy Me A Coffee](https://img.shields.io/badge/Buy%20Me%20A%20Coffee-ffdd00?logo=buy-me-a-coffee&logoColor=black)](https://buymeacoffee.com/pulkitmundra)
[![GitHub Sponsors](https://img.shields.io/badge/Sponsor-%E2%9D%A4-ea4aaa?logo=github-sponsors)](https://github.com/sponsors/pulkitmundra)

An ultra-low-latency, GPU-accelerated multi-camera NVR/DVR surveillance command center built in **Native Rust (`eframe` / `egui` + OpenGL `glow`)** with parallel zero-probe FFmpeg workers, kernel pipe burst draining, and built-in **Cloud P2P remote tunneling**.

Designed to stream **12+ concurrent cameras (`1 - 32` channels)** 24/7 with **`< 0.9s` parallel startup**, **`< 80ms` glass-to-glass latency**, **zero stream drift**, and **`~65 - 110 MB` total RAM** - on both local LAN and any remote internet connection.

---

## Why Native Rust v2.1?

| Metric | Legacy Python (`v1.x` Tkinter + OpenCV) | **Native Rust (`v2.1` `eframe/egui` + GPU + Zero-Lag Drain)** |
| :--- | :--- | :--- |
| **Cold Binary Launch** | `~1.2s` (Python + NumPy + OpenCV import) | **`< 40ms` (Standalone native ELF binary)** |
| **12-Camera Parallel Startup** | `12s - 20s` (Sequential 5 MB FFmpeg probe lock) | **`< 0.9s` (`probesize=32768`, `analyzeduration=0`, `fpsprobesize=0`)** |
| **Live Stream Latency & Sync** | `2.0s - 5.0s` lag & inter-camera drift | **`< 80ms` synchronized live edge (`ioctl(FIONREAD)` burst drain)** |
| **Digital Zoom & Pan (`1x-5x`)** | CPU NumPy crop + `cv2.resize` on every frame | **`0.0%` CPU cost (OpenGL UV texture coordinate transform)** |
| **Per-Camera `SD <-> HD` & `Single` View** | `1.5s - 3.0s` black screen reconnect | **`0ms` black flash (Independent 1-click `[SD]/[HD]` & `[Single]/[Grid]` per tile)** |
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
4. **Persistent GPU Texture Pool & UV Shader Zoom:**
   - Each camera maps to an OpenGL texture (`egui::TextureHandle`) that only uploads pixel data when the camera's atomic `frame_seq` counter advances. Mouse-wheel zoom (`1.0x - 5.0x`) and click-drag pan manipulate GPU UV coordinates directly with zero CPU image resizing.
5. **Zero-Config Cloud P2P Remote Access (Works on Any Internet Connection):**
   - Built-in native Rust Dahua / CP Plus / Amcrest Cloud P2P (`easy4ipcloud.com`) authenticated PTCP tunnel (`src/p2p.rs`). When away from home Wi-Fi (or when clicking the Top Bar `LAN / Cloud P2P` toggle), the viewer automatically tunnels all 12 camera streams using your NVR's Serial Number (`RTSP_P2P_SERIAL`) with zero router port forwarding and zero extra home hardware.

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

| Shortcut / Action | Function |
| :--- | :--- |
| **`1` - `9`, `0`, `-`, `=`** | Jump directly to Fullscreen for **Cameras 1 through 12** (`0` = Cam 10, `-` = Cam 11, `=` = Cam 12) |
| **`Double-Click` / `F11` / `Esc`** | Enter or exit Fullscreen mode (with seamless `SD -> HD` warm-swap) |
| **`Left` / `Right` (or `[` / `]`)** | Cycle to Previous / Next camera while in Fullscreen |
| **`Mouse Wheel` + `Click-Drag`** | GPU UV Digital Zoom (`1.0x - 5.0x`) and Pan on any camera tile |
| **`R`** | **Instant Re-Sync**: Flush all camera buffers and snap to live edge |
| **`P` or `Space`** | **Pause / Resume** all camera streams |
| **`S`** | **Save PNG Snapshot**: Saves a stitched 12-camera composite wall in Grid View, or full-res HD frame in Fullscreen |
| **`M`** | **Patrol Mode**: Automatically rotate Fullscreen across all cameras every `N` seconds |
| **`Right-Click Any Tile`** | Context menu: Rename Camera, Switch `HD`/`SD`, Swap Tile Position, Snapshot, Reset Zoom |

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
Proprietary Windows/mobile CCTV apps like **Dahua SmartPSS**, **DMSS**, **CP Plus KVMS Pro (`gCMOB`)**, **Amcrest Surveillance Pro**, and **Hikvision iVMS-4200** do not provide native Linux desktop clients and often crash or lag under Wine. **NVR Command Center** is a native Rust + `wgpu` Linux replacement (`sudo snap install nvr-command-center`) that connects directly via LAN RTSP or remote **Cloud P2P Device Serial Number (`RTSP_P2P_SERIAL`)** with zero port-forwarding required.

### How do I view RTSP IP cameras with hardware acceleration (`VAAPI` / `NVDEC`) on Linux?
Set `HWACCEL_MODE="auto"` (default) in the in-app **Settings (`⚙`)** modal or `config.env`. Each camera worker automatically negotiates GPU hardware decoding (`VAAPI` on Intel/AMD or `NVDEC` on NVIDIA) with seamless fallback to multi-threaded software decoding, rendering up to 32 `H.265` (`HEVC`) / `H.264` camera channels at 60 FPS on a single `wgpu` shader surface.

---

## More Linux & Ubuntu Desktop Apps by Pulkit Mundra

| Project | Description | Links |
| :--- | :--- | :--- |
| **[Crunchyroll Ubuntu Desktop](https://github.com/pulkitmundra/crunchyroll-ubuntu-desktop)** | Unofficial Linux Desktop & HTPC client for Crunchyroll with Widevine DRM, Custom Lists, Auto-Skip HUD, In-Player Quality Selector, Sleep Timer & Auto-PiP Mini-Player | [GitHub](https://github.com/pulkitmundra/crunchyroll-ubuntu-desktop) · [Snap Store](https://snapcraft.io/crunchyroll-ubuntu-desktop) |
| **[Galaxy Buds Ubuntu Switcher](https://github.com/pulkitmundra/galaxy-buds-ubuntu)** | Universal Samsung Galaxy Buds (`Buds3 Pro`, `Buds2 Pro`, `Buds FE`, `Buds2`, `Buds Pro`, `Buds Live`, `Buds+`) GNOME 45-48 Quick Settings Switcher, Live Battery %, A2DP/HFP Toggle & Android Call Auto-Handoff | [GitHub](https://github.com/pulkitmundra/galaxy-buds-ubuntu) · [GNOME Extensions](https://extensions.gnome.org/extension/11611/samsung-galaxy-buds-quick-settings-switcher/) |

---

## Support the Project

If **NVR Command Center** helps power your home lab, office security wall, or NVR setup, consider supporting ongoing development:

- **Buy Me a Coffee:** [https://buymeacoffee.com/pulkitmundra](https://buymeacoffee.com/pulkitmundra)
- **GitHub Sponsors:** [https://github.com/sponsors/pulkitmundra](https://github.com/sponsors/pulkitmundra)

---

## License

Released under the [MIT License](LICENSE).
