#!/bin/bash
# Launch script for NVR Command Center v3.0.2 (Native Rust + GPU + Interactive Playback)
# Supports: start | stop | toggle | status | restart | scan | help

umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BINARY_PATH="$SCRIPT_DIR/target/release/rtsp-camera-viewer"
RUNTIME_DIR="${XDG_RUNTIME_DIR:-$HOME/.rtsp_viewer}"
mkdir -p "$RUNTIME_DIR"
chmod 700 "$RUNTIME_DIR" 2>/dev/null || true
PID_FILE="$RUNTIME_DIR/rtsp_viewer.pid"
LOG_FILE="$RUNTIME_DIR/rtsp_viewer.log"

rotate_log_if_needed() {
    if [ -f "$LOG_FILE" ]; then
        local sz
        sz=$(wc -c < "$LOG_FILE" 2>/dev/null || echo 0)
        if [ "${sz:-0}" -gt 2097152 ]; then
            tail -c 524288 "$LOG_FILE" > "${LOG_FILE}.tmp" 2>/dev/null && mv -f "${LOG_FILE}.tmp" "$LOG_FILE"
        fi
        chmod 600 "$LOG_FILE" 2>/dev/null || true
    fi
}

is_viewer_pid() {
    local pid="$1" exe
    [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null || return 1
    exe="$(readlink "/proc/$pid/exe" 2>/dev/null || true)"
    case "$exe" in
        */rtsp-camera-viewer|*/rtsp-camera-viewer\ \(deleted\))
            return 0
            ;;
    esac
    tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -q "rtsp-camera-viewer"
}

find_viewer_pids() {
    local uid found=0 p exe
    uid="$(id -u)"
    for p in /proc/[0-9]*; do
        [ -O "$p" ] || continue
        exe="$(readlink "$p/exe" 2>/dev/null || true)"
        case "$exe" in
            */rtsp-camera-viewer|*/rtsp-camera-viewer\ \(deleted\))
                echo "${p#/proc/}"
                found=1
                ;;
        esac
    done
    if [ "$found" -eq 0 ]; then
        pgrep -u "$uid" -f "(^|/)rtsp-camera-viewer([[:space:]]|$)" 2>/dev/null | awk -v self="$$" '$1 != self' || true
    fi
}

ensure_binary() {
    if [ ! -x "$BINARY_PATH" ]; then
        echo "Building NVR Command Center v3.0.2 release binary..."
        cd "$SCRIPT_DIR" || exit 1
        if command -v cargo >/dev/null 2>&1; then
            cargo build --release --locked
        elif [ -x "$HOME/.cargo/bin/cargo" ]; then
            "$HOME/.cargo/bin/cargo" build --release --locked
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
        fi
    fi
    local discovered
    discovered="$(find_viewer_pids | sort -n | tail -n 1)"
    if [ -n "$discovered" ] && is_viewer_pid "$discovered"; then
        echo "$discovered" > "$PID_FILE"
        chmod 600 "$PID_FILE" 2>/dev/null || true
        return 0
    fi
    return 1
}

