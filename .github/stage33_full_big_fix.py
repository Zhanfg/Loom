from pathlib import Path

path = Path('crates/loom-erofs/src/compact_core.rs')
s = path.read_text()

old = '''const BIG_ADVISE: u16 = ADVISE_COMPACTED_2B | ADVISE_BIG_PCLUSTER_1 | ADVISE_BIG_PCLUSTER_2;'''
new = '''const BIG_ADVISE: u16 = ADVISE_COMPACTED_2B | ADVISE_BIG_PCLUSTER_1 | ADVISE_BIG_PCLUSTER_2;
const FULL_BIG_ADVISE: u16 = ADVISE_BIG_PCLUSTER_1;'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''struct BigTopology {
    nid: u64,
    logical_size: u64,
    logical_lclusters: usize,
    compact_2b_entries: usize,
    eof_plain_clusterofs: Option<usize>,
    extents: Vec<BigExtent>,
}'''
new = '''struct BigTopology {
    nid: u64,
    logical_size: u64,
    placement: Lz4Placement,
    logical_lclusters: usize,
    compact_2b_entries: usize,
    eof_plain_clusterofs: Option<usize>,
    extents: Vec<BigExtent>,
}'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''        BIG_REQUIRED_INCOMPAT => {
            compile_big_oracle(origin_path, target_path, replacement_image_path)
        }
        _ => Err(CoreError::UnsupportedFilesystem(
            "multi compressed oracle supports legacy full-index, ordinary LZ4_0PADDING, or big-pcluster compact images",
        )),'''
new = '''        FEATURE_BIG_PCLUSTER | BIG_REQUIRED_INCOMPAT => {
            compile_big_oracle(origin_path, target_path, replacement_image_path)
        }
        _ => Err(CoreError::UnsupportedFilesystem(
            "multi compressed oracle supports legacy/full ordinary or big-pcluster images and compact LZ4_0PADDING variants",
        )),'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''        BIG_REQUIRED_INCOMPAT => compile_big_lz4(origin_path, target_path, replacement_path),
        _ => Err(CoreError::UnsupportedFilesystem(
            "multi compressed self-encode supports legacy full-index, ordinary LZ4_0PADDING, or big-pcluster compact images",
        )),'''
new = '''        FEATURE_BIG_PCLUSTER | BIG_REQUIRED_INCOMPAT => {
            compile_big_lz4(origin_path, target_path, replacement_path)
        }
        _ => Err(CoreError::UnsupportedFilesystem(
            "multi compressed self-encode supports legacy/full ordinary or big-pcluster images and compact LZ4_0PADDING variants",
        )),'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''        let compressed =
            lz4::encode(logical_extent).map_err(|_| CoreError::CompressionValidationFailed)?;
        if compressed.len() > capacity {
            return Err(CoreError::CompressionDoesNotFit {
                head_lcn: extent.lcn,
                encoded: compressed.len(),
                capacity,
            });
        }
        if compressed.first().copied().unwrap_or(0) == 0 {
            return Err(CoreError::CompressionValidationFailed);
        }
        if lz4::decode(&compressed, logical_extent.len())
            .map_err(|_| CoreError::CompressionValidationFailed)?
            != logical_extent
        {
            return Err(CoreError::CompressionValidationFailed);
        }

        let mut span = vec![0_u8; capacity];
        let padded_start = capacity
            .checked_sub(compressed.len())
            .ok_or(CoreError::ArithmeticOverflow)?;
        span[padded_start..].copy_from_slice(&compressed);
        if lz4::decode_0padding(&span, logical_extent.len())
            .map_err(|_| CoreError::CompressionValidationFailed)?
            != logical_extent
        {
            return Err(CoreError::CompressionValidationFailed);
        }
        encoded_bytes.push(compressed.len());
        encoded_spans.push(span);'''
new = '''        let (span, encoded_len) = encode_big_extent(
            extent.lcn,
            logical_extent,
            capacity,
            topology.placement,
        )?;
        encoded_bytes.push(encoded_len);
        encoded_spans.push(span);'''
assert s.count(old) == 1
s = s.replace(old, new)

