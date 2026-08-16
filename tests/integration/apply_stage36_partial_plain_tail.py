from pathlib import Path

path = Path('crates/loom-erofs/src/compact_core.rs')
text = path.read_text()
old = '''        if inode.size % u64::from(BLOCK_SIZE) != 0 && eof_plain_clusterofs.is_none() {
            return Err(CoreError::UnsupportedInode(
                "Stage 34 full big-pcluster partial EOF requires the verified zero-block PLAIN sentinel",
            ));
        }
'''
assert old in text
path.write_text(text.replace(old, '', 1))
