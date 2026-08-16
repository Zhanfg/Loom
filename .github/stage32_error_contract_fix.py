from pathlib import Path

core = Path('crates/loom-erofs/src/compact_core.rs')
s = core.read_text()
old = 'partial full-index PLAIN tail is neither a zero-block EOF sentinel nor an aligned raw data head'
new = 'partial full-index file lacks the expected zero-block PLAIN EOF sentinel'
assert s.count(old) == 1, s.count(old)
core.write_text(s.replace(old, new))

proof = Path('tests/integration/stage32_erofs_legacy_full_plain_partial_tail.sh')
p = proof.read_text()
assert p.count(old) == 1, p.count(old)
proof.write_text(p.replace(old, new))
