from pathlib import Path

core = Path('crates/loom-erofs/src/compact_core.rs')
s = core.read_text()

old = '''#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Head {
    lcn: usize,
    pcluster: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Topology {'''
new = '''#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Head {
    lcn: usize,
    pcluster: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lz4Placement {
    LegacyStart,
    ZeroPadding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Topology {'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''    algorithm: u8,
    advise: u16,
    logical_lclusters: usize,'''
new = '''    algorithm: u8,
    advise: u16,
    placement: Lz4Placement,
    logical_lclusters: usize,'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''    match origin.sb.incompat {
        FEATURE_LZ4_0PADDING => compile_oracle(origin_path, target_path, replacement_image_path),
        BIG_REQUIRED_INCOMPAT => {
            compile_big_oracle(origin_path, target_path, replacement_image_path)
        }
        _ => Err(CoreError::UnsupportedFilesystem(
            "multi compact oracle supports only ordinary LZ4_0PADDING or big-pcluster compact images",
        )),
    }'''
new = '''    match origin.sb.incompat {
        0 | FEATURE_LZ4_0PADDING => {
            compile_oracle(origin_path, target_path, replacement_image_path)
        }
        BIG_REQUIRED_INCOMPAT => {
            compile_big_oracle(origin_path, target_path, replacement_image_path)
        }
        _ => Err(CoreError::UnsupportedFilesystem(
            "multi compressed oracle supports legacy full-index, ordinary LZ4_0PADDING, or big-pcluster compact images",
        )),
    }'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''    match origin.sb.incompat {
        FEATURE_LZ4_0PADDING => compile_lz4(origin_path, target_path, replacement_path),
        BIG_REQUIRED_INCOMPAT => compile_big_lz4(origin_path, target_path, replacement_path),
        _ => Err(CoreError::UnsupportedFilesystem(
            "multi compact self-encode supports only ordinary LZ4_0PADDING or big-pcluster compact images",
        )),
    }'''
new = '''    match origin.sb.incompat {
        0 | FEATURE_LZ4_0PADDING => compile_lz4(origin_path, target_path, replacement_path),
        BIG_REQUIRED_INCOMPAT => compile_big_lz4(origin_path, target_path, replacement_path),
        _ => Err(CoreError::UnsupportedFilesystem(
            "multi compressed self-encode supports legacy full-index, ordinary LZ4_0PADDING, or big-pcluster compact images",
        )),
    }'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''        let (block, encoded_len) = encode_extent(head.lcn, extent)?;'''
new = '''        let (block, encoded_len) = encode_extent(head.lcn, extent, topology.placement)?;'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''fn encode_extent(head_lcn: usize, extent: &[u8]) -> Result<(Vec<u8>, usize), CoreError> {'''
new = '''fn encode_extent(
    head_lcn: usize,
    extent: &[u8],
    placement: Lz4Placement,
) -> Result<(Vec<u8>, usize), CoreError> {'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''    let mut pcluster = vec![0_u8; BLOCK_BYTES];
    let start = BLOCK_BYTES
        .checked_sub(compressed.len())
        .ok_or(CoreError::ArithmeticOverflow)?;
    pcluster[start..].copy_from_slice(&compressed);
    if lz4::decode_0padding(&pcluster, extent.len())
        .map_err(|_| CoreError::CompressionValidationFailed)?
        != extent
    {
        return Err(CoreError::CompressionValidationFailed);
    }
    Ok((pcluster, compressed.len()))'''
new = '''    let mut pcluster = vec![0_u8; BLOCK_BYTES];
    match placement {
        Lz4Placement::LegacyStart => {
            pcluster[..compressed.len()].copy_from_slice(&compressed);
        }
        Lz4Placement::ZeroPadding => {
            let start = BLOCK_BYTES
                .checked_sub(compressed.len())
                .ok_or(CoreError::ArithmeticOverflow)?;
            pcluster[start..].copy_from_slice(&compressed);
            if lz4::decode_0padding(&pcluster, extent.len())
                .map_err(|_| CoreError::CompressionValidationFailed)?
                != extent
            {
                return Err(CoreError::CompressionValidationFailed);
            }
        }
    }
    Ok((pcluster, compressed.len()))'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''    fn read_topology(&mut self, nid: u64) -> Result<Topology, CoreError> {
        if self.sb.incompat != FEATURE_LZ4_0PADDING {
            return Err(CoreError::UnsupportedFilesystem(
                "normal compact mode does not accept big-pcluster incompatible features",
            ));
        }
        let inode = self.read_inode(nid)?;
        if inode.layout == DATA_COMPRESSED_FULL {
            return self.read_full_topology_from_inode(inode);
        }
        let logical_lclusters = validate_target_inode(&inode)?;'''
