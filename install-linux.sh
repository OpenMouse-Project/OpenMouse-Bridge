#!/usr/bin/env bash
#
# OpenMouse Bridge Linux installer.
#
#   curl -fsSL https://openmouse.app/bridge/download/install-linux.sh | bash
#   (short alias: https://openmouse.app/install-linux.sh)
#
# Flags go after `--`:
#
#   curl -fsSL .../install-linux.sh | bash -s -- --yes
#
# What it does:
#   1. Detects the distro package manager and checks for the runtime
#      libraries Bridge needs (hidapi/libudev, GTK tray, X11/GL, unzip).
#      Missing packages are installed with sudo (or printed with --no-deps).
#   2. Downloads the latest (or --tag) linux-x64 release zip, verifies its
#      SHA-256 checksum, and extracts it to the install dir.
#   3. Installs a udev rule so the mouse's hidraw nodes are user-accessible
#      (without this Bridge can only open the mouse as root).
#   4. Registers login autostart via ~/.config/autostart (same entry Bridge
#      itself writes), unless --no-autostart.
#
set -euo pipefail

REPO="OpenMouse-Project/OpenMouse-Bridge"
ASSET="openmouse-bridge-linux-x64.zip"
RAW_BASE="https://raw.githubusercontent.com/${REPO}/main"
TAG="latest"
INSTALL_DIR=""
BIN_LINK_DIR=""
SYSTEM=false
WITH_DEPS=true
WITH_UDEV=true
WITH_AUTOSTART=true
START=false
UNINSTALL=false
ASSUME_YES=false

# Every HID vendor id Bridge drivers can talk to: native Rust drivers
# (Pulsar 0x3710 in src/drivers/pulsar.rs, Delux 0x1d57/0x248a/0x373e in
# src/drivers/delux.rs) plus each brand in native-hid/src/brands.mjs.
# Keep in sync when a brand is added there.
VENDOR_IDS="045e 046d 0483 093a 1532 1915 1caa 1d57 248a 25a7 2fe3 31e3 3151 3367 3434 3554 361d 36a7 3710 373b 373e 3879"

log() { printf '==> %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

usage() {
    cat <<USAGE
Usage: install-linux.sh [options]

Options:
  --tag TAG        Install release TAG instead of the latest (e.g. v1.0.0)
  --dir DIR        Install files to DIR (default: ~/.local/share/openmouse-bridge)
  --system         System-wide install to /opt/openmouse-bridge with a
                   /usr/local/bin symlink (needs root/sudo)
  --no-deps        Do not install missing system packages, just report them
  --no-udev        Skip the hidraw udev rule (Bridge then needs root to reach mice)
  --no-autostart   Skip login autostart registration
  --start          Launch Bridge after installing
  --uninstall      Remove a previous install (dir, symlink, autostart, udev rule)
  --yes            Assume yes for package installs
  -h, --help       Show this help
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --tag) TAG="${2:?--tag needs a value}"; shift 2 ;;
        --dir) INSTALL_DIR="${2:?--dir needs a value}"; shift 2 ;;
        --system) SYSTEM=true; shift ;;
        --no-deps) WITH_DEPS=false; shift ;;
        --no-udev) WITH_UDEV=false; shift ;;
        --no-autostart) WITH_AUTOSTART=false; shift ;;
        --start) START=true; shift ;;
        --uninstall) UNINSTALL=true; shift ;;
        --yes) ASSUME_YES=true; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option: $1 (see --help)" ;;
    esac
done

# Sudo helper: empty when root, "sudo" when available, otherwise "none".
sudo_cmd() {
    if [ "$(id -u)" -eq 0 ]; then
        echo ""
    elif command -v sudo >/dev/null 2>&1; then
        echo "sudo"
    else
        echo "none"
    fi
}

