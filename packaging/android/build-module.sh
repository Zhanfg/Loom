#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 6 ]]; then
  echo "usage: $0 <loom-binary> <output-zip> <version-name> <version-code> <source-ref> <source-sha>" >&2
  exit 2
fi

BINARY="$1"
OUTPUT="$2"
VERSION_NAME="$3"
VERSION_CODE="$4"
SOURCE_REF="$5"
SOURCE_SHA="$6"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEMPLATE="$ROOT/module"

[[ -f "$BINARY" ]] || { echo "missing Loom binary: $BINARY" >&2; exit 1; }
[[ -f "$TEMPLATE/module.prop.in" ]] || { echo "missing Android module template" >&2; exit 1; }
[[ "$VERSION_CODE" =~ ^[0-9]+$ ]] || { echo "version code must be numeric" >&2; exit 1; }

TMP="$(mktemp -d)"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

STAGE="$TMP/module"
mkdir -p "$STAGE/bin"
cp -a "$TEMPLATE/." "$STAGE/"
rm -f "$STAGE/module.prop.in"
install -m 0755 "$BINARY" "$STAGE/bin/loom"
touch "$STAGE/skip_mount"

sed \
  -e "s|@VERSION_NAME@|$VERSION_NAME|g" \
  -e "s|@VERSION_CODE@|$VERSION_CODE|g" \
  -e "s|@SOURCE_SHA@|$SOURCE_SHA|g" \
  "$TEMPLATE/module.prop.in" > "$STAGE/module.prop"

cat > "$STAGE/build-info.txt" <<EOF
project=Loom
package_kind=android-kernelsu-magisk-module
source_ref=$SOURCE_REF
source_sha=$SOURCE_SHA
version=$VERSION_NAME
versionCode=$VERSION_CODE
architecture=arm64-v8a
runtime_activation=fail-closed-packaging-alpha
EOF

chmod 0644 "$STAGE/module.prop" "$STAGE/build-info.txt" "$STAGE/skip_mount"
for script in customize.sh post-fs-data.sh service.sh action.sh uninstall.sh; do
  [[ -f "$STAGE/$script" ]] || { echo "missing module script: $script" >&2; exit 1; }
  chmod 0755 "$STAGE/$script"
done

mkdir -p "$(dirname "$OUTPUT")"
OUTPUT_ABS="$(cd "$(dirname "$OUTPUT")" && pwd)/$(basename "$OUTPUT")"
rm -f "$OUTPUT_ABS"
(
  cd "$STAGE"
  # -X strips host-specific extra attributes; the archive root is the module root.
  zip -X -9 -r "$OUTPUT_ABS" . >/dev/null
)

unzip -t "$OUTPUT_ABS" >/dev/null
printf 'Android module package: %s\n' "$OUTPUT_ABS"
printf 'Source: %s (%s)\n' "$SOURCE_REF" "$SOURCE_SHA"
printf 'Version: %s (%s)\n' "$VERSION_NAME" "$VERSION_CODE"
