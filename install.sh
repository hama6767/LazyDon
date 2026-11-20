#!/bin/sh
# Install the latest supported release to a user-writable directory.
set -eu

fail() { printf 'LazyDon: %s\n' "$*" >&2; exit 1; }

case "$(uname -s):$(uname -m)" in
    Linux:x86_64) archive=lazydon-linux-x86_64.tar.gz ;;
    Linux:aarch64|Linux:arm64) archive=lazydon-linux-aarch64.tar.gz ;;
    Darwin:arm64|Darwin:aarch64) archive=lazydon-macos-aarch64.tar.gz ;;
    *) fail 'Supported platforms: Linux x86_64 / ARM64 and macOS Apple Silicon.' ;;
esac
for command in curl tar awk mktemp install; do
    command -v "$command" >/dev/null 2>&1 || fail "$command is required."
done
if command -v sha256sum >/dev/null 2>&1; then
    checksum=sha256sum
elif command -v shasum >/dev/null 2>&1; then
    checksum=shasum
else
    fail 'sha256sum or shasum is required.'
fi
[ -n "${HOME:-}" ] || fail 'HOME is not set.'

destination=${LAZYDON_INSTALL_DIR:-"$HOME/.local/bin"}
# Absolute paths avoid interpreting a user-provided relative path as an option
# on BSD utilities, which do not all support the GNU-style `--` separator.
case "$destination" in /*) ;; *) destination="$PWD/$destination" ;; esac
base=https://github.com/hama6767/LazyDon/releases/latest/download
temp_parent=${TMPDIR:-/tmp}
case "$temp_parent" in /*) ;; *) temp_parent="$PWD/$temp_parent" ;; esac
tmp=$(mktemp -d "$temp_parent/lazydon-install.XXXXXXXX")
new_binary=
cleanup() {
    rm -rf "$tmp"
    if [ -n "$new_binary" ]; then rm -f "$new_binary"; fi
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

printf 'Downloading the latest LazyDon release...\n'
download() {
    file=$1
    attempt=1
    while [ "$attempt" -le 3 ]; do
        if curl -fsSL --retry 2 --connect-timeout 15 "$base/$file" -o "$tmp/$file"; then
            return
        fi
        if [ "$attempt" -lt 3 ]; then sleep 2; fi
        attempt=$((attempt + 1))
    done
    fail "Could not download $file. Release assets may still be processing; please try again in a few minutes."
}
download "$archive"
download "$archive.sha256"

# Accept exactly one checksum for the expected filename; never pass an
# untrusted filename from the downloaded checksum file to sha256sum.
digest=$(awk -v name="$archive" '
    NF == 2 && $2 == name && length($1) == 64 && $1 ~ /^[0-9a-fA-F]+$/ { value=$1; count++ }
    END { if (NR == 1 && count == 1) print value; else exit 1 }
' "$tmp/$archive.sha256") || fail 'Invalid checksum file.'
if [ "$checksum" = sha256sum ]; then
    (cd "$tmp" && printf '%s  %s\n' "$digest" "$archive" | sha256sum -c -) ||
        fail 'Checksum verification failed.'
else
    (cd "$tmp" && printf '%s  %s\n' "$digest" "$archive" | shasum -a 256 -c -) ||
        fail 'Checksum verification failed.'
fi

# Extract only the expected binary, so no archive path can escape the temp dir.
tar -xOzf "$tmp/$archive" ./lazydon > "$tmp/lazydon" || fail 'Could not extract the executable.'
[ -s "$tmp/lazydon" ] || fail 'The executable is empty.'
mkdir -p "$destination"
[ ! -d "$destination/lazydon" ] || fail 'The destination lazydon is a directory.'
new_binary=$(mktemp "$destination/.lazydon.XXXXXXXX")
install -m 755 "$tmp/lazydon" "$new_binary"
mv -f "$new_binary" "$destination/lazydon"
new_binary=
printf 'Installed: %s/lazydon\n' "$destination"
case ":$PATH:" in
    *":$destination:"*) printf 'Run lazydon to start.\n' ;;
    *) printf 'Add the directory to PATH: export PATH="%s:$PATH"\n' "$destination" ;;
esac
