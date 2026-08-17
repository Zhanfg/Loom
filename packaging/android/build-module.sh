#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 9 ]]; then
  echo "usage: $0 <loom-binary> <loom-flatten-binary> <loom-early-map-binary> <loom-fiemap-binary> <output-zip> <version-name> <version-code> <source-ref> <source-sha>" >&2
  exit 2
fi

BINARY="$1"
FLATTEN_BINARY="$2"
EARLY_MAP_BINARY="$3"
FIEMAP_BINARY="$4"
OUTPUT="$5"
VERSION_NAME="$6"
VERSION_CODE="$7"
SOURCE_REF="$8"
SOURCE_SHA="$9"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEMPLATE="$ROOT/module"

for pair in \
  "loom:$BINARY" \
  "loom-flatten:$FLATTEN_BINARY" \
  "loom-early-map:$EARLY_MAP_BINARY" \
  "loom-fiemap:$FIEMAP_BINARY"; do
  name=${pair%%:*}
  path=${pair#*:}
  [[ -f "$path" ]] || { echo "missing binary: $name ($path)" >&2; exit 1; }
done
[[ -f "$TEMPLATE/module.prop.in" ]] || { echo "missing Android module template" >&2; exit 1; }
for runtime in loom-sidecar loom-shadow loom-shadow-commit loom-compose loom-early-prepare; do
  [[ -f "$TEMPLATE/bin/$runtime" ]] || { echo "missing Android runtime: $runtime" >&2; exit 1; }
done
for config in sidecar.conf shadow.conf compose.conf early.conf; do
  [[ -f "$TEMPLATE/$config" ]] || { echo "missing Android configuration: $config" >&2; exit 1; }
done
[[ "$VERSION_CODE" =~ ^[0-9]+$ ]] || { echo "version code must be numeric" >&2; exit 1; }

TMP="$(mktemp -d)"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

STAGE="$TMP/module"
mkdir -p "$STAGE/bin"
cp -a "$TEMPLATE/." "$STAGE/"
rm -f "$STAGE/module.prop.in"

# The Alpha 2/3 layered runtime remains the transient compiler executor. Public
# loom-shadow is the Alpha 4 collision-safe single-DM commit wrapper.
mv "$STAGE/bin/loom-shadow" "$STAGE/bin/loom-shadow-layered"
mv "$STAGE/bin/loom-shadow-commit" "$STAGE/bin/loom-shadow"
install -m 0755 "$BINARY" "$STAGE/bin/loom"
install -m 0755 "$FLATTEN_BINARY" "$STAGE/bin/loom-flatten"
install -m 0755 "$EARLY_MAP_BINARY" "$STAGE/bin/loom-early-map"
install -m 0755 "$FIEMAP_BINARY" "$STAGE/bin/loom-fiemap"
touch "$STAGE/skip_mount"

sed \
  -e "s|@VERSION_NAME@|$VERSION_NAME|g" \
  -e "s|@VERSION_CODE@|$VERSION_CODE|g" \
  -e "s|@SOURCE_SHA@|$SOURCE_SHA|g" \
  "$TEMPLATE/module.prop.in" >"$STAGE/module.prop"

cat >"$STAGE/build-info.txt" <<EOF
project=Loom
package_kind=android-kernelsu-magisk-module
source_ref=$SOURCE_REF
source_sha=$SOURCE_SHA
version=$VERSION_NAME
versionCode=$VERSION_CODE
architecture=arm64-v8a
runtime_activation=alpha5-early-snapshot-prepare
composition_scope=enabled-ordinary-module-system-trees
composition_order=lexical-last-wins
filesystem_fabric=origin-plus-aggregate-sparse-shadow
shadow_origin=direct-block-device
shadow_backing=single-readonly-loop
compile_composition=transactional-layered-dm-linear
stable_composition=flattened-single-dm-linear
stable_dm_depth=1
early_snapshot_storage=metadata-ext4-raw-extents
early_snapshot_default=disabled
early_snapshot_state=prepared-not-active
early_shadow_loop_required=false
generation_commit=boot-completed
interrupted_boot_policy=recovery-hold
mount_scope=/data/adb/loom/mnt
uses_overlayfs=false
uses_magic_mount=false
stock_mount_replacement=false
first_stage_takeover=false
system_takeover=false
EOF

chmod 0644 "$STAGE/module.prop" "$STAGE/build-info.txt" "$STAGE/skip_mount" \
  "$STAGE/sidecar.conf" "$STAGE/shadow.conf" "$STAGE/compose.conf" "$STAGE/early.conf"
chmod 0755 \
  "$STAGE/bin/loom" \
  "$STAGE/bin/loom-flatten" \
  "$STAGE/bin/loom-early-map" \
  "$STAGE/bin/loom-fiemap" \
  "$STAGE/bin/loom-sidecar" \
  "$STAGE/bin/loom-shadow" \
  "$STAGE/bin/loom-shadow-layered" \
  "$STAGE/bin/loom-compose" \
  "$STAGE/bin/loom-early-prepare"
for script in customize.sh post-fs-data.sh service.sh boot-completed.sh action.sh uninstall.sh; do
  [[ -f "$STAGE/$script" ]] || { echo "missing module script: $script" >&2; exit 1; }
  chmod 0755 "$STAGE/$script"
done

mkdir -p "$(dirname "$OUTPUT")"
OUTPUT_ABS="$(cd "$(dirname "$OUTPUT")" && pwd)/$(basename "$OUTPUT")"
rm -f "$OUTPUT_ABS"
(
  cd "$STAGE"
  zip -X -9 -r "$OUTPUT_ABS" . >/dev/null
)

unzip -t "$OUTPUT_ABS" >/dev/null
printf 'Android module package: %s\n' "$OUTPUT_ABS"
printf 'Source: %s (%s)\n' "$SOURCE_REF" "$SOURCE_SHA"
printf 'Version: %s (%s)\n' "$VERSION_NAME" "$VERSION_CODE"
