from pathlib import Path

path = Path('crates/loom-erofs/src/compact_core.rs')
s = path.read_text()

old = '''        let entries = self.read_all_full_entries(map.ebase, logical_lclusters)?;
        let extents = recover_full_big_extents(&entries, logical_lclusters)?;
        validate_big_total_physical_blocks(&extents, encoded_physical_blocks)?;
        validate_big_block_spans(&extents, self.bytes)?;

        Ok(BigTopology {
            nid: inode.nid,
            logical_size: inode.size,
            placement: Lz4Placement::LegacyStart,
            logical_lclusters,
            compact_2b_entries: 0,
            eof_plain_clusterofs: None,
            extents,
        })'''
new = '''        let entries = self.read_all_full_entries(map.ebase, logical_lclusters)?;
        let eof_plain_clusterofs =
            validate_full_eof_plain_sentinel(&entries, logical_lclusters, inode.size)?;
        if inode.size % u64::from(BLOCK_SIZE) != 0 && eof_plain_clusterofs.is_none() {
            return Err(CoreError::UnsupportedInode(
                "Stage 34 full big-pcluster partial EOF requires the verified zero-block PLAIN sentinel",
            ));
        }
        let extents =
            recover_full_big_extents(&entries, logical_lclusters, eof_plain_clusterofs)?;
        validate_big_total_physical_blocks(&extents, encoded_physical_blocks)?;
        validate_big_block_spans(&extents, self.bytes)?;

        Ok(BigTopology {
            nid: inode.nid,
            logical_size: inode.size,
            placement: Lz4Placement::LegacyStart,
            logical_lclusters,
            compact_2b_entries: 0,
            eof_plain_clusterofs,
            extents,
        })'''
assert s.count(old) == 1, s.count(old)
s = s.replace(old, new)

old = '''fn recover_full_big_extents(
    entries: &[FullEntry],
    total: usize,
) -> Result<Vec<BigExtent>, CoreError> {'''
new = '''fn recover_full_big_extents(
    entries: &[FullEntry],
    total: usize,
    eof_plain_clusterofs: Option<usize>,
) -> Result<Vec<BigExtent>, CoreError> {'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''    let mut head_lcns = Vec::new();
    for (lcn, entry) in entries.iter().enumerate() {
        if entry.advise & !LCLUSTER_TYPE_MASK != 0 {
            return Err(CoreError::UnsupportedInode(
                "Stage 33 full big-pcluster entries do not accept auxiliary advice bits",
            ));
        }
        if entry.clusterofs != 0 {
            return Err(CoreError::UnsupportedInode(
                "Stage 33 full big-pcluster entries require zero cluster offsets",
            ));
        }
        match entry.kind {
            LCLUSTER_HEAD1 => head_lcns.push(lcn),
            LCLUSTER_NONHEAD => {}
            _ => {
                return Err(CoreError::UnsupportedInode(
                    "Stage 33 full big-pcluster supports only HEAD1 and NONHEAD entries",
                ));
            }
        }
    }'''
new = '''    let mut head_lcns = Vec::new();
    for (lcn, entry) in entries.iter().enumerate() {
        if entry.advise & !LCLUSTER_TYPE_MASK != 0 {
            return Err(CoreError::UnsupportedInode(
                "full big-pcluster entries do not accept auxiliary advice bits",
            ));
        }
        let is_eof_sentinel = eof_plain_clusterofs.is_some() && lcn + 1 == total;
        if is_eof_sentinel {
            if entry.kind != LCLUSTER_PLAIN {
                return Err(CoreError::InvalidFilesystem(
                    "full big-pcluster EOF sentinel is not PLAIN",
                ));
            }
            continue;
        }
        if entry.clusterofs != 0 {
            return Err(CoreError::UnsupportedInode(
                "full big-pcluster data entries require zero cluster offsets",
            ));
        }
        match entry.kind {
            LCLUSTER_HEAD1 => head_lcns.push(lcn),
            LCLUSTER_NONHEAD => {}
            _ => {
                return Err(CoreError::UnsupportedInode(
                    "full big-pcluster supports only HEAD1/NONHEAD data plus the verified EOF PLAIN sentinel",
                ));
            }
        }
    }'''
assert s.count(old) == 1, s.count(old)
s = s.replace(old, new)

old = '''    let mut extents = Vec::with_capacity(head_lcns.len());
    for (index, &head_lcn) in head_lcns.iter().enumerate() {
        let next_head = head_lcns.get(index + 1).copied().unwrap_or(total);'''
new = '''    let data_end = if eof_plain_clusterofs.is_some() {
        total.saturating_sub(1)
    } else {
        total
    };
    let mut extents = Vec::with_capacity(head_lcns.len());
    for (index, &head_lcn) in head_lcns.iter().enumerate() {
        let next_head = head_lcns.get(index + 1).copied().unwrap_or(data_end);'''
assert s.count(old) == 1
s = s.replace(old, new)

# The only existing full-big caller is in read_full_big_topology_from_inode.
# Keep compile-time coverage by updating any tests or helpers that call the private
# function directly if they exist later; currently none do.

path.write_text(s)
