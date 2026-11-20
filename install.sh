#!/bin/sh
# Install the latest Linux release to a user-writable directory.
set -eu

fail() { printf 'LazyDon: %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = Linux ] && [ "$(uname -m)" = x86_64 ] ||
    fail 'Linux x86_64 のみ対応しています。'
for command in curl tar sha256sum awk mktemp install; do
    command -v "$command" >/dev/null 2>&1 || fail "$command が必要です。"
done
[ -n "${HOME:-}" ] || fail 'HOME が設定されていません。'

destination=${LAZYDON_INSTALL_DIR:-"$HOME/.local/bin"}
archive=lazydon-linux-x86_64.tar.gz
base=https://github.com/hama6767/LazyDon/releases/latest/download
tmp=$(mktemp -d "${TMPDIR:-/tmp}/lazydon-install.XXXXXXXX")
new_binary=
cleanup() {
    rm -rf -- "$tmp"
    if [ -n "$new_binary" ]; then rm -f -- "$new_binary"; fi
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

printf 'LazyDon の最新リリースをダウンロードしています...\n'
curl -fsSL --retry 3 --connect-timeout 15 "$base/$archive" -o "$tmp/$archive"
curl -fsSL --retry 3 --connect-timeout 15 "$base/$archive.sha256" -o "$tmp/$archive.sha256"

# Accept exactly one checksum for the expected filename; never pass an
# untrusted filename from the downloaded checksum file to sha256sum.
digest=$(awk -v name="$archive" '
    NF == 2 && $2 == name && length($1) == 64 && $1 ~ /^[0-9a-fA-F]+$/ { value=$1; count++ }
    END { if (NR == 1 && count == 1) print value; else exit 1 }
' "$tmp/$archive.sha256") || fail 'チェックサムファイルが不正です。'
(cd "$tmp" && printf '%s  %s\n' "$digest" "$archive" | sha256sum -c -) ||
    fail 'チェックサムが一致しません。'

# Extract only the expected binary, so no archive path can escape the temp dir.
tar -xOzf "$tmp/$archive" ./lazydon > "$tmp/lazydon" || fail '実行ファイルを展開できません。'
[ -s "$tmp/lazydon" ] || fail '実行ファイルが空です。'
mkdir -p -- "$destination"
new_binary=$(mktemp "$destination/.lazydon.XXXXXXXX")
install -m 755 -- "$tmp/lazydon" "$new_binary"
mv -f -- "$new_binary" "$destination/lazydon"
new_binary=
printf 'インストールしました: %s/lazydon\n' "$destination"
case ":$PATH:" in
    *":$destination:"*) printf 'lazydon で起動できます。\n' ;;
    *) printf 'PATH に追加してから起動してください: export PATH="%s:$PATH"\n' "$destination" ;;
esac
