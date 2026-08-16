from pathlib import Path

p = Path('crates/loom-erofs/src/compact_core.rs')
s = p.read_text()

old = '''        let pcluster = if fragment_offset == 0 {
            self.recover_stage39_packed_pcluster(packed_nid, fragment_size)?
        } else {
            self.recover_stage40_shared_packed_pcluster(packed_nid, fragment_offset, fragment_size)?
        };'''
new = '''        let packed_size = self.read_inode(packed_nid)?.size;
        let pcluster = if fragment_offset == 0 && packed_size == fragment_size {
            self.recover_stage39_packed_pcluster(packed_nid, fragment_size)?
        } else {
            self.recover_stage40_shared_packed_pcluster(packed_nid, fragment_offset, fragment_size)?
        };'''
assert old in s
s = s.replace(old, new, 1)

old = '''        if fragment_offset == 0
            || fragment_offset % block != 0
            || fragment_size == 0
            || fragment_size % block != 0
        {
            return Err(CoreError::UnsupportedInode(
                "Stage 40 requires a non-zero block-aligned shared fragment extent",
            ));
        }'''
new = '''        if fragment_offset % block != 0 || fragment_size == 0 || fragment_size % block != 0 {
            return Err(CoreError::UnsupportedInode(
                "shared fragment support requires a block-aligned non-empty extent",
            ));
        }'''
assert old in s
s = s.replace(old, new, 1)

p.write_text(s)
