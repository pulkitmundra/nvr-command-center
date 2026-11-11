#!/bin/bash
# Launch script for RTSP Camera Viewer v2.1.1 (Native Rust + GPU Command Center)
# Supports: start | stop | toggle | status | restart | scan

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BINARY_PATH="$SCRIPT_DIR/target/release/rtsp-camera-viewer"
RUNTIME_DIR="${XDG_RUNTIME_DIR:-$HOME/.rtsp_viewer}"
mkdir -p "$RUNTIME_DIR"
chmod 700 "$RUNTIME_DIR" 2>/dev/null || true
PID_FILE="$RUNTIME_DIR/rtsp_viewer.pid"
LOG_FILE="$SCRIPT_DIR/rtsp_viewer.log"

is_viewer_pid() {
    local pid="$1"
    [ -n "$pid" ] && ps -p "$pid" > /dev/null 2>&1 && tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -q "rtsp-camera-viewer"
}

ensure_binary() {
    if [ ! -x "$BINARY_PATH" ]; then
        echo "Building RTSP Camera Viewer release binary..."
        cd "$SCRIPT_DIR" || exit 1
        if command -v cargo >/dev/null 2>&1; then
            cargo build --release
        elif [ -x "$HOME/.cargo/bin/cargo" ]; then
            "$HOME/.cargo/bin/cargo" build --release
        else
            echo "Error: cargo not found. Install Rust via https://rustup.rs"
            exit 1
        fi
    fi
}

is_running() {
    if [ -f "$PID_FILE" ]; then
        PID=$(cat "$PID_FILE")
        if is_viewer_pid "$PID"; then
            return 0
        else
            rm -f "$PID_FILE"
            return 1
        fi
    fi
    return 1
}

start_viewer() {
    ensure_binary
    echo "Starting RTSP Camera Viewer v2.1.1..."
    cd "$SCRIPT_DIR" || exit 1

    USER_RUN_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
    if [ -z "${WAYLAND_DISPLAY:-}" ] && [ -S "$USER_RUN_DIR/wayland-0" ]; then
        WAYLAND_DISPLAY="wayland-0"
    fi

    if command -v systemd-run >/dev/null 2>&1; then
        systemctl --user stop rtsp-camera-viewer.service 2>/dev/null || true
        systemctl --user reset-failed rtsp-camera-viewer.service 2>/dev/null || true
        systemd-run --user --collect --unit=rtsp-camera-viewer \
            --working-directory="$SCRIPT_DIR" \
            -E DISPLAY="${DISPLAY:-:0}" \
            -E WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-}" \
            -E XDG_RUNTIME_DIR="$USER_RUN_DIR" \
            -E XDG_SESSION_TYPE="${XDG_SESSION_TYPE:-wayland}" \
            -E XAUTHORITY="${XAUTHORITY:-}" \
            "$BINARY_PATH" > "$LOG_FILE" 2>&1
    else
        DISPLAY="${DISPLAY:-:0}" WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-}" XDG_RUNTIME_DIR="$USER_RUN_DIR" setsid "$BINARY_PATH" > "$LOG_FILE" 2>&1 < /dev/null &
    fi
    sleep 0.3
    REAL_PID=$(pgrep -n -f "$BINARY_PATH" || echo "")
    if [ -n "$REAL_PID" ]; then
        echo "$REAL_PID" > "$PID_FILE"
    fi

    echo "RTSP Camera Viewer started (PID: $(cat "$PID_FILE" 2>/dev/null || echo 'unknown'))"
    echo "Log file: $LOG_FILE"
}

stop_viewer() {
    systemctl --user stop rtsp-camera-viewer.service 2>/dev/null || true
    if [ -f "$PID_FILE" ]; then
        PID=$(cat "$PID_FILE")
        if is_viewer_pid "$PID"; then
            echo "Stopping RTSP Camera Viewer (PID: $PID)..."
            kill "$PID" 2>/dev/null || true

            for _ in {1..10}; do
                if ! is_viewer_pid "$PID"; then
                    echo "RTSP Camera Viewer stopped gracefully"
                    rm -f "$PID_FILE"
                    return 0
                fi
                sleep 0.2
            done

            echo "Force killing RTSP Camera Viewer..."
            kill -9 "$PID" 2>/dev/null || true
            echo "RTSP Camera Viewer stopped (forced)"
        fi
        rm -f "$PID_FILE"
    else
        pkill -f "target/release/rtsp-camera-viewer" 2>/dev/null || true
        echo "RTSP Camera Viewer is not running"
    fi
}

toggle_viewer() {
    if is_running; then
        stop_viewer
    else
        start_viewer
    fi
}

case "${1:-toggle}" in
    start)
        if is_running; then
            echo "RTSP Camera Viewer is already running (PID: $(cat "$PID_FILE"))"
        else
            start_viewer
        fi
        ;;
    stop)
        stop_viewer
        ;;
    toggle)
        toggle_viewer
        ;;
    status)
        if is_running; then
            echo "RTSP Camera Viewer is running (PID: $(cat "$PID_FILE"))"
        else
            echo "RTSP Camera Viewer is not running"
        fi
        ;;
    restart)
        stop_viewer
        sleep 0.3
        start_viewer
        ;;
    scan)
        ensure_binary
        cd "$SCRIPT_DIR" || exit 1
        "$BINARY_PATH" --scan
        ;;
    *)
        echo "Usage: $0 {start|stop|toggle|status|restart|scan}"
        exit 1
        ;;
esac