# Prints the distro package manager: apt, dnf, pacman, zypper, apk, or unknown.
detect_pm() {
    if command -v apt-get >/dev/null 2>&1; then echo "apt"; return; fi
    if command -v dnf >/dev/null 2>&1; then echo "dnf"; return; fi
    if command -v pacman >/dev/null 2>&1; then echo "pacman"; return; fi
    if command -v zypper >/dev/null 2>&1; then echo "zypper"; return; fi
    if command -v apk >/dev/null 2>&1; then echo "apk"; return; fi
    echo "unknown"
}

# Prints the runtime packages Bridge needs on the given manager. These are
# the distro names for: libudev (hidapi), GTK3 + AppIndicator (tray icon),
# XCB/XKB/GL (tray window + overlay banner), unzip, and a TLS downloader.
pm_packages() {
    case "$1" in
        apt) echo "libudev1 libgtk-3-0 libayatana-appindicator3-1 libxcb-shape0 libxcb-xfixes0 libxcb-render0 libxkbcommon0 libgl1 unzip curl ca-certificates" ;;
        dnf) echo "systemd-libs gtk3 libappindicator-gtk3 libxcb libxkbcommon mesa-libGL dbus-libs unzip curl ca-certificates" ;;
        pacman) echo "systemd-libs gtk3 libappindicator-gtk3 libxcb libxkbcommon mesa dbus unzip curl ca-certificates" ;;
        zypper) echo "libudev1 gtk3 libappindicator3-1 libxcb1 libxkbcommon0 Mesa-libGL1 dbus-1 unzip curl ca-certificates" ;;
        apk) echo "eudev-libs gtk+3.0 libxcb libxkbcommon mesa-gl dbus unzip curl ca-certificates" ;;
        *) return 1 ;;
    esac
}

# pkg_installed <pm> <pkg>: 0 when the package is already installed.
pkg_installed() {
    case "$1" in
        apt) dpkg-query -W -f='${Status}' "$2" 2>/dev/null | grep -q "install ok installed" ;;
        dnf|zypper) rpm -q "$2" >/dev/null 2>&1 ;;
        pacman) pacman -Qq "$2" >/dev/null 2>&1 ;;
        apk) apk info -e "$2" >/dev/null 2>&1 ;;
        *) return 1 ;;
    esac
}

# Prints the subset of the manager's package list that is not installed.
missing_pkgs() {
    local pm="$1" pkg
    for pkg in $(pm_packages "$pm"); do
        if ! pkg_installed "$pm" "$pkg"; then
            echo "$pkg"
        fi
    done
}

install_pkgs() {
    local pm="$1"
    shift
    local sudo="$1"
    shift
    local yes_flags=""
    if [ "$ASSUME_YES" = true ]; then
        case "$pm" in
            apt) yes_flags="-y" ;;
            dnf) yes_flags="-y" ;;
            pacman) yes_flags="--noconfirm" ;;
            zypper) yes_flags="--non-interactive" ;;
            apk) yes_flags="" ;;
        esac
    fi
    case "$pm" in
        apt) $sudo apt-get update && $sudo apt-get install $yes_flags "$@" ;;
        dnf) $sudo dnf install $yes_flags "$@" ;;
        pacman) $sudo pacman -S $yes_flags "$@" ;;
        zypper) $sudo zypper install $yes_flags "$@" ;;
        apk) $sudo apk add "$@" ;;
    esac
}

# Reports shared libraries the Bridge binary needs but the loader cannot
# find (e.g. "libgtk-3.so.0 => not found"). Empty output means all resolved.
missing_libs() {
    local binary="$1"
    if ! command -v ldd >/dev/null 2>&1; then
        return 0
    fi
    ldd "$binary" 2>/dev/null | awk '/not found/ { print $1 }' | sort -u || true
}

# Warns about optional pieces package installs cannot cover.
check_optionals() {
    if ! command -v paplay >/dev/null 2>&1 && ! command -v aplay >/dev/null 2>&1; then
        warn "no audio player found (paplay or aplay): the notification chime stays silent (try alsa-utils)"
    fi
    if [ -z "${DISPLAY:-}" ] && [ -z "${WAYLAND_DISPLAY:-}" ]; then
        warn "no DISPLAY or WAYLAND_DISPLAY: Bridge installs fine, but the tray panel needs a desktop session"
    fi
    if [ -n "${WAYLAND_DISPLAY:-}" ] && [ -z "${DISPLAY:-}" ]; then
        warn "Wayland-only session: foreground-app detection and the overlay banner need X11 (banner falls back to a system notification)"
    fi
}

