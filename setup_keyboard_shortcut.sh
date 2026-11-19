#!/bin/bash
# Automatic keyboard shortcut setup for GNOME/Ubuntu

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LAUNCH_SCRIPT="$SCRIPT_DIR/launch_viewer.sh"

echo "=========================================="
echo "NVR Command Center - Keyboard Shortcut Setup"
echo "=========================================="
echo ""

DESKTOP_ENV="${XDG_CURRENT_DESKTOP:-}"

# Check if running GNOME (GNOME, ubuntu:GNOME, pop:GNOME, GNOME-Classic:GNOME, etc.)
if [[ "$DESKTOP_ENV" == *GNOME* ]]; then
    echo "[OK] Detected GNOME desktop environment ($DESKTOP_ENV)"
    echo ""

    # Make launch script executable
    chmod +x "$LAUNCH_SCRIPT"
    echo "[OK] Made launch script executable"
    echo ""

    # Ask for keyboard shortcut
    echo "Choose a keyboard shortcut:"
    echo "  1) Ctrl+Alt+C (Recommended)"
    echo "  2) Ctrl+Alt+V"
    echo "  3) Super+C"
    echo "  4) F12"
    echo "  5) Custom"
    echo ""
    read -r -p "Enter choice [1-5]: " choice

    case $choice in
        1)
            BINDING="<Primary><Alt>c"
            SHORTCUT_LABEL="Ctrl+Alt+C"
            ;;
        2)
            BINDING="<Primary><Alt>v"
            SHORTCUT_LABEL="Ctrl+Alt+V"
            ;;
        3)
            BINDING="<Super>c"
            SHORTCUT_LABEL="Super+C"
            ;;
        4)
            BINDING="F12"
            SHORTCUT_LABEL="F12"
            ;;
        5)
            echo ""
            echo "Enter custom binding (GNOME format):"
            echo "Examples:"
            echo "  <Primary><Alt>c  = Ctrl+Alt+C"
            echo "  <Super>v         = Super+V"
            echo "  <Shift><Alt>c    = Shift+Alt+C"
            echo ""
            read -r -p "Binding: " BINDING
            SHORTCUT_LABEL="$BINDING"
            ;;
        *)
            echo "Invalid choice, using default: Ctrl+Alt+C"
            BINDING="<Primary><Alt>c"
            SHORTCUT_LABEL="Ctrl+Alt+C"
            ;;
    esac

    echo ""
    echo "Setting up keyboard shortcut: $SHORTCUT_LABEL"
    echo ""

    # Get existing custom keybindings
    EXISTING=$(gsettings get org.gnome.settings-daemon.plugins.media-keys custom-keybindings)

    # Reuse existing 'NVR Command Center' custom keybinding slot if already registered
    CUSTOM_PATH=""
    while IFS= read -r candidate; do
        [ -n "$candidate" ] || continue
        existing_name=$(gsettings get "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:$candidate" name 2>/dev/null || true)
        case "$existing_name" in
            "'NVR Command Center'"|"\"NVR Command Center\""|"'RTSP Camera Viewer'"|"\"RTSP Camera Viewer\"")
                CUSTOM_PATH="$candidate"
                break
                ;;
        esac
    done < <(echo "$EXISTING" | grep -oE "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/[^'\"]+" || true)

    if [ -z "$CUSTOM_PATH" ]; then
        # Find next available slot
        SLOT=0
        while echo "$EXISTING" | grep -q "custom$SLOT/"; do
            SLOT=$((SLOT + 1))
        done

        CUSTOM_PATH="/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom$SLOT/"

        # Add new custom keybinding to the list
        if [ "$EXISTING" = "@as []" ] || [ -z "$EXISTING" ]; then
            NEW_LIST="['$CUSTOM_PATH']"
        else
            NEW_LIST=$(echo "$EXISTING" | sed "s|]$|, '$CUSTOM_PATH']|")
        fi

        gsettings set org.gnome.settings-daemon.plugins.media-keys custom-keybindings "$NEW_LIST"
    fi

    # Set the name, command, and keyboard shortcut
    gsettings set "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:$CUSTOM_PATH" name 'NVR Command Center'
    gsettings set "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:$CUSTOM_PATH" command "\"$LAUNCH_SCRIPT\" toggle"
    gsettings set "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:$CUSTOM_PATH" binding "$BINDING"

    echo "[OK] Keyboard shortcut configured successfully!"
    echo ""
    echo "=========================================="
    echo "Setup Complete!"
    echo "=========================================="
    echo ""
    echo "Keyboard Shortcut: $SHORTCUT_LABEL"
    echo "Action: Toggle NVR Command Center"
    echo ""
    echo "Press $SHORTCUT_LABEL to start the viewer"
    echo "Press $SHORTCUT_LABEL again to stop it"
    echo ""
    echo "You can also use:"
    echo "  ./launch_viewer.sh start   - Start viewer"
    echo "  ./launch_viewer.sh stop    - Stop viewer"
    echo "  ./launch_viewer.sh status  - Check status"
    echo ""

elif [[ "$DESKTOP_ENV" == *KDE* ]]; then
    echo "KDE Plasma detected"
    echo ""
    echo "Please set up keyboard shortcut manually:"
    echo "1. Open System Settings"
    echo "2. Go to Shortcuts -> Custom Shortcuts"
    echo "3. Add new shortcut with command:"
    echo "   \"$LAUNCH_SCRIPT\" toggle"
    echo ""
    echo "See README.md for detailed instructions"

elif [[ "$DESKTOP_ENV" == *XFCE* ]]; then
    echo "XFCE detected"
    echo ""
    echo "Please set up keyboard shortcut manually:"
    echo "1. Open Settings -> Keyboard -> Application Shortcuts"
    echo "2. Add new shortcut with command:"
    echo "   \"$LAUNCH_SCRIPT\" toggle"
    echo ""
    echo "See README.md for detailed instructions"

else
    echo "Desktop environment: ${DESKTOP_ENV:-unknown}"
    echo ""
    echo "Please set up keyboard shortcut manually."
    echo "See README.md for instructions."
    echo ""
    echo "Launch script command:"
    echo "  \"$LAUNCH_SCRIPT\" toggle"
fi

echo ""
echo "For more information, see:"
echo "  - README.md (setup & usage guide)"
echo "  - ./launch_viewer.sh help (script usage)"
echo ""