start_viewer() {
    ensure_binary
    rotate_log_if_needed
    echo "Starting NVR Command Center v3.0.2..."
    cd "$SCRIPT_DIR" || exit 1

    USER_RUN_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
    if [ -z "${WAYLAND_DISPLAY:-}" ] && [ -S "$USER_RUN_DIR/wayland-0" ]; then
        WAYLAND_DISPLAY="wayland-0"
    fi

    SESSION_ENV_ARGS=()
    if [ -n "${WAYLAND_DISPLAY:-}" ]; then
        SESSION_ENV_ARGS=(-E "XDG_SESSION_TYPE=${XDG_SESSION_TYPE:-wayland}")
    elif [ -n "${XDG_SESSION_TYPE:-}" ]; then
        SESSION_ENV_ARGS=(-E "XDG_SESSION_TYPE=${XDG_SESSION_TYPE}")
    fi

    : > "$LOG_FILE"
    chmod 600 "$LOG_FILE" 2>/dev/null || true

    if command -v systemd-run >/dev/null 2>&1; then
        systemctl --user stop rtsp-camera-viewer.service 2>/dev/null || true
        systemctl --user reset-failed rtsp-camera-viewer.service 2>/dev/null || true
        systemd-run --user --collect --unit=rtsp-camera-viewer \
            --working-directory="$SCRIPT_DIR" \
            -E DISPLAY="${DISPLAY:-:0}" \
            -E WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-}" \
            -E XDG_RUNTIME_DIR="$USER_RUN_DIR" \
            "${SESSION_ENV_ARGS[@]}" \
            -E XAUTHORITY="${XAUTHORITY:-}" \
            "$BINARY_PATH" >> "$LOG_FILE" 2>&1
    else
        if [ -n "${WAYLAND_DISPLAY:-}" ]; then
            DISPLAY="${DISPLAY:-:0}" WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-}" XDG_SESSION_TYPE="${XDG_SESSION_TYPE:-wayland}" XDG_RUNTIME_DIR="$USER_RUN_DIR" setsid "$BINARY_PATH" >> "$LOG_FILE" 2>&1 < /dev/null &
        else
            DISPLAY="${DISPLAY:-:0}" WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-}" XDG_RUNTIME_DIR="$USER_RUN_DIR" setsid "$BINARY_PATH" >> "$LOG_FILE" 2>&1 < /dev/null &
        fi
    fi
    sleep 0.3
    REAL_PID="$(find_viewer_pids | sort -n | tail -n 1)"
    if [ -n "$REAL_PID" ]; then
        echo "$REAL_PID" > "$PID_FILE"
        chmod 600 "$PID_FILE" 2>/dev/null || true
    fi

    echo "NVR Command Center started (PID: $(cat "$PID_FILE" 2>/dev/null || echo 'unknown'))"
    echo "Log file: $LOG_FILE"
}

stop_viewer() {
    systemctl --user stop rtsp-camera-viewer.service 2>/dev/null || true
    if [ -f "$PID_FILE" ]; then
        PID=$(cat "$PID_FILE")
        if is_viewer_pid "$PID"; then
            echo "Stopping NVR Command Center (PID: $PID)..."
            kill "$PID" 2>/dev/null || true

            for _ in {1..10}; do
                if ! is_viewer_pid "$PID"; then
                    echo "NVR Command Center stopped gracefully"
                    rm -f "$PID_FILE"
                    return 0
                fi
                sleep 0.2
            done

            echo "Force killing NVR Command Center..."
            kill -9 "$PID" 2>/dev/null || true
            echo "NVR Command Center stopped (forced)"
        fi
        rm -f "$PID_FILE"
    else
        local pids
        pids="$(find_viewer_pids)"
        if [ -n "$pids" ]; then
            echo "Stopping NVR Command Center..."
            for p in $pids; do
                kill "$p" 2>/dev/null || true
            done
        else
            echo "NVR Command Center is not running"
        fi
    fi
}

toggle_viewer() {
    if is_running; then
        stop_viewer
    else
        start_viewer
    fi
}

print_help() {
    cat <<EOF
Usage: $0 {start|stop|toggle|status|restart|scan|help}

Commands:
  start    Build if needed and launch NVR Command Center in the background
  stop     Gracefully stop any running NVR Command Center instance
  toggle   Start if stopped, or stop if currently running (default)
  status   Show whether NVR Command Center is running and its PID
  restart  Restart NVR Command Center
  scan     Probe NVR channels 1..16 in parallel and print active channels
  help     Show this help message
EOF
}

case "${1:-toggle}" in
    start)
        if is_running; then
            echo "NVR Command Center is already running (PID: $(cat "$PID_FILE"))"
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
            echo "NVR Command Center is running (PID: $(cat "$PID_FILE"))"
        else
            echo "NVR Command Center is not running"
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
    help|-h|--help)
        print_help
        exit 0
        ;;
    *)
        print_help >&2
        exit 1
        ;;
esac
