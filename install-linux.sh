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
#   5. Installs a GNOME/KDE launcher entry
#      (~/.local/share/applications/io.openmouse.bridge.desktop) with the
#      bundled app icon, unless --no-launcher.
set -euo pipefail

REPO="OpenMouse-Project/OpenMouse-Bridge"
ASSET="openmouse-bridge-linux-x64-full.zip"
# Releases before 1.0.0 published the full package under this name;
# since then it is a small archive for older updaters, not a usable install.
LEGACY_ASSET="openmouse-bridge-linux-x64.zip"
RAW_BASE="https://raw.githubusercontent.com/${REPO}/main"
TAG="latest"
INSTALL_DIR=""
BIN_LINK_DIR=""
SYSTEM=false
WITH_DEPS=true
WITH_UDEV=true
WITH_AUTOSTART=true
WITH_LAUNCHER=true
START=false
UNINSTALL=false
ASSUME_YES=false

# The hidraw udev rule and its helper. They open every hidraw node of a USB
# device with a mouse interface, so a new brand needs no change here. Kept
# equal to RULE and HELPER in src/hid_access.rs (the panel's Fix permissions
# button; a test compares them).
UDEV_RULE="/etc/udev/rules.d/69-openmouse-bridge.rules"
UDEV_HELPER="/etc/udev/openmouse-bridge-is-mouse"

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
  --no-launcher    Skip the GNOME/KDE app-launcher entry
  --start          Launch Bridge after installing
  --uninstall      Remove a previous install (dir, symlink, autostart, launcher, udev rule)
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
        --no-launcher) WITH_LAUNCHER=false; shift ;;
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

# Writes the hidraw udev rule to $1. Where logind (or elogind) runs, which
# /run/systemd/seats shows, only the user at the seat gets access; without it
# nothing turns uaccess into an ACL, so the nodes are opened up instead.
write_udev_rule() {
    local access='MODE="0666"'
    [ -d /run/systemd/seats ] && access='TAG+="uaccess"'
    sed "s|@ACCESS@|$access|" >"$1" <<'OPENMOUSE_UDEV_RULE'
# OpenMouse Bridge: let the signed-in user reach mice over hidraw.
# Installed by OpenMouse Bridge or install-linux.sh; safe to delete on uninstall.
# Every hidraw node of a USB device with a mouse interface (a mouse or its
# receiver), including the vendor interface its settings go through.
SUBSYSTEM=="hidraw", SUBSYSTEMS=="usb", PROGRAM="/bin/sh /etc/udev/openmouse-bridge-is-mouse %S%p", @ACCESS@
# The button listener presses keys and side buttons through a virtual keyboard and mouse.
KERNEL=="uinput", MODE="0660", TAG+="uaccess", OPTIONS+="static_node=uinput"
OPENMOUSE_UDEV_RULE
}

