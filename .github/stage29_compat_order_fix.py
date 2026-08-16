from pathlib import Path

path = Path('crates/loom-erofs/src/compact_core.rs')
s = path.read_text()
old = '''    if origin.advise != replacement.advise {
        return Err(CoreError::IncompatibleReplacement(
            "compressed map advice differs",
        ));
    }
    if origin.placement != replacement.placement {
        return Err(CoreError::IncompatibleReplacement(
            "LZ4 physical placement mode differs",
        ));
    }
'''
new = '''    if origin.placement != replacement.placement {
        return Err(CoreError::IncompatibleReplacement(
            "LZ4 physical placement mode differs",
        ));
    }
    if origin.advise != replacement.advise {
        return Err(CoreError::IncompatibleReplacement(
            "compressed map advice differs",
        ));
    }
'''
assert s.count(old) == 1, s.count(old)
path.write_text(s.replace(old, new))
