#!/usr/bin/env bash
set -euo pipefail

repository_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
temporary_directory=$(mktemp -d)
trap 'rm -rf -- "$temporary_directory"' EXIT

cat > "$temporary_directory/tines-runner-rs" <<'STUB'
#!/usr/bin/env bash
if [[ ${1-} == --version ]]; then
  printf 'tines-runner-rs %s\n' "${PACKAGE_VERSION:?}"
else
  exit 1
fi
STUB
chmod +x "$temporary_directory/tines-runner-rs"

PACKAGE_VERSION=0.1.0 bash "$repository_root/scripts/package-release.sh" \
  0.1.0 x86_64-unknown-linux-gnu "$temporary_directory/tines-runner-rs" \
  "$temporary_directory/first"
PACKAGE_VERSION=0.1.0 bash "$repository_root/scripts/package-release.sh" \
  0.1.0 x86_64-unknown-linux-gnu "$temporary_directory/tines-runner-rs" \
  "$temporary_directory/second"

archive_name=tines-runner-rs-v0.1.0-x86_64-unknown-linux-gnu.tar.gz
cmp "$temporary_directory/first/$archive_name" \
  "$temporary_directory/second/$archive_name"
cmp "$temporary_directory/first/$archive_name.sha256" \
  "$temporary_directory/second/$archive_name.sha256"
(
  cd "$temporary_directory/first"
  sha256sum --check "$archive_name.sha256"
)

mkdir "$temporary_directory/extracted"
tar -xzf "$temporary_directory/first/$archive_name" \
  -C "$temporary_directory/extracted"
extracted_version=$(PACKAGE_VERSION=0.1.0 \
  "$temporary_directory/extracted/tines-runner-rs" --version)
[[ "$extracted_version" == 'tines-runner-rs 0.1.0' ]]

if PACKAGE_VERSION=9.9.9 bash "$repository_root/scripts/package-release.sh" \
  0.1.0 x86_64-unknown-linux-gnu "$temporary_directory/tines-runner-rs" \
  "$temporary_directory/wrong-version"; then
  echo 'packaging accepted a binary with the wrong version' >&2
  exit 1
fi

echo 'release packaging checks passed'
