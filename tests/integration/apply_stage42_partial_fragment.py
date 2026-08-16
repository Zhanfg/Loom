from pathlib import Path

p = Path('crates/loom-erofs/src/compact_core.rs')
s = p.read_text()

old = '''        if fragment_size == 0 || fragment_size % u64::from(BLOCK_SIZE) != 0 {
            return Err(CoreError::UnsupportedInode(
                "Stage 39 requires an aligned non-empty fragment tail",
            ));
        }'''
new = '''        if fragment_size == 0 {
            return Err(CoreError::UnsupportedInode(
                "fragment tail must be non-empty",
            ));
        }'''
assert old in s
s = s.replace(old, new, 1)

old = '''        let packed_eof =
            validate_full_eof_plain_sentinel(&packed_entries, packed_lclusters, packed.size)?;
        if packed_eof.is_some() {
            return Err(CoreError::UnsupportedInode(
                "Stage 39 packed inode must be logical-cluster aligned",
            ));
        }
        let packed_heads = recover_full_data_heads(&packed_entries, packed_lclusters, None)?;'''
new = '''        let packed_eof =
            validate_full_eof_plain_sentinel(&packed_entries, packed_lclusters, packed.size)?;
        let packed_heads =
            recover_full_data_heads(&packed_entries, packed_lclusters, packed_eof)?;'''
assert old in s
s = s.replace(old, new, 1)

old = '''        validate_full_nonheads(&packed_entries, &packed_heads, packed_lclusters, None)?;'''
new = '''        validate_full_nonheads(
            &packed_entries,
            &packed_heads,
            packed_lclusters,
            packed_eof,
        )?;'''
assert old in s
s = s.replace(old, new, 1)

p.write_text(s)
