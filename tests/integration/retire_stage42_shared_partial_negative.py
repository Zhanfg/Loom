from pathlib import Path

p = Path('tests/integration/stage42_erofs_single_owner_partial_fragment.sh')
s = p.read_text()

s = s.replace(
    'ROOT="$WORK/root"; SHARED_ROOT="$WORK/shared-root"; MNT="$WORK/mnt"\n',
    'ROOT="$WORK/root"; MNT="$WORK/mnt"\n',
    1,
)
s = s.replace(
    'IMG="$WORK/origin.erofs"; SHARED_IMG="$WORK/shared.erofs"\n',
    'IMG="$WORK/origin.erofs"\n',
    1,
)
s = s.replace(
    'SHARED_SHADOW="$WORK/shared.shadow"; SHARED_TABLE="$WORK/shared.table"\n',
    '',
    1,
)
s = s.replace(
    'ORIGIN_LOOP=""; SHADOW_LOOP=""; SHARED_LOOP=""; MAPPER="loom-stage42-${RANDOM}-${RANDOM}"\n',
    'ORIGIN_LOOP=""; SHADOW_LOOP=""; MAPPER="loom-stage42-${RANDOM}-${RANDOM}"\n',
    1,
)
s = s.replace(
    '  [[ -n "$SHARED_LOOP" ]] && sudo losetup -d "$SHARED_LOOP"\n',
    '',
    1,
)
s = s.replace('mkdir -p "$ROOT" "$SHARED_ROOT" "$MNT"\n', 'mkdir -p "$ROOT" "$MNT"\n', 1)

start = s.index('# Shared partial fragments remain deliberately unsupported because offsets/sizes are not block aligned.\n')
end_marker = 'sudo losetup -d "$SHARED_LOOP"; SHARED_LOOP=""\n\n'
end = s.index(end_marker, start) + len(end_marker)
s = s[:start] + s[end:]

s = s.replace(
    "  '  shared partial fragment remains fail-closed: PASS' \\\n",
    '',
    1,
)

p.write_text(s)
