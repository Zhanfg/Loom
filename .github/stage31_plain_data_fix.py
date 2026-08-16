from pathlib import Path

path = Path('crates/loom-erofs/src/compact_core.rs')
s = path.read_text()

old = '''#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Head {
    lcn: usize,
    pcluster: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lz4Placement {'''
new = '''#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeadKind {
    Lz4,
    Plain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Head {
    lcn: usize,
    pcluster: u64,
    kind: HeadKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lz4Placement {'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''        let (block, encoded_len) = encode_extent(head.lcn, extent, topology.placement)?;
        encoded_blocks.push(block);
        encoded_bytes.push(encoded_len);'''
new = '''        let (block, encoded_len) = match head.kind {
            HeadKind::Lz4 => encode_extent(head.lcn, extent, topology.placement)?,
            HeadKind::Plain => encode_plain_extent(head.lcn, extent)?,
        };
        encoded_blocks.push(block);
        encoded_bytes.push(encoded_len);'''
assert s.count(old) == 1
s = s.replace(old, new)

anchor = '''fn encode_extent(
    head_lcn: usize,
    extent: &[u8],
    placement: Lz4Placement,
) -> Result<(Vec<u8>, usize), CoreError> {'''
helper = '''fn encode_plain_extent(head_lcn: usize, extent: &[u8]) -> Result<(Vec<u8>, usize), CoreError> {
    if extent.len() != BLOCK_BYTES {
        return Err(CoreError::UnsupportedInode(
            "full-index PLAIN data head must cover exactly one aligned 4 KiB logical cluster",
        ));
    }
    let _ = head_lcn;
    Ok((extent.to_vec(), BLOCK_BYTES))
}

fn encode_extent(
    head_lcn: usize,
    extent: &[u8],
    placement: Lz4Placement,
) -> Result<(Vec<u8>, usize), CoreError> {'''
assert s.count(anchor) == 1
s = s.replace(anchor, helper)

old = '''    if origin
        .heads
        .iter()
        .map(|head| head.lcn)
        .ne(replacement.heads.iter().map(|head| head.lcn))
    {
        return Err(CoreError::IncompatibleReplacement(
            "compressed HEAD-lcluster topology differs",
        ));
    }'''
new = '''    if origin
        .heads
        .iter()
        .map(|head| (head.lcn, head.kind))
        .ne(replacement.heads.iter().map(|head| (head.lcn, head.kind)))
    {
        return Err(CoreError::IncompatibleReplacement(
            "compressed HEAD type/lcluster topology differs",
        ));
    }'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''                    heads.push(Head {
                        lcn,
                        pcluster: u64::from(entry.word),
                    });'''
new = '''                    heads.push(Head {
                        lcn,
                        pcluster: u64::from(entry.word),
                        kind: HeadKind::Lz4,
                    });'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''                LCLUSTER_PLAIN => {
                    if eof_plain_clusterofs.is_none() || lcn + 1 != logical_lclusters {
                        return Err(CoreError::UnsupportedInode(
                            "full-index PLAIN entries are supported only as the partial-EOF sentinel",
                        ));
                    }
                }'''
new = '''                LCLUSTER_PLAIN => {
                    let is_eof_sentinel =
                        eof_plain_clusterofs.is_some() && lcn + 1 == logical_lclusters;
                    if !is_eof_sentinel {
                        if entry.clusterofs != 0 {
                            return Err(CoreError::UnsupportedInode(
                                "full-index PLAIN data heads require zero cluster offsets",
                            ));
                        }
                        heads.push(Head {
                            lcn,
                            pcluster: u64::from(entry.word),
                            kind: HeadKind::Plain,
                        });
                    }
                }'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''        if heads.len() != compressed_blocks {
            return Err(CoreError::InvalidFilesystem(
                "full-index compressed block count does not match recovered HEAD count",
            ));
        }

        validate_full_nonheads(&entries, &heads, logical_lclusters, eof_plain_clusterofs)?;'''
new = '''        if heads.len() != compressed_blocks {
            return Err(CoreError::InvalidFilesystem(
                "full-index encoded physical-block count does not match recovered data HEAD count",
            ));
        }

        validate_full_plain_data_heads(&heads, logical_lclusters, eof_plain_clusterofs)?;
        validate_full_nonheads(&entries, &heads, logical_lclusters, eof_plain_clusterofs)?;'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''                    let pcluster = self.reconstruct_head_pcluster(ebase, total, lcn, *entry)?;
                    heads.push(Head { lcn, pcluster });'''
new = '''                    let pcluster = self.reconstruct_head_pcluster(ebase, total, lcn, *entry)?;
                    heads.push(Head {
                        lcn,
                        pcluster,
                        kind: HeadKind::Lz4,
                    });'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''    if remainder == 0 {
        if entries.last().map(|entry| entry.kind) == Some(LCLUSTER_PLAIN) {
            return Err(CoreError::InvalidFilesystem(
                "block-aligned full-index file unexpectedly ends in PLAIN",
            ));
        }
        return Ok(None);
    }'''
new = '''    if remainder == 0 {
        return Ok(None);
    }'''
assert s.count(old) == 1
s = s.replace(old, new)

anchor = '''fn validate_full_nonheads(
    entries: &[FullEntry],
    heads: &[Head],
    logical_lclusters: usize,
    eof_plain_clusterofs: Option<usize>,
) -> Result<(), CoreError> {'''
helper = '''fn validate_full_plain_data_heads(
    heads: &[Head],
    logical_lclusters: usize,
    eof_plain_clusterofs: Option<usize>,
) -> Result<(), CoreError> {
    for (index, head) in heads.iter().enumerate() {
        if head.kind != HeadKind::Plain {
            continue;
        }
        let end = heads.get(index + 1).map_or_else(
            || {
                if eof_plain_clusterofs.is_some() {
                    logical_lclusters.saturating_sub(1)
                } else {
                    logical_lclusters
                }
            },
            |next| next.lcn,
        );
        if end != head.lcn.saturating_add(1) {
            return Err(CoreError::UnsupportedInode(
                "full-index PLAIN data head must cover exactly one aligned 4 KiB logical cluster",
            ));
        }
    }
    Ok(())
}

fn validate_full_nonheads(
    entries: &[FullEntry],
    heads: &[Head],
    logical_lclusters: usize,
    eof_plain_clusterofs: Option<usize>,
) -> Result<(), CoreError> {'''
assert s.count(anchor) == 1
s = s.replace(anchor, helper)

old = '''            heads: vec![Head {
                lcn: 0,
                pcluster: 10,
            }],'''
new = '''            heads: vec![Head {
                lcn: 0,
                pcluster: 10,
                kind: HeadKind::Lz4,
            }],'''
assert s.count(old) == 1
s = s.replace(old, new)

path.write_text(s)