# Writes the hidraw udev rule granting user access to known mouse vendors.
write_udev_rule() {
    local path="$1" vid
    {
        echo "# OpenMouse Bridge: let the signed-in user reach gaming mice over hidraw."
        echo "# Installed by install-linux.sh; safe to delete on uninstall."
        for vid in $VENDOR_IDS; do
            printf 'SUBSYSTEM=="hidraw", ATTRS{idVendor}=="%s", MODE="0666", TAG+="uaccess"\n' "$vid"
        done
    } >"$path"
}

# Writes the XDG autostart entry Bridge itself uses (see src/platform/linux.rs).
write_autostart() {
    local path="$1" exec="$2"
    mkdir -p "$(dirname "$path")"
    cat >"$path" <<EOF
[Desktop Entry]
Type=Application
Name=OpenMouse Bridge
Exec="$exec"
Hidden=false
X-GNOME-Autostart-enabled=true
NoDisplay=true
EOF
}

download() {
    local url="$1" dest="$2"
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$url" -o "$dest"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$dest" "$url"
    else
        die "need curl or wget to download the release"
    fi
}

release_url() {
    if [ "$TAG" = "latest" ]; then
        echo "https://github.com/${REPO}/releases/latest/download/${ASSET}"
    else
        echo "https://github.com/${REPO}/releases/download/${TAG}/${ASSET}"
    fi
}

do_uninstall() {
    local dir="$1" link="$2" desktop="$3" rule="$4" sudo="$5" root="$6"
    log "Removing ${dir}, ${link}, autostart entry, and udev rule (if present)..."
    $root rm -f "$link"
    $root rm -rf "$dir"
    rm -f "$desktop"
    if [ -f "$rule" ]; then
        if [ "$sudo" = "none" ]; then
            die "cannot remove $rule without root; delete it manually"
        fi
        $sudo rm -f "$rule"
        $sudo udevadm control --reload-rules 2>/dev/null || true
    fi
    log "Uninstalled."
}

