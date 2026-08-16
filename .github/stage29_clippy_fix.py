from pathlib import Path

path = Path('crates/loom-erofs/src/compact_core.rs')
s = path.read_text()

old = '''        for (index, head) in heads.iter().enumerate() {
            let end = heads
                .get(index + 1)
                .map(|next| next.lcn)
                .unwrap_or(logical_lclusters);
            if end <= head.lcn {
                return Err(CoreError::InvalidFilesystem(
                    "full-index HEAD lclusters are not strictly increasing",
                ));
            }
            for lcn in head.lcn + 1..end {
                let entry = entries
                    .get(lcn)
                    .ok_or(CoreError::UnexpectedEndOfStructure)?;
                if entry.kind != LCLUSTER_NONHEAD {
                    return Err(CoreError::InvalidFilesystem(
                        "full-index logical extent contains a non-NONHEAD interior entry",
                    ));
                }
                let delta0 = usize::from((entry.word & 0xffff) as u16);
                let delta1 = usize::from((entry.word >> 16) as u16);
                let expected0 = lcn
                    .checked_sub(head.lcn)
                    .ok_or(CoreError::ArithmeticOverflow)?;
                let expected1 = end.checked_sub(lcn).ok_or(CoreError::ArithmeticOverflow)?;
                if delta0 != expected0 || delta1 != expected1 {
                    return Err(CoreError::InvalidFilesystem(
                    "full-index NONHEAD forward/backward deltas disagree with recovered HEAD topology",
                ));
                }
            }
        }
'''
new = '''        validate_full_nonheads(&entries, &heads, logical_lclusters)?;
'''
assert s.count(old) == 1, s.count(old)
s = s.replace(old, new)

anchor = '''fn validate_target_inode(inode: &Inode) -> Result<usize, CoreError> {'''
helper = '''fn validate_full_nonheads(
    entries: &[FullEntry],
    heads: &[Head],
    logical_lclusters: usize,
) -> Result<(), CoreError> {
    for (index, head) in heads.iter().enumerate() {
        let end = heads
            .get(index + 1)
            .map_or(logical_lclusters, |next| next.lcn);
        if end <= head.lcn {
            return Err(CoreError::InvalidFilesystem(
                "full-index HEAD lclusters are not strictly increasing",
            ));
        }
        for lcn in head.lcn + 1..end {
            let entry = entries
                .get(lcn)
                .ok_or(CoreError::UnexpectedEndOfStructure)?;
            if entry.kind != LCLUSTER_NONHEAD {
                return Err(CoreError::InvalidFilesystem(
                    "full-index logical extent contains a non-NONHEAD interior entry",
                ));
            }
            let delta0 = usize::from((entry.word & 0xffff) as u16);
            let delta1 = usize::from((entry.word >> 16) as u16);
            let expected0 = lcn
                .checked_sub(head.lcn)
                .ok_or(CoreError::ArithmeticOverflow)?;
            let expected1 = end.checked_sub(lcn).ok_or(CoreError::ArithmeticOverflow)?;
            if delta0 != expected0 || delta1 != expected1 {
                return Err(CoreError::InvalidFilesystem(
                    "full-index NONHEAD forward/backward deltas disagree with recovered HEAD topology",
                ));
            }
        }
    }
    Ok(())
}

fn validate_target_inode(inode: &Inode) -> Result<usize, CoreError> {'''
assert s.count(anchor) == 1
s = s.replace(anchor, helper)
path.write_text(s)