# Writes the rule's helper, which decides whether a hidraw node is a mouse's, to $1.
write_udev_helper() {
    cat >"$1" <<'OPENMOUSE_UDEV_HELPER'
#!/bin/sh
# OpenMouse Bridge udev helper for /etc/udev/rules.d/69-openmouse-bridge.rules;
# safe to delete with it. Succeeds when the hidraw device at sysfs path $1
# belongs to a USB device with a mouse interface: a boot-protocol mouse, or a
# HID interface whose report descriptor has a Generic Desktop Mouse collection.
dev=$(readlink -f "$1/device") || exit 1
while [ ! -f "$dev/bNumInterfaces" ]; do
    [ "$dev" != / ] || exit 1
    dev=$(dirname "$dev")
done
interfaces="$dev/${dev##*/}:"
want=$(($(cat "$dev/bNumInterfaces")))

# Whether report descriptor $1 has a Generic Desktop Mouse application
# collection. Parses HID items, so a Report ID between the Usage Page and the
# Usage, or a 2-byte Usage Page, still counts. Short usages take the Usage
# Page in force at the main item; 4-byte usages carry their own.
is_mouse_descriptor() {
    set -- $(od -An -tu1 -v "$1")
    page=0
    usages=""
    while [ "$#" -gt 0 ]; do
        prefix=$1
        shift
        if [ "$prefix" -eq 254 ]; then
            [ "$#" -ge 2 ] || return 1
            skip=$(($1 + 2))
            [ "$#" -ge "$skip" ] || return 1
            shift "$skip"
            continue
        fi
        size=$((prefix & 3))
        [ "$size" -eq 3 ] && size=4
        [ "$#" -ge "$size" ] || return 1
        value=0
        byte=0
        while [ "$byte" -lt "$size" ]; do
            value=$((value | ($1 << (8 * byte))))
            shift
            byte=$((byte + 1))
        done
        kind=$(((prefix >> 2) & 3))
        case "$kind:$((prefix >> 4))" in
            1:0) page=$value ;;
            2:0)
                if [ "$size" -eq 4 ]; then usages="$usages $value"; else usages="$usages s$value"; fi
                ;;
            0:10)
                if [ "$value" -eq 1 ]; then
                    for usage in $usages; do
                        case "$usage" in s*) usage=$(((page << 16) | ${usage#s})) ;; esac
                        [ "$usage" -eq 65538 ] && return 0
                    done
                fi
                ;;
        esac
        [ "$kind" -eq 0 ] && usages=""
    done
    return 1
}

# On hotplug the interfaces, and the report descriptors under them, appear
# one by one: wait (up to a second) until every interface and every HID
# interface's descriptor is there.
complete() {
    count=0
    for interface in "$interfaces"*; do
        [ -e "$interface" ] || return 1
        count=$((count + 1))
        [ "$(cat "$interface/bInterfaceClass" 2>/dev/null)" = 03 ] || continue
        for descriptor in "$interface"/*/report_descriptor; do
            [ -e "$descriptor" ] || return 1
        done
    done
    [ "$count" -ge "$want" ]
}
tries=0
until complete || [ "$tries" -ge 10 ]; do
    tries=$((tries + 1))
    sleep 0.1
done

for interface in "$interfaces"*; do
    [ "$(cat "$interface/bInterfaceClass" 2>/dev/null)" = 03 ] || continue
    [ "$(cat "$interface/bInterfaceProtocol" 2>/dev/null)" = 02 ] && exit 0
    for descriptor in "$interface"/*/report_descriptor; do
        [ -r "$descriptor" ] && is_mouse_descriptor "$descriptor" && exit 0
    done
done
exit 1
OPENMOUSE_UDEV_HELPER
}

# Installs $1 at $2 as root: copied beside it, then moved over it, so a failed
# install never leaves a half-written file in place.
install_root_file() {
    local sudo="$3"
    $sudo install -m 0644 "$1" "$2.openmouse-new" && $sudo mv -f "$2.openmouse-new" "$2"
}

# Writes the XDG autostart entry Bridge itself uses (see src/platform/linux.rs).
desktop_exec() {
    local value="$1" character index
    [[ "$value" != *[=$'\001'-$'\037'$'\177']* ]] || return 1
    for ((index=0; index<${#value}; index++)); do
        character="${value:index:1}"
        case "$character" in
            '%') printf '%s' '%%' ;;
            '\') printf '%s' '\\\\' ;;
            '"'|'$'|'`') printf '%s%s' '\\' "$character" ;;
            *) printf '%s' "$character" ;;
        esac
    done
}

startup_config_home() {
    case "${XDG_CONFIG_HOME:-}" in
        /*) printf '%s' "$XDG_CONFIG_HOME" ;;
        *) printf '%s' "$HOME/.config" ;;
    esac
}

write_autostart() {
    local path="$1" exec
    exec=$(desktop_exec "$2") || return 1
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

# Writes the app-launcher entry (visible in GNOME/KDE launchers) next to the
# installed files. Bridge is a tray app: launching it a second time just
# opens another copy, so SingleMainWindow keeps the launcher from stacking
# duplicate icons while it runs.
write_launcher() {
    local path="$1" exec icon="$3"
    exec=$(desktop_exec "$2") || return 1
    [[ "$icon" != *[$'\001'-$'\037'$'\177']* ]] || return 1
    icon="${icon//\\/\\\\}"
    mkdir -p "$(dirname "$path")"
    cat >"$path" <<EOF
[Desktop Entry]
Type=Application
Version=1.0
Name=OpenMouse Bridge
Comment=System tray companion for the OpenMouse control panel
Exec="$exec"
Icon=$icon
Terminal=false
Categories=Settings;HardwareSettings;Utility;
Keywords=mouse;DPI;polling;gaming;OpenMouse;
SingleMainWindow=true
StartupWMClass=openmouse-bridge
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

# The HTTP status of a HEAD request for $1, empty when the server is unreachable.
http_status() {
    if command -v curl >/dev/null 2>&1; then
        curl -sIL -o /dev/null -w '%{http_code}' "$1" 2>/dev/null || true
    elif command -v wget >/dev/null 2>&1; then
        wget -S --spider "$1" 2>&1 | awk '/^ *HTTP\//{code=$2} END{print code}' || true
    fi
}

release_url() {
    if [ "$TAG" = "latest" ]; then
        echo "https://github.com/${REPO}/releases/latest/download/${ASSET}"
    else
        echo "https://github.com/${REPO}/releases/download/${TAG}/${ASSET}"
    fi
}

# Refreshes the launcher icon cache when present; harmless when missing
# (GNOME/KDE pick up the new .desktop file on next login regardless).
update_desktop_database() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$HOME/.local/share/applications" >/dev/null 2>&1 || true
    fi
    if command -v kbuildsycoca6 >/dev/null 2>&1; then
        kbuildsycoca6 >/dev/null 2>&1 || true
    elif command -v kbuildsycoca5 >/dev/null 2>&1; then
        kbuildsycoca5 >/dev/null 2>&1 || true
    fi
}

do_uninstall() {
    local dir="$1" link="$2" desktop="$3" rule="$4" sudo="$5" root="$6" launcher="$7"
    log "Removing ${dir}, ${link}, autostart entry, launcher entry, and udev rule (if present)..."
    $root rm -f "$link"
    $root rm -rf "$dir"
    rm -f "$desktop" "$launcher"
    # The helper is checked on its own: a rule deleted by hand leaves it behind.
    if [ -f "$rule" ] || [ -f "$UDEV_HELPER" ]; then
        if [ "$sudo" = "none" ]; then
            die "cannot remove $rule and $UDEV_HELPER without root; delete them manually"
        fi
        $sudo rm -f "$rule" "$UDEV_HELPER" /etc/modules-load.d/openmouse-bridge.conf
        $sudo udevadm control --reload-rules 2>/dev/null || true
    fi
    update_desktop_database
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
    local desktop="$(startup_config_home)/autostart/io.openmouse.bridge.desktop"
    # GNOME/KDE launcher: user-local always, so it never needs root and
    # --uninstall can remove it without sudo. Icon path is absolute because
    # launchers do not resolve relative to the .desktop location.
    local launcher="$HOME/.local/share/applications/io.openmouse.bridge.desktop"
    local icon="$INSTALL_DIR/openmouse-app-icon.png"
    local rule="$UDEV_RULE"
    local sudo
    sudo="$(sudo_cmd)"

    local root=""
    [ -w "$(dirname "$INSTALL_DIR")" ] || root="$sudo"
    if [ "$UNINSTALL" = true ]; then
        do_uninstall "$INSTALL_DIR" "$link" "$desktop" "$rule" "$sudo" "$root" "$launcher"
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
    # Expand $tmp now: it is local to main(), so a single-quoted trap would
    # hit "tmp: unbound variable" under `set -u` when it fires on EXIT.
    trap "rm -rf \"$tmp\"" EXIT
    log "Downloading OpenMouse Bridge ($TAG)..."
    if ! download "$(release_url)" "$tmp/$ASSET"; then
        # Only a release that has no -full asset falls back; any other failure
        # must not end in the legacy archive, which has no Node.js runtime.
        if [ "$(http_status "$(release_url)")" = "404" ]; then
            ASSET="$LEGACY_ASSET"
            download "$(release_url)" "$tmp/$ASSET"
        else
            die "could not download $ASSET; check your connection and try again"
        fi
    fi
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
    # ~/.local/bin may be root-owned from an older install that used sudo
    # for it; reclaim with sudo and hand ownership back so reruns work.
    # (The old `[ ! -w dir ] && [ ! -L link ]` test took the non-sudo path
    # whenever the link already existed, failing on a root-owned dir.)
    local link_sudo
    link_sudo=""
    if { [ -e "$BIN_LINK_DIR" ] && [ ! -w "$BIN_LINK_DIR" ]; } || { [ ! -e "$BIN_LINK_DIR" ] && [ ! -w "$(dirname "$BIN_LINK_DIR")" ]; }; then
        [ "$sudo" = "none" ] && die "cannot write to $BIN_LINK_DIR; rerun with sudo"
        link_sudo="$sudo"
    fi
    if [ -n "$link_sudo" ]; then
        # shellcheck disable=SC2086
        $link_sudo mkdir -p "$BIN_LINK_DIR"
        # shellcheck disable=SC2086
        $link_sudo ln -sf "$binary" "$link"
    else
        mkdir -p "$BIN_LINK_DIR"
        ln -sf "$binary" "$link"
    fi
    case "$BIN_LINK_DIR" in
        "$HOME"/*)
            if [ -n "$link_sudo" ]; then
                # shellcheck disable=SC2086
                $link_sudo chown -R "$(id -u):$(id -g)" "$BIN_LINK_DIR" 2>/dev/null || true
            fi
            ;;
    esac
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
            # The helper goes first so the rule never runs without it.
            write_udev_helper "$tmp/openmouse-bridge-is-mouse"
            write_udev_rule "$tmp/69-openmouse-bridge.rules"
            install_root_file "$tmp/openmouse-bridge-is-mouse" "$UDEV_HELPER" "$sudo" || die "could not install $UDEV_HELPER"
            install_root_file "$tmp/69-openmouse-bridge.rules" "$rule" "$sudo" || die "could not install $rule"
            # /dev/uinput only exists once the uinput module is loaded.
            # Best effort: button assignments need it, the rest of Bridge does not.
            if ! { $sudo mkdir -p /etc/modules-load.d && echo uinput | $sudo tee /etc/modules-load.d/openmouse-bridge.conf >/dev/null; }; then
                warn "could not configure uinput to load at boot; button assignments need /dev/uinput"
            fi
            $sudo modprobe uinput 2>/dev/null || true
            $sudo udevadm control --reload-rules 2>/dev/null || true
            $sudo udevadm trigger --subsystem-match=hidraw --action=change 2>/dev/null || true
            $sudo udevadm trigger --subsystem-match=misc --sysname-match=uinput --action=change 2>/dev/null || true
            log "Unplug and replug the mouse (or reboot) so the new rule applies."
        fi
    else
        log "Skipping udev rule (--no-udev)."
    fi

    # 6. Autostart.
    if [ "$WITH_AUTOSTART" = true ]; then
        write_autostart "$desktop" "$binary" || die "could not write autostart entry: invalid path or write failure"
        log "Login autostart enabled."
    fi

    # 7. App launcher entry + icon refresh. The zip is expected to ship
    # openmouse-app-icon.png next to the binary (see release.yml Package).
    if [ "$WITH_LAUNCHER" = true ]; then
        write_launcher "$launcher" "$binary" "$icon" || die "could not write launcher entry: invalid path or write failure"
        update_desktop_database
        log "Launcher entry installed."
    else
        log "Skipping launcher entry (--no-launcher)."
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