main() {
    local arch
    arch="$(uname -m)"
    [ "$arch" = "x86_64" ] || die "unsupported architecture: $arch (only x86_64 Linux builds are published)"

    local data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
    if [ -z "$INSTALL_DIR" ]; then
        if [ "$SYSTEM" = true ]; then INSTALL_DIR="/opt/openmouse-bridge"; else INSTALL_DIR="$data_home/openmouse-bridge"; fi
    fi
    if [ -z "$BIN_LINK_DIR" ]; then
        if [ "$SYSTEM" = true ]; then BIN_LINK_DIR="/usr/local/bin"; else BIN_LINK_DIR="$HOME/.local/bin"; fi
    fi
    local binary="$INSTALL_DIR/openmouse-bridge"
    local link="$BIN_LINK_DIR/openmouse-bridge"
    local desktop="$HOME/.config/autostart/io.openmouse.bridge.desktop"
    local rule="/etc/udev/rules.d/69-openmouse-bridge.rules"
    local sudo
    sudo="$(sudo_cmd)"

    local root=""
    [ -w "$(dirname "$INSTALL_DIR")" ] || root="$sudo"
    if [ "$UNINSTALL" = true ]; then
        do_uninstall "$INSTALL_DIR" "$link" "$desktop" "$rule" "$sudo" "$root"
        return 0
    fi

    # 1. System dependencies.
    local pm
    pm="$(detect_pm)"
    if [ "$WITH_DEPS" = true ]; then
        if [ "$pm" = "unknown" ]; then
            die "no supported package manager found (apt, dnf, pacman, zypper, apk); rerun with --no-deps and install the README's library list by hand"
        fi
        local missing
        missing="$(missing_pkgs "$pm")"
        if [ -n "$missing" ]; then
            log "Missing system packages: $missing"
            if [ "$sudo" = "none" ]; then
                # shellcheck disable=SC2086
                die "cannot install without root; rerun as root/with sudo, or with --no-deps to skip"
            fi
            # shellcheck disable=SC2086
            install_pkgs "$pm" "$sudo" $missing
        else
            log "All system packages already installed."
        fi
    else
        log "Skipping system package checks (--no-deps)."
    fi

    # 2. Download + verify.
    local tmp
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    log "Downloading OpenMouse Bridge ($TAG)..."
    download "$(release_url)" "$tmp/$ASSET"
    download "$(release_url).sha256" "$tmp/$ASSET.sha256"
    (
        cd "$tmp"
        if command -v sha256sum >/dev/null 2>&1; then
            sha256sum -c "$ASSET.sha256"
        else
            shasum -a 256 -c "$ASSET.sha256"
        fi
    )

    # 3. Extract.
    command -v unzip >/dev/null 2>&1 || die "unzip is required to extract the release (install it, then rerun)"
    log "Installing to $INSTALL_DIR..."
    if [ ! -w "$(dirname "$INSTALL_DIR")" ]; then
        [ "$sudo" = "none" ] && die "cannot write to $(dirname "$INSTALL_DIR"); rerun with sudo or pick --dir"
        $sudo mkdir -p "$INSTALL_DIR"
        $sudo unzip -oq "$tmp/$ASSET" -d "$INSTALL_DIR"
        $sudo chmod +x "$binary"
    else
        mkdir -p "$INSTALL_DIR"
        unzip -oq "$tmp/$ASSET" -d "$INSTALL_DIR"
        chmod +x "$binary"
    fi
    if [ ! -w "$BIN_LINK_DIR" ] && [ ! -L "$link" ]; then
        [ "$sudo" = "none" ] && die "cannot write to $BIN_LINK_DIR; rerun with sudo"
        $sudo mkdir -p "$BIN_LINK_DIR"
        $sudo ln -sf "$binary" "$link"
    else
        mkdir -p "$BIN_LINK_DIR"
        ln -sf "$binary" "$link"
    fi
    log "Binary ready: $link"

    # 4. Confirm the loader resolves everything (catches distro renames
    # the static package list missed).
    local missing_lib
    missing_lib="$(missing_libs "$binary")"
    if [ -n "$missing_lib" ]; then
        warn "the loader still cannot find: $missing_lib"
        warn "install the distro packages providing them, then rerun (or run with --no-deps to skip this)"
    fi

    # 5. udev rule for hidraw access.
    if [ "$WITH_UDEV" = true ]; then
        if [ "$sudo" = "none" ]; then
            warn "skipping udev rule without root; Bridge will need root to reach mice. Rerun with sudo or install $rule by hand."
        else
            log "Installing hidraw udev rule..."
            write_udev_rule "$tmp/69-openmouse-bridge.rules"
            $sudo cp "$tmp/69-openmouse-bridge.rules" "$rule"
            $sudo udevadm control --reload-rules 2>/dev/null || true
            $sudo udevadm trigger --subsystem-match=hidraw --action=change 2>/dev/null || true
            log "Unplug and replug the mouse (or reboot) so the new rule applies."
        fi
    else
        log "Skipping udev rule (--no-udev)."
    fi

    # 6. Autostart.
    if [ "$WITH_AUTOSTART" = true ]; then
        write_autostart "$desktop" "$binary"
        log "Login autostart enabled."
    fi

    check_optionals

    if [ "$START" = true ]; then
        log "Starting Bridge..."
        nohup "$binary" >/dev/null 2>&1 &
        disown || true
    fi

    log "Done. Open https://control.openmouse.app to connect."
    case ":$PATH:" in
        *":$BIN_LINK_DIR:"*) ;;
        *) warn "$BIN_LINK_DIR is not on PATH; add it or run $binary directly" ;;
    esac
}

# Sourcing the file (e.g. in tests) must not run the installer.
if [ -z "${OPENMOUSE_INSTALL_SOURCED:-}" ]; then
    main "$@"
fi
