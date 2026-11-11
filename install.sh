#!/bin/bash
# One-command installer for NVR Command Center v2.1.1 (Native Rust + GPU + Cloud P2P)

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

echo "=================================================="
echo "  NVR Command Center v2.1.1 - Native Rust Installer"
echo "=================================================="

# 1. Check ffmpeg / ffprobe
if ! command -v ffmpeg >/dev/null 2>&1; then
    echo "Installing ffmpeg..."
    sudo apt-get update && sudo apt-get install -y ffmpeg
fi

# 2. Check Rust / Cargo
CARGO_BIN="cargo"
if ! command -v cargo >/dev/null 2>&1; then
    if [ -x "$HOME/.cargo/bin/cargo" ]; then
        CARGO_BIN="$HOME/.cargo/bin/cargo"
    else
        echo "Error: Rust/Cargo not found. Install Rust via: curl https://sh.rustup.rs -sSf | sh"
        exit 1
    fi
fi

# 3. Create .env from .env.example if missing
if [ ! -f "$SCRIPT_DIR/.env" ] && [ -f "$SCRIPT_DIR/.env.example" ]; then
    cp "$SCRIPT_DIR/.env.example" "$SCRIPT_DIR/.env"
    chmod 600 "$SCRIPT_DIR/.env"
    echo "Created .env (0600 permissions) from .env.example."
fi

# 4. Build optimized native release binary
echo "Compiling Native Rust release binary..."
"$CARGO_BIN" build --release

chmod +x "$SCRIPT_DIR/launch_viewer.sh" "$SCRIPT_DIR/setup_keyboard_shortcut.sh"

# 5. Install desktop entry & hicolor icons for GNOME Dock / Alt+Tab
mkdir -p "$HOME/.local/share/applications"
mkdir -p "$HOME/.local/share/icons/hicolor/scalable/apps"
mkdir -p "$HOME/.local/share/icons/hicolor/256x256/apps"

cp "$SCRIPT_DIR/assets/icon.svg" "$HOME/.local/share/icons/hicolor/scalable/apps/rtsp-camera-viewer.svg"
cp "$SCRIPT_DIR/assets/icon.png" "$HOME/.local/share/icons/hicolor/256x256/apps/rtsp-camera-viewer.png"

cat > "$HOME/.local/share/applications/rtsp-camera-viewer.desktop" <<EOF
[Desktop Entry]
Version=1.0
Type=Application
Name=NVR Command Center
Comment=Ultra-low-latency GPU multi-camera NVR & Cloud P2P command center
Exec=$SCRIPT_DIR/launch_viewer.sh toggle
Icon=$SCRIPT_DIR/assets/icon.svg
Terminal=false
Categories=Video;AudioVideo;
Keywords=camera;rtsp;nvr;p2p;surveillance;video;rust;
StartupNotify=true
StartupWMClass=rtsp-camera-viewer
EOF

update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true
gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" 2>/dev/null || true

echo ""
echo "Installation complete!"
echo "  Binary:   $SCRIPT_DIR/target/release/rtsp-camera-viewer"
echo "  Launch:   ./launch_viewer.sh start"
echo "  Toggle:   ./launch_viewer.sh toggle"
echo "  Scan NVR: ./launch_viewer.sh scan"