anchor = '''fn encode_plain_extent(head_lcn: usize, extent: &[u8]) -> Result<(Vec<u8>, usize), CoreError> {'''
helper = '''fn encode_big_extent(
    head_lcn: usize,
    logical_extent: &[u8],
    capacity: usize,
    placement: Lz4Placement,
) -> Result<(Vec<u8>, usize), CoreError> {
    let compressed =
        lz4::encode(logical_extent).map_err(|_| CoreError::CompressionValidationFailed)?;
    if compressed.len() > capacity {
        return Err(CoreError::CompressionDoesNotFit {
            head_lcn,
            encoded: compressed.len(),
            capacity,
        });
    }
    if compressed.first().copied().unwrap_or(0) == 0 {
        return Err(CoreError::CompressionValidationFailed);
    }
    if lz4::decode(&compressed, logical_extent.len())
        .map_err(|_| CoreError::CompressionValidationFailed)?
        != logical_extent
    {
        return Err(CoreError::CompressionValidationFailed);
    }

    let mut span = vec![0_u8; capacity];
    match placement {
        Lz4Placement::LegacyStart => span[..compressed.len()].copy_from_slice(&compressed),
        Lz4Placement::ZeroPadding => {
            let start = capacity
                .checked_sub(compressed.len())
                .ok_or(CoreError::ArithmeticOverflow)?;
            span[start..].copy_from_slice(&compressed);
            if lz4::decode_0padding(&span, logical_extent.len())
                .map_err(|_| CoreError::CompressionValidationFailed)?
                != logical_extent
            {
                return Err(CoreError::CompressionValidationFailed);
            }
        }
    }
    Ok((span, compressed.len()))
}

fn encode_plain_extent(head_lcn: usize, extent: &[u8]) -> Result<(Vec<u8>, usize), CoreError> {'''
assert s.count(anchor) == 1
s = s.replace(anchor, helper)

old = '''fn validate_big_compatible_topology(
    origin: &BigTopology,
    replacement: &BigTopology,
) -> Result<(), CoreError> {
    if origin.logical_size != replacement.logical_size'''
new = '''fn validate_big_compatible_topology(
    origin: &BigTopology,
    replacement: &BigTopology,
) -> Result<(), CoreError> {
    if origin.placement != replacement.placement {
        return Err(CoreError::IncompatibleReplacement(
            "big-pcluster LZ4 physical placement mode differs",
        ));
    }
    if origin.logical_size != replacement.logical_size'''
assert s.count(old) == 1
s = s.replace(old, new)

start = s.index('    fn read_big_topology(&mut self, nid: u64) -> Result<BigTopology, CoreError> {')
end = s.index('    fn read_map_header(', start)
chunk = s[start:end]
old = '''    fn read_big_topology(&mut self, nid: u64) -> Result<BigTopology, CoreError> {
        if self.sb.incompat != BIG_REQUIRED_INCOMPAT {
            return Err(CoreError::UnsupportedFilesystem(
                "big-pcluster proof requires only LZ4_0PADDING + big-pcluster incompatible features",
            ));
        }
        let inode = self.read_inode(nid)?;
        let logical_lclusters = validate_target_inode(&inode)?;'''
new = '''    fn read_big_topology(&mut self, nid: u64) -> Result<BigTopology, CoreError> {
        let inode = self.read_inode(nid)?;
        if inode.layout == DATA_COMPRESSED_FULL {
            if self.sb.incompat != FEATURE_BIG_PCLUSTER {
                return Err(CoreError::UnsupportedFilesystem(
                    "legacy full-index big-pcluster requires only the BIG_PCLUSTER incompat feature",
                ));
            }
            return self.read_full_big_topology_from_inode(inode);
        }
        if self.sb.incompat != BIG_REQUIRED_INCOMPAT {
            return Err(CoreError::UnsupportedFilesystem(
                "compact big-pcluster requires LZ4_0PADDING + BIG_PCLUSTER incompat features",
            ));
        }
        let logical_lclusters = validate_target_inode(&inode)?;'''
assert chunk.count(old) == 1
chunk = chunk.replace(old, new)
old = '''        Ok(BigTopology {
            nid,
            logical_size: inode.size,
            logical_lclusters,'''
new = '''        Ok(BigTopology {
            nid,
            logical_size: inode.size,
            placement: Lz4Placement::ZeroPadding,
            logical_lclusters,'''
assert chunk.count(old) == 1
chunk = chunk.replace(old, new)

insert_before = '    fn read_map_header(\n'
full_reader = '''    fn read_full_big_topology_from_inode(
        &mut self,
        inode: Inode,
    ) -> Result<BigTopology, CoreError> {
        if inode.file_type() != MODE_REGULAR {
            return Err(CoreError::NotRegularFile(inode.nid));
        }
        if inode.layout != DATA_COMPRESSED_FULL {
            return Err(CoreError::UnsupportedInode(
                "full big-pcluster reader requires EROFS_INODE_COMPRESSED_FULL",
            ));
        }
        let logical_lclusters_u64 = div_ceil(inode.size, u64::from(BLOCK_SIZE))?;
        if logical_lclusters_u64 < 2 {
            return Err(CoreError::UnsupportedInode(
                "full big-pcluster requires at least two logical clusters",
            ));
        }
        let logical_lclusters = usize::try_from(logical_lclusters_u64)
            .map_err(|_| CoreError::ArithmeticOverflow)?;
        let encoded_physical_blocks =
            usize::try_from(inode.data_word).map_err(|_| CoreError::ArithmeticOverflow)?;
        if encoded_physical_blocks == 0 {
            return Err(CoreError::InvalidFilesystem(
                "full big-pcluster inode reports zero encoded physical blocks",
            ));
        }

        let map = self.read_full_map_header(&inode)?;
        if map.advise != FULL_BIG_ADVISE {
            return Err(CoreError::UnsupportedInode(
                "Stage 33 full big-pcluster requires exactly BIG_PCLUSTER_1 map advice",
            ));
        }
        if map.algorithm != LZ4_ALGORITHM || map.secondary_algorithm != 0 || map.cluster_bits != 0 {
            return Err(CoreError::UnsupportedInode(
                "Stage 33 full big-pcluster requires HEAD1 LZ4 with 4 KiB logical clusters",
            ));
        }

        let entries = self.read_all_full_entries(map.ebase, logical_lclusters)?;
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
        })
    }

'''
chunk = chunk.replace(insert_before, full_reader + insert_before)
s = s[:start] + chunk + s[end:]

