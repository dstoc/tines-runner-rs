#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 4 ]]; then
  echo "usage: package-release.sh <version> <target> <binary> <output-directory>" >&2
  exit 2
fi

version=$1
target=$2
binary=$3
output_directory=$4

case "$version" in
  ''|*[!0-9A-Za-z.+-]*)
    echo "invalid release version: $version" >&2
    exit 2
    ;;
esac

case "$target" in
  ''|*[!0-9A-Za-z_-]*)
    echo "invalid release target: $target" >&2
    exit 2
    ;;
esac

if [[ ! -x "$binary" ]]; then
  echo "release binary is missing or not executable: $binary" >&2
  exit 1
fi

expected_version="tines-runner-rs $version"
actual_version=$("$binary" --version)
if [[ "$actual_version" != "$expected_version" ]]; then
  echo "expected '$expected_version' from --version, got '$actual_version'" >&2
  exit 1
fi

binary_directory=$(cd -- "$(dirname -- "$binary")" && pwd -P)
binary_name=$(basename -- "$binary")
mkdir -p -- "$output_directory"
output_directory=$(cd -- "$output_directory" && pwd -P)

archive_name="tines-runner-rs-v${version}-${target}.tar.gz"
archive_path="$output_directory/$archive_name"

tar --sort=name --mtime='@0' --owner=0 --group=0 --numeric-owner \
  --format=ustar -cf - -C "$binary_directory" "$binary_name" \
  | gzip --no-name > "$archive_path"

(
  cd -- "$output_directory"
  sha256sum "$archive_name" > "$archive_name.sha256"
)

printf '%s\n' "$archive_path" "$archive_path.sha256"
