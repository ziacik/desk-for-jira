#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
prefix="${PREFIX:-$HOME/.local}"
id="com.github.ziacik.DeskForJira"

cd "$root"
cargo build --release

install -Dm755 target/release/desk-for-jira "$prefix/bin/desk-for-jira"
install -Dm644 "packaging/$id.desktop" "$prefix/share/applications/$id.desktop"
install -Dm644 "packaging/$id.svg" "$prefix/share/icons/hicolor/scalable/apps/$id.svg"
install -Dm644 "packaging/$id-symbolic.svg" "$prefix/share/icons/hicolor/symbolic/apps/$id-symbolic.svg"
install -Dm644 "packaging/$id.metainfo.xml" "$prefix/share/metainfo/$id.metainfo.xml"

echo "Installed Desk for Jira to $prefix"
echo "Ensure $prefix/bin is on your PATH."
