# NVR Command Center v3.0 (Native Rust + GPU + AI Playback + Cloud P2P)

[![Version](https://img.shields.io/badge/version-3.0.1-blue.svg)](https://github.com/pulkitmundra/nvr-command-center/releases)
[![Snap Store](https://img.shields.io/badge/Snap_Store-nvr--command--center-82BEA0?logo=snapcraft&logoColor=white)](https://snapcraft.io/nvr-command-center)
[![Case Study](https://img.shields.io/badge/Case_Study-Interactive_Demo-10B981?logo=vercel&logoColor=white)](https://pulkit-mundra-portfolio.vercel.app/projects/nvr-command-center)
[![Architecture Deep Dive](https://img.shields.io/badge/Deep_Dive-64KB_Linux_Pipe_Trap-0EA5E9?logo=linux&logoColor=white)](https://pulkit-mundra-portfolio.vercel.app/engineering/python-to-rust-12-camera-nvr-linux-pipe-trap)
[![Portfolio](https://img.shields.io/badge/Portfolio-pulkit--mundra-2563EB?logo=vercel&logoColor=white)](https://pulkit-mundra-portfolio.vercel.app)
[![Rust](https://img.shields.io/badge/Rust-Native_GPU-orange?logo=rust)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/Platform-Linux%20%7C%20Ubuntu-E95420?logo=ubuntu&logoColor=white)](https://github.com/pulkitmundra/nvr-command-center)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)
[![Buy Me A Coffee](https://img.shields.io/badge/Buy%20Me%20A%20Coffee-ffdd00?logo=buy-me-a-coffee&logoColor=black)](https://buymeacoffee.com/pulkitmundra)
[![GitHub Sponsors](https://img.shields.io/badge/Sponsor-%E2%9D%A4-ea4aaa?logo=github-sponsors)](https://github.com/sponsors/pulkitmundra)

An ultra-low-latency, GPU-accelerated multi-camera NVR/DVR live security wall and **interactive "What Happened" playback client** built in **Native Rust (`eframe` / `egui` + OpenGL `glow`)** with parallel zero-probe FFmpeg workers, kernel pipe burst draining, a distraction-free **Obsidian Studio** UI, **Multi-Scale AI Event Timeline & Smart Skip**, and built-in **Dual-Port Cloud P2P remote tunneling**.

Designed to stream **12+ concurrent live cameras (`1 - 32` channels)** 24/7 with **`< 0.9s` parallel startup**, **`< 80ms` glass-to-glass latency**, **zero stream drift**, and **`~65 - 110 MB` live RAM** - plus instant `1080p` recorded history scrubbing, `-4x .. 16x` trick-play, single-frame step, and `.MP4` clip export on both local LAN and any remote internet connection.

> **Interactive Simulator & Systems Writeup:** Try the live browser-based 12-camera pipe-drain simulator in the [**Case Study**](https://pulkit-mundra-portfolio.vercel.app/projects/nvr-command-center) or read the engineering postmortem: [**650 MB to 70 MB RAM: Porting a 12-Camera Python CCTV Wall & Solving the 64 KB Linux Pipe Trap ->**](https://pulkit-mundra-portfolio.vercel.app/engineering/python-to-rust-12-camera-nvr-linux-pipe-trap)

---

## Why Native Rust v3.0?

| Metric | Legacy Python / Generic NVR Web UIs | **Native Rust (`v3.0` `eframe/egui` + GPU + AI Playback)** |
| :--- | :--- | :--- |
| **Cold Binary Launch** | `~1.2s` (Python + NumPy + OpenCV import) | **`< 40ms` (Standalone native ELF binary)** |
| **12-Camera Parallel Startup** | `12s - 20s` (Sequential 5 MB FFmpeg probe lock) | **`< 0.9s` (`probesize=32768`, `analyzeduration=0`, `fpsprobesize=0`)** |
| **Live Stream Latency & Sync** | `2.0s - 5.0s` lag & inter-camera drift | **`< 80ms` synchronized live edge (`ioctl(FIONREAD)` burst drain)** |
| **Recorded History & "What Happened"** | Clunky plugins, invisible `3s` motion ticks, frozen `4x+` fast-forward | **Magnetic `24h -> 2m` AI Event Timeline + `Smart Skip` (`8x` Glide / Jump) + `-4x..16x` & `,`/`.` Frame-Step** |
| **Area Box Zoom & GPU e-PTZ (`1x-8x`)** | CPU NumPy crop + `cv2.resize` (resets on seek) | **`0.0%` CPU cost (Persistent across Live, Scrubbing, Frame-Step & PNG/MP4 Export)** |
| **Credential & Process Security** | Plaintext `rtsp://user:pass@` visible in `ps aux` | **Zero-leak `127.0.0.1` `AuthGateway` (`OsRng` 1-time tokens) + `0600` storage + Canary lockout guard** |
| **24/7 Memory Footprint** | `550 MB - 900 MB` | **`~65 MB - 110 MB` Live (Unlinked `memfd_create` RAM cache for playback, `0` `/tmp` files)** |

---

## Architecture Highlights

1. **Interactive "What Happened" NVR Playback & Multi-Scale Magnetic Timeline (`v3.0`):**
   - Press **`T`** (or click **`History`** in the Top Bar) on any camera to open full **`1920x1080` Main-Stream** recorded playback with a **`98px` Obsidian Timeline Dock**, optional **`240px` Event Drawer**, and a floating **`196x110` Live Picture-in-Picture (PiP)** corner window for `0ms` 1-click return to Live (plus **`TTT`** for `2x2` 4-camera synchronized playback on LAN).
   - Scroll-wheel zoom smoothly across **`24h <-> 6h <-> 1h <-> 15m <-> 5m <-> 2m`** centered on your cursor, with **sub-pixel event dilation (`>= 3.5px`)** so even a `2s` human/vehicle trigger stands out clearly at `24h` view across semantic lanes (`Human`, `Vehicle`, `Intrusion/Alarm`, `Motion`, `Continuous`).
   - **`Smart Skip` Dead-Air Engine**: Toggle between **`8x Fast-Glide`** (glides over empty footage at `8x`, automatically dropping to `1.0x` `3s` before matching events) and **`Instant Event Jump`**, plus **`-4x .. 16x` trick-play (`J`/`K`/`L`)**, **`0ms` `,`/`.` single-frame step** (via unlinked Linux `memfd_create` RAM chunks + `DHAV` header parser), and **2-click lossless `.MP4` clip export (`I` -> `O` -> `Export MP4`)**.
2. **Zero-Lag Kernel Pipe Burst Drain (`ioctl(FIONREAD)`):**
   - Each Rust worker thread drains its live stream at **100% native wire rate** and queries `libc::ioctl(fd, libc::FIONREAD)` on the `1 MiB` pipe after every frame, discarding intermediate burst packets before locking `FrameSlot` to guarantee **`0` buffered frames** across all 12+ cameras.
3. **Instant Parallel Startup (`-fpsprobesize 0` + Hybrid Priority) & 4.5s Socket Watchdog:**
   - Passing `-probesize 32768 -analyzeduration 0 -fpsprobesize 0 -fflags nobuffer+discardcorrupt -flags low_delay` forces immediate first-keyframe decoding in `< 0.9s`, supervised 24/7 by a frozen-socket watchdog.
4. **Smart Area Box Zoom (`e-PTZ`), Native Wayland Pinch & Interactive Radar Minimap:**
   - Click-drag a rectangle around any region (`1.0x - 8.0x`, `Shift+Drag` / `Box` for nested zoom, `Backspace` to step back), or use **native Wayland two-finger touchpad pinch (`zwp_pointer_gestures_v1`)**, **zero-lag trackpad/wheel scroll**, and the **interactive bottom-left wireframe radar minimap** - preserved seamlessly across both Live and Recorded Playback.
5. **Dual-Port Cloud P2P Remote Access (`RTSP :554` + `HTTP :80` on One Session):**
   - Built-in native Rust Dahua / CP Plus / Amcrest Cloud P2P authenticated PTCP tunnel (`src/p2p.rs`) with multi-profile fallback across Dahua **Easy4IP** (`easy4ipcloud.com`), **Dolynk Cloud** (`dolynkcloud.com`), and CP Plus **InstaOn Cloud** (`instaonserver.com` / `instaonserverctc.com`), multiplexing **both** live RTSP (`:554`) and NVR HTTP CGI / `/RPC2` (`:80`) over a single encrypted session with `SO_PEERCRED` local UID verification and automatic LAN <-> Cloud P2P switching.
6. **Defense-in-Depth Security (`AuthGateway` + Canary Circuit Breaker):**
   - In-process `127.0.0.1:0` `AuthGateway` (`src/auth_gateway.rs`) strips NVR passwords from `ffmpeg`/`ffprobe` CLI args (`ps aux` / `/proc/<pid>/cmdline` never expose secrets), blocks Basic-auth downgrades, enforces `PR_SET_DUMPABLE=0` + `Zeroize` + `0600` `O_NOFOLLOW` file creation, and probes Channel 1 first to prevent multi-camera `401` NVR account lockouts.

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
install -m 600 .env.example .env
```

```ini
RTSP_BRAND="dahua"             # "dahua" (Dahua/CP Plus/Amcrest), "hikvision", or "custom"
RTSP_HOST="192.168.1.100"      # Primary LAN NVR IP
RTSP_P2P_SERIAL="YOUR_NVR_SN"  # Optional Cloud P2P Serial Number (Live + Playback on any internet!)
RTSP_USE_CLOUD_P2P="false"     # Auto-activates when LAN NVR IP is unreachable
RTSP_TAILSCALE_HOST="100.x.y.z"# Optional Tailscale remote IP for 1-click switching
RTSP_USE_TAILSCALE="false"
RTSP_PORT="554"                # RTSP Port (default: 554)
RTSP_HTTP_PORT="80"            # NVR HTTP/CGI Port for Playback Timeline & AI Events (default: 80)
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
| **`T` / `T T T`** | Toggle **1080p Playback History + Timeline + Live PiP** (`T`) or **2x2 Sync Playback Grid** (`TTT` on LAN) |
| **`J` / `K` (or `Space`) / `L`** | **Playback Trick-Play**: Slower / Reverse (`J`), Play/Pause (`K` / `Space`), Faster up to `16x` (`L`) |
| **`,` / `.` (in Playback)** | **Exact Single-Frame Step** (`-1` frame / `+1` frame via in-RAM ring buffer) |
| **`Left` / `Right` (`Shift`: `30s`)** | Seek `-5s` / `+5s` (`-30s` / `+30s` with `Shift`) in Playback, or navigate 2D grid in Live |
| **`[` / `]`** | Jump to **Previous / Next AI Event** in Playback, or cycle cameras in Live |
| **`I` / `O` / `Ctrl+E`** | Mark Clip **In** (`I`), Mark Clip **Out** (`O`), and **Export Lossless `.MP4` Clip** (`Ctrl+E`) |
| **`S` / `S S S` (or `Shift+S`)** | Switch active camera to **SD** (`S`) or switch **ALL** cameras to **SD** (`SSS` / `Shift+S`) |
| **`H` / `H H H` (or `Shift+H`)** | Switch active camera to **HD** (`H`) or switch **ALL** cameras to **HD** (`HHH` / `Shift+H`) |
| **`Drag Box` / `Shift+Drag` / `Box`** | **Smart Area Box Zoom (`e-PTZ`, `1.0x - 8.0x`)**: Drag a rectangle around any region to zoom smoothly into it (`Shift+Drag` or dock `Box` button for nested box-zoom) |
| **`Scroll` / `Pinch` / `+` / `-`** | **Cursor-Anchored GPU Zoom** (`1.0x - 8.0x` on video, or `24h <-> 2m` zoom on Timeline) |
| **`Backspace` / `Z` / `Z Z Z` / `Mid-Click`** | **Step back** to previous box zoom (`Backspace`), smooth reset active camera zoom (`Z` / `Mid-Click`), or reset **ALL** cameras (`ZZZ` / `Shift+Z`) |
| **`Enter` / `F` / `Double-Click`** | Toggle **Single View** on the active camera (with optional `SD -> HD` warm-swap) |
| **`1` - `9`, `0`, `F1` - `F10`, `F12`** | Direct camera focus (`0` resets zoom first if zoomed, else Cam 10; switches active Playback camera when in History mode) |
| **`B` / `F11` / `A`** | **Borderless Zen Wall** (`B`), **OS Monitor Fullscreen** (`F11`), **`16:9` Fit vs `Fill`** (`A`) |
| **`C` or `Ctrl+S`** | **Save PNG Snapshot**: Stitched wall in Grid, or full-res / zoomed-region crop in Single & Historical Playback |
| **`M` / `P` / `R`** | **Patrol Mode** (`M`), **Pause/Resume Live Wall** (`P`), **Instant Live Re-Sync** (`R`) |
| **`?` / `Esc`** | **Keyboard Shortcuts Cheat-Sheet** (`?`) / Progressive **Escape** (`Reset Zoom -> Exit Playback -> Single -> Zen -> F11`) |
| **`Right-Click Any Tile`** | Context menu: View Playback History (`T`), Reset/Step-Back Zoom, Expand/Grid, `HD`/`SD`, Snapshot, Re-Sync, Rename, Swap |

---

## Multi-Brand NVR, DVR, XVR & IP Camera Support (Native Linux Alternative)

**NVR Command Center** serves as a fast, native Linux desktop replacement for proprietary Windows/Android CCTV & NVR surveillance clients:

| Hardware Brand / Ecosystem | Proprietary Windows / Mobile Apps Replaced | Preset (`RTSP_BRAND`) & RTSP / Cloud P2P Support |
| :--- | :--- | :--- |
| **Dahua / IMOU / Lorex** | **SmartPSS** (`Smart PSS`), **DMSS** (`gDMSS` / `iDMSS`), **Easy4ip** (`Easy4ipCloud`), **Imou Life** | `RTSP_BRAND="dahua"` + Built-in **Cloud P2P (`RTSP_P2P_SERIAL`)** (`rtsp://{user}:{password}@{host}:{port}/cam/realmonitor?channel={channel}&subtype={subtype}`) |
| **CP Plus (`CPPlus`)** | **KVMS Pro** (`KVMS`), **gCMOB**, **iCMOB**, **InstaOn**, **EzyKam** | `RTSP_BRAND="dahua"` + Built-in **Cloud P2P (`RTSP_P2P_SERIAL`)** with native **InstaOn Cloud** (`instaonserver.com` & `instaonserverctc.com` via KVMS Pro Lite / gCMOB profiles) |
| **Amcrest** | **Amcrest Surveillance Pro**, **Amcrest View Pro**, **Amcrest Cloud** | `RTSP_BRAND="dahua"` + Built-in **Cloud P2P (`RTSP_P2P_SERIAL`)** |
| **Hikvision / HiLook / EZVIZ** | **iVMS-4200** (`iVMS`), **Hik-Connect** (`HikConnect`), **SADP** | `RTSP_BRAND="hikvision"` (`rtsp://{user}:{password}@{host}:{port}/Streaming/Channels/{channel}0{1\|2}`) + LAN / Tailscale |
| **Reolink / TP-Link Tapo & VIGI / Uniview (`UNV`) / Axis / Foscam / Ubiquiti UniFi / ONVIF** | Reolink Client, EZView, UNV Guard, Frigate, ZoneMinder, Shinobi, Blue Iris, Agent DVR | `RTSP_BRAND="custom"` via custom `RTSP_CUSTOM_URL_TEMPLATE` (`H.265` / `HEVC` & `H.264`) |

---

## Frequently Asked Questions (Linux CCTV, NVR & Cloud P2P Viewer)

### How do I run Dahua SmartPSS / DMSS / Easy4ip or CP Plus KVMS Pro / gCMOB on Ubuntu & Linux?
Proprietary Windows/mobile CCTV apps like **Dahua SmartPSS**, **DMSS**, **CP Plus KVMS Pro (`gCMOB`)**, **Amcrest Surveillance Pro**, and **Hikvision iVMS-4200** do not provide native Linux desktop clients and often crash or lag under Wine. **NVR Command Center** is a native Rust (`eframe`/`egui` OpenGL `glow`) Linux replacement (`sudo snap install nvr-command-center`) that connects directly via LAN RTSP or remote **Cloud P2P Device Serial Number (`RTSP_P2P_SERIAL`)** with zero port-forwarding required. It natively speaks both Dahua Cloud (Easy4IP / Dolynk) and CP Plus InstaOn Cloud (`instaonserver.com` KVMS Pro / gCMOB protocols) out-of-the-box.

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

## Contributors

- **Dinesh Manajipet ([@saidinesh5](https://github.com/saidinesh5))** - Multi-profile Cloud P2P handshake support for CP Plus InstaOn (`instaonserver.com`, `instaonserverctc.com`) and Dahua Dolynk Cloud (`dolynkcloud.com`) with WSSE digest test vectors.

---

## License

Released under the [MIT License](LICENSE).
