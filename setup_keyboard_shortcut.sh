#!/bin/bash
# Automatic keyboard shortcut setup for GNOME/Ubuntu

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LAUNCH_SCRIPT="$SCRIPT_DIR/launch_viewer.sh"

echo "=========================================="
echo "RTSP Camera Viewer - Keyboard Shortcut Setup"
echo "=========================================="
echo ""

# Check if running GNOME
if [ "$XDG_CURRENT_DESKTOP" = "GNOME" ] || [ "$XDG_CURRENT_DESKTOP" = "ubuntu:GNOME" ]; then
    echo "✓ Detected GNOME desktop environment"
    echo ""
    
    # Make launch script executable
    chmod +x "$LAUNCH_SCRIPT"
    echo "✓ Made launch script executable"
    echo ""
    
    # Ask for keyboard shortcut
    echo "Choose a keyboard shortcut:"
    echo "  1) Ctrl+Alt+C (Recommended)"
    echo "  2) Ctrl+Alt+V"
    echo "  3) Super+C"
    echo "  4) F12"
    echo "  5) Custom"
    echo ""
    read -p "Enter choice [1-5]: " choice
    
    case $choice in
        1)
            BINDING="<Primary><Alt>c"
            DISPLAY="Ctrl+Alt+C"
            ;;
        2)
            BINDING="<Primary><Alt>v"
            DISPLAY="Ctrl+Alt+V"
            ;;
        3)
            BINDING="<Super>c"
            DISPLAY="Super+C"
            ;;
        4)
            BINDING="F12"
            DISPLAY="F12"
            ;;
        5)
            echo ""
            echo "Enter custom binding (GNOME format):"
            echo "Examples:"
            echo "  <Primary><Alt>c  = Ctrl+Alt+C"
            echo "  <Super>v         = Super+V"
            echo "  <Shift><Alt>c    = Shift+Alt+C"
            echo ""
            read -p "Binding: " BINDING
            DISPLAY="$BINDING"
            ;;
        *)
            echo "Invalid choice, using default: Ctrl+Alt+C"
            BINDING="<Primary><Alt>c"
            DISPLAY="Ctrl+Alt+C"
            ;;
    esac
    
    echo ""
    echo "Setting up keyboard shortcut: $DISPLAY"
    echo ""
    
    # Get existing custom keybindings
    EXISTING=$(gsettings get org.gnome.settings-daemon.plugins.media-keys custom-keybindings)
    
    # Find next available slot
    SLOT=0
    while echo "$EXISTING" | grep -q "custom$SLOT"; do
        SLOT=$((SLOT + 1))
    done
    
    CUSTOM_PATH="/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom$SLOT/"
    
    # Add new custom keybinding to the list
    if [ "$EXISTING" = "@as []" ]; then
        # No existing custom keybindings
        NEW_LIST="['$CUSTOM_PATH']"
    else
        # Append to existing list
        NEW_LIST=$(echo "$EXISTING" | sed "s/]$/, '$CUSTOM_PATH']/")
    fi
    
    # Set the custom keybindings list
    gsettings set org.gnome.settings-daemon.plugins.media-keys custom-keybindings "$NEW_LIST"
    
    # Set the name
    gsettings set org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:$CUSTOM_PATH name 'RTSP Camera Viewer'
    
    # Set the command
    gsettings set org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:$CUSTOM_PATH command "$LAUNCH_SCRIPT toggle"
    
    # Set the keyboard shortcut
    gsettings set org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:$CUSTOM_PATH binding "$BINDING"
    
    echo "✓ Keyboard shortcut configured successfully!"
    echo ""
    echo "=========================================="
    echo "Setup Complete!"
    echo "=========================================="
    echo ""
    echo "Keyboard Shortcut: $DISPLAY"
    echo "Action: Toggle RTSP Camera Viewer"
    echo ""
    echo "Press $DISPLAY to start the viewer"
    echo "Press $DISPLAY again to stop it"
    echo ""
    echo "You can also use:"
    echo "  ./launch_viewer.sh start   - Start viewer"
    echo "  ./launch_viewer.sh stop    - Stop viewer"
    echo "  ./launch_viewer.sh status  - Check status"
    echo ""
    
elif [ "$XDG_CURRENT_DESKTOP" = "KDE" ]; then
    echo "✗ KDE Plasma detected"
    echo ""
    echo "Please set up keyboard shortcut manually:"
    echo "1. Open System Settings"
    echo "2. Go to Shortcuts → Custom Shortcuts"
    echo "3. Add new shortcut with command:"
    echo "   $LAUNCH_SCRIPT toggle"
    echo ""
    echo "See KEYBOARD_SHORTCUT_SETUP.md for detailed instructions"
    
elif [ "$XDG_CURRENT_DESKTOP" = "XFCE" ]; then
    echo "✗ XFCE detected"
    echo ""
    echo "Please set up keyboard shortcut manually:"
    echo "1. Open Settings → Keyboard → Application Shortcuts"
    echo "2. Add new shortcut with command:"
    echo "   $LAUNCH_SCRIPT toggle"
    echo ""
    echo "See KEYBOARD_SHORTCUT_SETUP.md for detailed instructions"
    
else
    echo "✗ Unknown desktop environment: $XDG_CURRENT_DESKTOP"
    echo ""
    echo "Please set up keyboard shortcut manually"
    echo "See KEYBOARD_SHORTCUT_SETUP.md for instructions"
    echo ""
    echo "Launch script location:"
    echo "  $LAUNCH_SCRIPT toggle"
fi

echo ""
echo "For more information, see:"
echo "  - KEYBOARD_SHORTCUT_SETUP.md (detailed guide)"
echo "  - ./launch_viewer.sh --help (script usage)"
echo ""
