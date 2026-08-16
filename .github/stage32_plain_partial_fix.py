from pathlib import Path

path = Path('crates/loom-erofs/src/compact_core.rs')
s = path.read_text()

old = '''fn encode_plain_extent(head_lcn: usize, extent: &[u8]) -> Result<(Vec<u8>, usize), CoreError> {
    if extent.len() != BLOCK_BYTES {
        return Err(CoreError::UnsupportedInode(
            "full-index PLAIN data head must cover exactly one aligned 4 KiB logical cluster",
        ));
    }
    let _ = head_lcn;
    Ok((extent.to_vec(), BLOCK_BYTES))
}'''
new = '''fn encode_plain_extent(head_lcn: usize, extent: &[u8]) -> Result<(Vec<u8>, usize), CoreError> {
    if extent.is_empty() || extent.len() > BLOCK_BYTES {
        return Err(CoreError::UnsupportedInode(
            "full-index PLAIN data head must fit within one logical cluster",
        ));
    }
    let _ = head_lcn;
    let mut block = vec![0_u8; BLOCK_BYTES];
    block[..extent.len()].copy_from_slice(extent);
    Ok((block, extent.len()))
}'''
assert s.count(old) == 1, s.count(old)
s = s.replace(old, new)

old = '''    let eof = entries.last().ok_or(CoreError::UnexpectedEndOfStructure)?;
    if eof.kind != LCLUSTER_PLAIN
        || usize::from(eof.clusterofs) != remainder
        || eof.word != 0
        || eof.advise != LCLUSTER_PLAIN
    {
        return Err(CoreError::InvalidFilesystem(
            "partial full-index file lacks the expected zero-block PLAIN EOF sentinel",
        ));
    }
    Ok(Some(remainder))'''
new = '''    let eof = entries.last().ok_or(CoreError::UnexpectedEndOfStructure)?;
    if eof.kind != LCLUSTER_PLAIN || eof.advise != LCLUSTER_PLAIN {
        return Err(CoreError::InvalidFilesystem(
            "partial full-index file must end in a verified PLAIN entry",
        ));
    }
    if usize::from(eof.clusterofs) == remainder && eof.word == 0 {
        return Ok(Some(remainder));
    }
    if eof.clusterofs == 0 && eof.word != 0 {
        return Ok(None);
    }
    Err(CoreError::InvalidFilesystem(
        "partial full-index PLAIN tail is neither a zero-block EOF sentinel nor an aligned raw data head",
    ))'''
assert s.count(old) == 1, s.count(old)
s = s.replace(old, new)

old = '''            return Err(CoreError::UnsupportedInode(
                "full-index PLAIN data head must cover exactly one aligned 4 KiB logical cluster",
            ));'''
new = '''            return Err(CoreError::UnsupportedInode(
                "full-index PLAIN data head must occupy exactly one logical lcluster; only the final lcluster may be EOF-clamped",
            ));'''
assert s.count(old) == 1, s.count(old)
s = s.replace(old, new)

path.write_text(s)
