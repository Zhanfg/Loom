from pathlib import Path

path = Path('crates/loom-erofs/src/compact_core.rs')
s = path.read_text()

old = '''            algorithm: 0,
            advise: ADVISE_COMPACTED_2B,
            logical_lclusters: 2,'''
new = '''            algorithm: 0,
            advise: ADVISE_COMPACTED_2B,
            placement: Lz4Placement::ZeroPadding,
            logical_lclusters: 2,'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''        assert!(encode_extent(0, &good).is_ok());'''
new = '''        assert!(encode_extent(0, &good, Lz4Placement::ZeroPadding).is_ok());'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''            encode_extent(8, &bad),'''
new = '''            encode_extent(8, &bad, Lz4Placement::ZeroPadding),'''
assert s.count(old) == 1
s = s.replace(old, new)

path.write_text(s)