anchor = '''fn recover_big_extents(
    entries: &[CompactEntry],'''
full_helpers = '''fn recover_full_big_extents(
    entries: &[FullEntry],
    total: usize,
) -> Result<Vec<BigExtent>, CoreError> {
    if entries.len() != total {
        return Err(CoreError::InvalidFilesystem(
            "full big-pcluster index vector length differs from logical lcluster count",
        ));
    }
    let mut head_lcns = Vec::new();
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
    }
    if head_lcns.first().copied() != Some(0) {
        return Err(CoreError::InvalidFilesystem(
            "first full big-pcluster extent does not begin at lcluster zero",
        ));
    }

    let mut extents = Vec::with_capacity(head_lcns.len());
    for (index, &head_lcn) in head_lcns.iter().enumerate() {
        let next_head = head_lcns.get(index + 1).copied().unwrap_or(total);
        if next_head <= head_lcn {
            return Err(CoreError::InvalidFilesystem(
                "full big-pcluster HEAD lclusters are not strictly increasing",
            ));
        }
        let head = entries
            .get(head_lcn)
            .ok_or(CoreError::UnexpectedEndOfStructure)?;
        let physical_blocks = validate_full_big_extent(entries, head_lcn, next_head)?;
        extents.push(BigExtent {
            lcn: head_lcn,
            pcluster: u64::from(head.word),
            physical_blocks,
        });
    }
    if extents.is_empty() {
        return Err(CoreError::InvalidFilesystem(
            "full big-pcluster topology contains no HEAD",
        ));
    }
    Ok(extents)
}

fn validate_full_big_extent(
    entries: &[FullEntry],
    head_lcn: usize,
    next_head: usize,
) -> Result<usize, CoreError> {
    if next_head == head_lcn + 1 {
        return Ok(1);
    }
    let first_nonhead_lcn = head_lcn
        .checked_add(1)
        .ok_or(CoreError::ArithmeticOverflow)?;
    let first = entries
        .get(first_nonhead_lcn)
        .ok_or(CoreError::InvalidFilesystem("missing full big CBLKCNT index"))?;
    let delta0 = (first.word & 0xffff) as u16;
    let delta1 = (first.word >> 16) as u16;
    if first.kind != LCLUSTER_NONHEAD || delta0 & D0_CBLKCNT == 0 {
        return Err(CoreError::InvalidFilesystem(
            "first NONHEAD after full big HEAD does not carry D0_CBLKCNT",
        ));
    }
    let physical_blocks = usize::from(delta0 & !D0_CBLKCNT);
    if physical_blocks == 0 {
        return Err(CoreError::InvalidFilesystem(
            "full big-pcluster CBLKCNT records zero physical blocks",
        ));
    }
    let expected_first_delta1 = next_head
        .checked_sub(first_nonhead_lcn)
        .ok_or(CoreError::ArithmeticOverflow)?;
    if usize::from(delta1) != expected_first_delta1 {
        return Err(CoreError::InvalidFilesystem(
            "full big CBLKCNT entry delta1 disagrees with next HEAD",
        ));
    }

    for lcn in first_nonhead_lcn + 1..next_head {
        let entry = entries
            .get(lcn)
            .ok_or(CoreError::InvalidFilesystem("missing full big NONHEAD entry"))?;
        if entry.kind != LCLUSTER_NONHEAD {
            return Err(CoreError::InvalidFilesystem(
                "full big extent contains an unexpected non-NONHEAD entry",
            ));
        }
        let d0 = usize::from((entry.word & 0xffff) as u16);
        let d1 = usize::from((entry.word >> 16) as u16);
        let expected0 = lcn
            .checked_sub(head_lcn)
            .ok_or(CoreError::ArithmeticOverflow)?;
        let expected1 = next_head
            .checked_sub(lcn)
            .ok_or(CoreError::ArithmeticOverflow)?;
        if d0 != expected0 || d1 != expected1 {
            return Err(CoreError::InvalidFilesystem(
                "full big NONHEAD forward/backward deltas disagree with recovered HEAD topology",
            ));
        }
    }
    Ok(physical_blocks)
}

fn recover_big_extents(
    entries: &[CompactEntry],'''
assert s.count(anchor) == 1
s = s.replace(anchor, full_helpers)

path.write_text(s)
