set -eu
export OPENMOUSE_INSTALL_SOURCED=1
fixture_parent="$1"
set --
source ./install-linux.sh
root=$(mktemp -d "$fixture_parent/bridge-startup.XXXXXXXX")
trap 'rm -rf "$root"' EXIT
executable='/opt/name-value/50%/A "$`B\bridge'
expected='Exec="/opt/name-value/50%%/A \\"\\$\\`B\\\\bridge"'
write_autostart "$root/autostart.desktop" "$executable"
write_launcher "$root/launcher.desktop" "$executable" '/opt/icon.png'
for entry in "$root/autostart.desktop" "$root/launcher.desktop"; do
    actual=$(while IFS= read -r line; do case "$line" in Exec=*) printf '%s' "$line";; esac; done < "$entry")
    [ "$actual" = "$expected" ] || { printf 'Incorrect Exec entry: %s\n' "$actual" >&2; exit 1; }
done
printf '%s' sentinel > "$root/equals.desktop"
if write_autostart "$root/equals.desktop" '/opt/name=value/bridge'; then
    printf '%s\n' 'Autostart accepted an equals executable path' >&2
    exit 1
fi
[ "$(cat "$root/equals.desktop")" = sentinel ]
if write_launcher "$root/equals.desktop" '/opt/name=value/bridge' '/opt/icon.png'; then
    printf '%s\n' 'Launcher accepted an equals executable path' >&2
    exit 1
fi
[ "$(cat "$root/equals.desktop")" = sentinel ]
if write_autostart "$root/invalid.desktop" $'/opt/invalid\nentry'; then exit 1; fi
if write_launcher "$root/invalid.desktop" '/opt/bridge' $'/opt/invalid\nicon'; then exit 1; fi
[ "$(HOME=/home/fixture XDG_CONFIG_HOME=/xdg/config startup_config_home)" = /xdg/config ]
[ "$(HOME=/home/fixture XDG_CONFIG_HOME=relative startup_config_home)" = /home/fixture/.config ]
for code in {1..31} 127; do
    printf -v octal '%03o' "$code"
    printf -v control '%b' "\\$octal"
    printf '%s' sentinel > "$root/invalid.desktop"
    if write_autostart "$root/invalid.desktop" "/opt/a${control}b"; then exit 1; fi
    if write_launcher "$root/invalid.desktop" "/opt/a${control}b" '/opt/icon.png'; then exit 1; fi
    if write_launcher "$root/invalid.desktop" '/opt/bridge' "/opt/a${control}b"; then exit 1; fi
    [ "$(cat "$root/invalid.desktop")" = sentinel ]
done
for fixture_case in valid autostart launcher equals-autostart equals-launcher; do
    call_root="$root/$fixture_case"
    mkdir -p "$call_root"
    set +e
    (
        set -e
        export HOME="$call_root/home" XDG_CONFIG_HOME="$call_root/config"
        case "$fixture_case" in
            valid) INSTALL_DIR="$call_root/name-value" ;;
            equals-*) INSTALL_DIR="$call_root/name=value" ;;
            *) INSTALL_DIR="$call_root/"$'invalid\tpath' ;;
        esac
        BIN_LINK_DIR="$call_root/bin"
        WITH_DEPS=false
        WITH_UDEV=false
        case "$fixture_case" in *launcher) WITH_AUTOSTART=false ;; esac
        uname() { printf '%s\n' x86_64; }
        sudo_cmd() { printf '%s\n' none; }
        detect_pm() { printf '%s\n' unknown; }
        download() { :; }
        sha256sum() { :; }
        shasum() { :; }
        unzip() { :; }
        chmod() { :; }
        ln() { :; }
        missing_libs() { :; }
        update_desktop_database() { :; }
        check_optionals() { :; }
        mktemp() { command mktemp -d "$call_root/download.XXXXXXXX"; }
        main
    ) > "$call_root/output" 2>&1
    status=$?
    set -e
    if [ "$fixture_case" = valid ]; then
        [ "$status" -eq 0 ]
        [ -f "$call_root/config/autostart/io.openmouse.bridge.desktop" ]
        [ -f "$call_root/home/.local/share/applications/io.openmouse.bridge.desktop" ]
    else
        [ "$status" -ne 0 ]
        grep -q "error: could not write ${fixture_case#equals-} entry" "$call_root/output" || { cat "$call_root/output"; exit 1; }
        if grep -q 'Done.' "$call_root/output"; then exit 1; fi
    fi
done
printf '%s\n' 'PASS: valid escaping, equals rejection with output preservation, all representable ASCII controls, and both inert production callsites'
