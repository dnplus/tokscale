#!/bin/sh
set -eu

fail() { printf 'Error: %s\n' "$*" >&2; exit 1; }
os=$(uname -s)
arch=$(uname -m)
case "$arch" in
    arm64|aarch64) arch=aarch64 ;;
    x86_64|amd64) arch=x86_64 ;;
    *) fail "Unsupported architecture: $arch. See https://github.com/dnplus/tokscale/releases" ;;
esac
case "$os" in
    Darwin) target=$arch-apple-darwin ;;
    Linux) target=$arch-unknown-linux-gnu ;;
    MINGW*|MSYS*|CYGWIN*|Windows*) fail 'Windows: download the .zip from https://github.com/dnplus/tokscale/releases' ;;
    *) fail "Unsupported OS: $os. See https://github.com/dnplus/tokscale/releases" ;;
esac
version=${TOKSCALE_VERSION:-}
case "$version" in
    *[!A-Za-z0-9._-]*) fail 'TOKSCALE_VERSION must be a release tag (letters, digits, dots, underscores or hyphens).' ;;
esac
install_dir=${TOKSCALE_INSTALL_DIR:-"$HOME/.local/bin"}
# Resolve latest only during a real install: dry runs make no network requests.
base=https://github.com/dnplus/tokscale/releases
if [ -z "$version" ] && [ "${TOKSCALE_DRY_RUN:-0}" != 1 ]; then
    command -v curl >/dev/null 2>&1 || fail 'curl is required.'
    latest=$(curl -fsSL -o /dev/null -w '%{url_effective}' "$base/latest")
    version=${latest##*/}
    case "$latest" in
        "$base/tag/"*) ;;
        *) fail 'Could not resolve the latest release.' ;;
    esac
fi
if [ -n "$version" ]; then
    download=$base/download/$version
else
    version='<latest-tag>'
    download=$base/latest/download
fi
archive=tokscale-$version-$target.tar.gz
printf 'Platform: %s\nArchive: %s/%s\nChecksum: %s/%s.sha256\nInstall: %s/tokscale\n' \
    "$target" "$download" "$archive" "$download" "$archive" "$install_dir"
[ "${TOKSCALE_DRY_RUN:-0}" != 1 ] || exit 0
command -v curl >/dev/null 2>&1 || fail 'curl is required.'
command -v tar >/dev/null 2>&1 || fail 'tar is required.'
if command -v sha256sum >/dev/null 2>&1; then
    checksum=sha256sum
elif command -v shasum >/dev/null 2>&1; then
    checksum=shasum
else
    fail 'sha256sum or shasum is required.'
fi
tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' 0
trap 'exit 1' HUP INT TERM
curl -fsSL "$download/$archive" -o "$tmp_dir/$archive"
curl -fsSL "$download/$archive.sha256" -o "$tmp_dir/$archive.sha256"
# Read the digest for this archive; never trust paths in the sidecar.
expected=$(awk 'NR == 1 {print $1}' "$tmp_dir/$archive.sha256")
[ "${#expected}" -eq 64 ] || fail 'Invalid SHA256 checksum.'
case "$expected" in *[!0-9a-fA-F]*) fail 'Invalid SHA256 checksum.' ;; esac
(
    cd "$tmp_dir"
    printf '%s  %s\n' "$expected" "$archive" > verified.sha256
    if [ "$checksum" = sha256sum ]; then
        sha256sum -c verified.sha256
    else
        shasum -a 256 -c verified.sha256
    fi
) || fail 'Checksum verification failed.'
mkdir "$tmp_dir/extracted"
tar -xzf "$tmp_dir/$archive" -C "$tmp_dir/extracted"
[ -f "$tmp_dir/extracted/tokscale" ] || fail 'Archive does not contain tokscale.'
mkdir -p "$install_dir"
cp "$tmp_dir/extracted/tokscale" "$install_dir/tokscale"
chmod +x "$install_dir/tokscale"
printf 'Installed tokscale to %s/tokscale\n' "$install_dir"
case ":${PATH:-}:" in
    *":$install_dir:"*) ;;
    *) printf 'Add %s to your PATH to run tokscale.\n' "$install_dir" ;;
esac