new = '''    fn read_topology(&mut self, nid: u64) -> Result<Topology, CoreError> {
        let inode = self.read_inode(nid)?;
        if inode.layout == DATA_COMPRESSED_FULL {
            if self.sb.incompat != 0 {
                return Err(CoreError::UnsupportedFilesystem(
                    "Stage 29 legacy full-index requires non-0padding LZ4 placement",
                ));
            }
            return self.read_full_topology_from_inode(inode);
        }
        if self.sb.incompat != FEATURE_LZ4_0PADDING {
            return Err(CoreError::UnsupportedFilesystem(
                "normal compact mode requires LZ4_0PADDING without big-pcluster features",
            ));
        }
        let logical_lclusters = validate_target_inode(&inode)?;'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''            algorithm: map.algorithm,
            advise: map.advise,
            logical_lclusters,'''
new = '''            algorithm: map.algorithm,
            advise: map.advise,
            placement: Lz4Placement::ZeroPadding,
            logical_lclusters,'''
assert s.count(old) == 1
s = s.replace(old, new, 1)

# The remaining constructor with the same algorithm/advise shape is the full-index topology.
old = '''            algorithm: map.algorithm,
            advise: map.advise,
            logical_lclusters,
            compact_2b_entries: 0,'''
new = '''            algorithm: map.algorithm,
            advise: map.advise,
            placement: Lz4Placement::LegacyStart,
            logical_lclusters,
            compact_2b_entries: 0,'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''    if origin.advise != replacement.advise {
        return Err(CoreError::IncompatibleReplacement(
            "compact map advice differs",
        ));
    }
    if origin.logical_lclusters != replacement.logical_lclusters {'''
new = '''    if origin.advise != replacement.advise {
        return Err(CoreError::IncompatibleReplacement(
            "compressed map advice differs",
        ));
    }
    if origin.placement != replacement.placement {
        return Err(CoreError::IncompatibleReplacement(
            "LZ4 physical placement mode differs",
        ));
    }
    if origin.logical_lclusters != replacement.logical_lclusters {'''
assert s.count(old) == 1
s = s.replace(old, new)

old = '''    if incompat & FEATURE_LZ4_0PADDING == 0 {
        return Err(CoreError::UnsupportedFilesystem(
            "compact core expects LZ4_0PADDING layout",
        ));
    }
'''
assert s.count(old) == 1
s = s.replace(old, '')

core.write_text(s)

proof = Path('tests/integration/stage29_erofs_legacy_full_index.sh')
p = proof.read_text()
old = '''assert struct.unpack_from('<I', raw, sb)[0] == 0xE0F5E1E2
meta = struct.unpack_from('<I', raw, sb + 0x28)[0]'''
new = '''assert struct.unpack_from('<I', raw, sb)[0] == 0xE0F5E1E2
incompat = struct.unpack_from('<I', raw, sb + 0x50)[0]
assert incompat == 0, incompat
meta = struct.unpack_from('<I', raw, sb + 0x28)[0]'''
assert p.count(old) == 1
p = p.replace(old, new, 1)
old = '''print(f'Stage 29 raw full-index topology PASS nid={nid} heads={heads} data_word={blocks} full_start={full_start}')'''
new = '''print(f'Stage 29 raw full-index topology PASS nid={nid} heads={heads} data_word={blocks} full_start={full_start} incompat={incompat}')'''
assert p.count(old) == 1
p = p.replace(old, new)
old = '''  '  inode layout: EROFS_INODE_COMPRESSED_FULL (1)' \\
  '  logical lclusters: 24' '''
new = '''  '  inode layout: EROFS_INODE_COMPRESSED_FULL (1)' \\
  '  superblock incompat: 0 (legacy non-0padding LZ4)' \\
  '  logical lclusters: 24' '''
assert p.count(old) == 1
p = p.replace(old, new)
proof.write_text(p)
