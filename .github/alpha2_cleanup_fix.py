from pathlib import Path

runtime = Path('packaging/android/module/bin/loom-shadow')
text = runtime.read_text()
old = '''  if ! dm_create_from_table "$dm_name" "$table"; then\n    "$LOSETUP" -d "$loop" >/dev/null 2>&1 || true\n    return 1\n  fi\n'''
new = '''  if ! dm_create_from_table "$dm_name" "$table"; then\n    dm_delete_name "$dm_name"\n    "$LOSETUP" -d "$loop" >/dev/null 2>&1 || true\n    return 1\n  fi\n'''
if old not in text:
    raise SystemExit('runtime cleanup anchor not found')
runtime.write_text(text.replace(old, new, 1))

test = Path('packaging/android/tests/shadow_runtime_test.sh')
text = test.read_text()
old = '''unset FAKE_LOOM_FAIL_TARGET\n\nsed -i 's/^LOOM_TAKEOVER=0$/LOOM_TAKEOVER=1/' "$STATE/shadow.conf"\n'''
new = '''unset FAKE_LOOM_FAIL_TARGET\n\n# A dm object created successfully but lacking a usable getpath must be deleted\n# immediately; its loop must also be detached.\n: > "$LOG_DM"; : > "$LOG_LOOP"; : > "$TMP/proc_mounts"\nexport FAKE_DM_GETPATH_FAIL=1\nif bash "$RUNTIME" activate; then\n  echo 'expected dm getpath failure to abort activation' >&2\n  exit 1\nfi\n[[ "$(cat "$STATE/status")" == SHADOW_COMPILE_FAILED ]]\n[[ ! -d "$STATE/shadow-runtime" ]]\n[[ ! -s "$TMP/proc_mounts" ]]\ngrep -Fq 'delete loom-shadow-test-1' "$LOG_DM"\ngrep -q '^detach ' "$LOG_LOOP"\nunset FAKE_DM_GETPATH_FAIL\n\nsed -i 's/^LOOM_TAKEOVER=0$/LOOM_TAKEOVER=1/' "$STATE/shadow.conf"\n'''
if old not in text:
    raise SystemExit('test scenario anchor not found')
text = text.replace(old, new, 1)
old = '''  getpath)\n    cat "$state/$2.path"\n    ;;\n'''
new = '''  getpath)\n    if [[ "${FAKE_DM_GETPATH_FAIL:-0}" == 1 ]]; then\n      exit 1\n    fi\n    cat "$state/$2.path"\n    ;;\n'''
if old not in text:
    raise SystemExit('fake dm getpath anchor not found')
test.write_text(text.replace(old, new, 1))
