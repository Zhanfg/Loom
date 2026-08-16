from pathlib import Path

path = Path('crates/loom-erofs/src/compact_core.rs')
s = path.read_text()

old = '''        let entries = self.read_all_full_entries(map.ebase, logical_lclusters)?;
        let mut heads = Vec::new();
        for (lcn, entry) in entries.iter().enumerate() {
            if entry.advise & !LCLUSTER_TYPE_MASK != 0 {
                return Err(CoreError::UnsupportedInode(
                    "Stage 29 full-index entries do not accept auxiliary advice bits",
                ));
            }
            if entry.clusterofs != 0 {
                return Err(CoreError::UnsupportedInode(
                    "Stage 29 full-index entries require zero cluster offsets",
                ));
            }
            match entry.kind {
                LCLUSTER_HEAD1 => heads.push(Head {
                    lcn,
                    pcluster: u64::from(entry.word),
                }),
                LCLUSTER_NONHEAD => {}
                LCLUSTER_PLAIN => {
                    return Err(CoreError::UnsupportedInode(
                        "Stage 29 full-index does not yet accept PLAIN lclusters",
                    ))
                }
                _ => {
                    return Err(CoreError::UnsupportedInode(
                        "Stage 29 full-index supports only HEAD1 and NONHEAD entries",
                    ))
                }
            }
        }'''
new = '''        let entries = self.read_all_full_entries(map.ebase, logical_lclusters)?;
        let eof_plain_clusterofs = validate_full_eof_plain_sentinel(
            &entries,
            logical_lclusters,
            inode.size,
        )?;
        let mut heads = Vec::new();
        for (lcn, entry) in entries.iter().enumerate() {
            if entry.advise & !LCLUSTER_TYPE_MASK != 0 {
                return Err(CoreError::UnsupportedInode(
                    "full-index entries do not accept auxiliary advice bits",
                ));
            }
            match entry.kind {
                LCLUSTER_HEAD1 => {
                    if entry.clusterofs != 0 {
                        return Err(CoreError::UnsupportedInode(
                            "full-index HEAD1 entries require zero cluster offsets",
                        ));
                    }
                    heads.push(Head {
                        lcn,
                        pcluster: u64::from(entry.word),
                    });
                }
                LCLUSTER_NONHEAD => {
                    if entry.clusterofs != 0 {
                        return Err(CoreError::UnsupportedInode(
                            "full-index NONHEAD entries require zero cluster offsets",
                        ));
                    }
                }
                LCLUSTER_PLAIN => {
                    if eof_plain_clusterofs.is_none() || lcn + 1 != logical_lclusters {
                        return Err(CoreError::UnsupportedInode(
                            "full-index PLAIN entries are supported only as the partial-EOF sentinel",
                        ));
                    }
                }
                _ => {
                    return Err(CoreError::UnsupportedInode(
                        "full-index supports only HEAD1, NONHEAD, and the verified partial-EOF PLAIN sentinel",
                    ))
                }
            }
        }'''
assert s.count(old) == 1, s.count(old)
s = s.replace(old, new)

old = '''        validate_full_nonheads(&entries, &heads, logical_lclusters)?;
        validate_head_blocks(&heads, self.bytes)?;'''
new = '''        validate_full_nonheads(
            &entries,
            &heads,
            logical_lclusters,
            eof_plain_clusterofs,
        )?;
        validate_head_blocks(&heads, self.bytes)?;'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''            compact_2b_entries: 0,
            eof_plain_clusterofs: None,
            heads,'''
new = '''            compact_2b_entries: 0,
            eof_plain_clusterofs,
            heads,'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''fn validate_full_nonheads(
    entries: &[FullEntry],
    heads: &[Head],
    logical_lclusters: usize,
) -> Result<(), CoreError> {
    for (index, head) in heads.iter().enumerate() {
        let end = heads
            .get(index + 1)
            .map_or(logical_lclusters, |next| next.lcn);'''
new = '''fn validate_full_eof_plain_sentinel(
    entries: &[FullEntry],
    total: usize,
    logical_size: u64,
) -> Result<Option<usize>, CoreError> {
    if entries.len() != total || total == 0 {
        return Err(CoreError::InvalidFilesystem(
            "full-index vector length differs from logical lcluster count",
        ));
    }
    let remainder = usize::try_from(logical_size % u64::from(BLOCK_SIZE))
        .map_err(|_| CoreError::ArithmeticOverflow)?;
    if remainder == 0 {
        if entries.last().map(|entry| entry.kind) == Some(LCLUSTER_PLAIN) {
            return Err(CoreError::InvalidFilesystem(
                "block-aligned full-index file unexpectedly ends in PLAIN",
            ));
        }
        return Ok(None);
    }
    let eof = entries.last().ok_or(CoreError::UnexpectedEndOfStructure)?;
    if eof.kind != LCLUSTER_PLAIN
        || usize::from(eof.clusterofs) != remainder
        || eof.word != 0
        || eof.advise != LCLUSTER_PLAIN
    {
        return Err(CoreError::InvalidFilesystem(
            "partial full-index file lacks the expected zero-block PLAIN EOF sentinel",
        ));
    }
    Ok(Some(remainder))
}

fn validate_full_nonheads(
    entries: &[FullEntry],
    heads: &[Head],
    logical_lclusters: usize,
    eof_plain_clusterofs: Option<usize>,
) -> Result<(), CoreError> {
    for (index, head) in heads.iter().enumerate() {
        let end = heads.get(index + 1).map_or_else(
            || {
                if eof_plain_clusterofs.is_some() {
                    logical_lclusters.saturating_sub(1)
                } else {
                    logical_lclusters
                }
            },
            |next| next.lcn,
        );'''
assert s.count(old) == 1, s.count(old)
s = s.replace(old, new)

path.write_text(s)
