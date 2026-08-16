from pathlib import Path
import re

p = Path('crates/loom-erofs/src/compact_core.rs')
s = p.read_text()

# Constants / feature gates.
s = s.replace(
    'const ADVISE_BIG_PCLUSTER_2: u16 = 0x0004;\n',
    'const ADVISE_BIG_PCLUSTER_2: u16 = 0x0004;\nconst ADVISE_INLINE_PCLUSTER: u16 = 0x0008;\n',
    1,
)
s = s.replace(
    'const FEATURE_BIG_PCLUSTER: u32 = 0x0000_0002;\nconst SUPPORTED_INCOMPAT: u32 = FEATURE_LZ4_0PADDING | FEATURE_BIG_PCLUSTER;\n',
    'const FEATURE_BIG_PCLUSTER: u32 = 0x0000_0002;\nconst FEATURE_ZTAILPACKING: u32 = 0x0000_0010;\nconst SUPPORTED_INCOMPAT: u32 =\n    FEATURE_LZ4_0PADDING | FEATURE_BIG_PCLUSTER | FEATURE_ZTAILPACKING;\n',
    1,
)

# Inline metadata topology.
anchor = '''#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lz4Placement {
    LegacyStart,
    ZeroPadding,
}
'''
assert anchor in s
s = s.replace(anchor, anchor + '''
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InlineTail {
    head_lcn: usize,
    header_offset: u64,
    data_offset: u64,
    capacity: usize,
}
''', 1)

s = s.replace(
    '    eof_plain_clusterofs: Option<usize>,\n    heads: Vec<Head>,\n}',
    '    eof_plain_clusterofs: Option<usize>,\n    inline_tail: Option<InlineTail>,\n    heads: Vec<Head>,\n}',
    1,
)

s = s.replace(
    '''struct FullMapHeader {
    ebase: u64,
    advise: u16,
''',
    '''struct FullMapHeader {
    header_offset: u64,
    ebase: u64,
    idata_size: u16,
    advise: u16,
''',
    1,
)

# Self-encode dispatch supports the exact ztailpacking feature bit.
s = s.replace(
    '        0 | FEATURE_LZ4_0PADDING => compile_lz4(origin_path, target_path, replacement_path),\n',
    '        0 | FEATURE_LZ4_0PADDING | FEATURE_ZTAILPACKING => {\n            compile_lz4(origin_path, target_path, replacement_path)\n        }\n',
    1,
)

# Encode inline tail before opening EffectiveBlockStore.
old = '''        let (block, encoded_len) = match head.kind {
            HeadKind::Lz4 => encode_extent(head.lcn, extent, topology.placement)?,
            HeadKind::Plain => encode_plain_extent(head.lcn, extent)?,
        };
'''
new = '''        let (block, encoded_len) = if topology
            .inline_tail
            .is_some_and(|inline| inline.head_lcn == head.lcn)
        {
            let inline = topology.inline_tail.ok_or(CoreError::InvalidFilesystem(
                "inline tail topology disappeared during encoding",
            ))?;
            if head.kind != HeadKind::Lz4 {
                return Err(CoreError::UnsupportedInode(
                    "inline pcluster support requires an LZ4 HEAD1 tail",
                ));
            }
            encode_inline_extent(head.lcn, extent, inline.capacity)?
        } else {
            match head.kind {
                HeadKind::Lz4 => encode_extent(head.lcn, extent, topology.placement)?,
                HeadKind::Plain => encode_plain_extent(head.lcn, extent)?,
            }
        };
'''
assert old in s
s = s.replace(old, new, 1)

# Dedicated fixed-capacity inline encoder.
anchor = '''fn encode_plain_extent(head_lcn: usize, extent: &[u8]) -> Result<(Vec<u8>, usize), CoreError> {'''
assert anchor in s
inline_fn = '''fn encode_inline_extent(
    head_lcn: usize,
    extent: &[u8],
    capacity: usize,
) -> Result<(Vec<u8>, usize), CoreError> {
    let compressed = lz4::encode(extent).map_err(|_| CoreError::CompressionValidationFailed)?;
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
    if lz4::decode(&compressed, extent.len()).map_err(|_| CoreError::CompressionValidationFailed)?
        != extent
    {
        return Err(CoreError::CompressionValidationFailed);
    }
    let mut span = vec![0_u8; capacity];
    span[..compressed.len()].copy_from_slice(&compressed);
    Ok((span, compressed.len()))
}

'''
s = s.replace(anchor, inline_fn + anchor, 1)

# Rewrite compile_blocks to support a metadata-resident final extent.
start = s.index('fn compile_blocks(')
end = s.index('\nfn compile_big_spans(', start)
new_compile_blocks = r'''fn compile_blocks(
    origin_path: &Path,
    topology: &Topology,
    replacement_heads: &[Head],
    encoded_blocks: Vec<Vec<u8>>,
    encoded_bytes: Vec<usize>,
) -> Result<CompiledCore, CoreError> {
    if replacement_heads.len() != topology.heads.len()
        || encoded_blocks.len() != topology.heads.len()
        || encoded_bytes.len() != topology.heads.len()
    {
        return Err(CoreError::InvalidFilesystem(
            "compact compiler received inconsistent extent vectors",
        ));
    }

    let mut view = EffectiveBlockStore::open(origin_path, BLOCK_SIZE).map_err(CoreError::View)?;
    let mut origin_pclusters = Vec::with_capacity(topology.heads.len());
    let mut replacement_pclusters = Vec::with_capacity(topology.heads.len());
    let mut head_lclusters = Vec::with_capacity(topology.heads.len());

    for (index, ((origin_head, replacement_head), encoded)) in topology
        .heads
        .iter()
        .zip(replacement_heads)
        .zip(encoded_blocks)
        .enumerate()
    {
        if let Some(inline) = topology
            .inline_tail
            .filter(|inline| inline.head_lcn == origin_head.lcn)
        {
            if encoded.len() != inline.capacity {
                return Err(CoreError::InvalidFilesystem(
                    "inline pcluster encoded span differs from fixed metadata capacity",
                ));
            }
            let encoded_len = *encoded_bytes
                .get(index)
                .ok_or(CoreError::UnexpectedEndOfStructure)?;
            let encoded_len_u16 =
                u16::try_from(encoded_len).map_err(|_| CoreError::ArithmeticOverflow)?;
            if encoded_len == 0 || encoded_len > inline.capacity {
                return Err(CoreError::InvalidFilesystem(
                    "inline pcluster encoded length exceeds fixed metadata capacity",
                ));
            }

            let metadata_block = inline.data_offset / u64::from(BLOCK_SIZE);
            let block_offset = usize::try_from(inline.data_offset % u64::from(BLOCK_SIZE))
                .map_err(|_| CoreError::ArithmeticOverflow)?;
            let end = block_offset
                .checked_add(inline.capacity)
                .ok_or(CoreError::ArithmeticOverflow)?;
            {
                let block = view.block_mut(metadata_block).map_err(CoreError::View)?;
                block
                    .get_mut(block_offset..end)
                    .ok_or(CoreError::UnexpectedEndOfStructure)?
                    .copy_from_slice(&encoded);
            }

            let header_block = inline.header_offset / u64::from(BLOCK_SIZE);
            if header_block != metadata_block {
                return Err(CoreError::InvalidFilesystem(
                    "inline pcluster header and payload moved into different metadata blocks",
                ));
            }
            let size_offset = usize::try_from(
                inline
                    .header_offset
                    .checked_add(2)
                    .ok_or(CoreError::ArithmeticOverflow)?
                    % u64::from(BLOCK_SIZE),
            )
            .map_err(|_| CoreError::ArithmeticOverflow)?;
            let size_end = size_offset
                .checked_add(2)
                .ok_or(CoreError::ArithmeticOverflow)?;
            view.block_mut(header_block)
                .map_err(CoreError::View)?
                .get_mut(size_offset..size_end)
                .ok_or(CoreError::UnexpectedEndOfStructure)?
                .copy_from_slice(&encoded_len_u16.to_le_bytes());
        } else {
            if encoded.len() != BLOCK_BYTES {
                return Err(CoreError::InvalidFilesystem(
                    "encoded extent does not occupy exactly one physical block",
                ));
            }
            view.block_mut(origin_head.pcluster)
                .map_err(CoreError::View)?
                .copy_from_slice(&encoded);
        }
        origin_pclusters.push(origin_head.pcluster);
        replacement_pclusters.push(replacement_head.pcluster);
        head_lclusters.push(origin_head.lcn);
    }

    let compiled = view.finalize().map_err(CoreError::View)?;
    if compiled.shadow_blocks > topology.heads.len() {
        return Err(CoreError::InvalidFilesystem(
            "compact shadow block count exceeds recovered extent footprint",
        ));
    }

    Ok(CompiledCore {
        map: compiled.map,
        shadow: compiled.shadow,
        block_size: compiled.block_size,
        origin_nid: topology.nid,
        origin_pclusters,
        replacement_pclusters,
        head_lclusters,
        encoded_bytes,
        logical_lclusters: topology.logical_lclusters,
        compact_2b_entries: topology.compact_2b_entries,
        shadow_blocks: compiled.shadow_blocks,
    })
}
'''
s = s[:start] + new_compile_blocks + s[end:]

# Full-index dispatch: ordinary legacy or exact ztailpacking only.
old = '''        if inode.layout == DATA_COMPRESSED_FULL {
            if self.sb.incompat != 0 {
                return Err(CoreError::UnsupportedFilesystem(
                    "Stage 29 legacy full-index requires non-0padding LZ4 placement",
                ));
            }
            return self.read_full_topology_from_inode(inode);
        }
'''
new = '''        if inode.layout == DATA_COMPRESSED_FULL {
            if self.sb.incompat != 0 && self.sb.incompat != FEATURE_ZTAILPACKING {
                return Err(CoreError::UnsupportedFilesystem(
                    "legacy full-index ordinary mode requires no incompat feature; inline mode requires only ZTAILPACKING",
                ));
            }
            return self.read_full_topology_from_inode(inode);
        }
'''
assert old in s
s = s.replace(old, new, 1)

# Full-index topology reader: validate ordinary vs inline and exclude inline placeholder
# from inode data_word / physical block validation.
old = '''        let map = self.read_full_map_header(&inode)?;
        if map.advise != 0 {
            return Err(CoreError::UnsupportedInode(
                "Stage 29 full-index core requires zero map advice",
            ));
        }
        if map.algorithm != LZ4_ALGORITHM || map.secondary_algorithm != 0 || map.cluster_bits != 0 {
            return Err(CoreError::UnsupportedInode(
                "Stage 29 full-index core requires HEAD1 LZ4 with 4 KiB logical clusters",
            ));
        }

        let entries = self.read_all_full_entries(map.ebase, logical_lclusters)?;
        let eof_plain_clusterofs =
            validate_full_eof_plain_sentinel(&entries, logical_lclusters, inode.size)?;
        let heads = recover_full_data_heads(&entries, logical_lclusters, eof_plain_clusterofs)?;
        if heads.first().map(|head| head.lcn) != Some(0) {
            return Err(CoreError::InvalidFilesystem(
                "first full-index compressed extent does not begin at lcluster zero",
            ));
        }
        if heads.len() != compressed_blocks {
            return Err(CoreError::InvalidFilesystem(
                "full-index encoded physical-block count does not match recovered data HEAD count",
            ));
        }

        validate_full_plain_data_heads(&heads, logical_lclusters, eof_plain_clusterofs)?;
        validate_full_nonheads(&entries, &heads, logical_lclusters, eof_plain_clusterofs)?;
        validate_head_blocks(&heads, self.bytes)?;

        Ok(Topology {
            nid: inode.nid,
            logical_size: inode.size,
            algorithm: map.algorithm,
            advise: map.advise,
            placement: Lz4Placement::LegacyStart,
            logical_lclusters,
            compact_2b_entries: 0,
            eof_plain_clusterofs,
            heads,
        })
'''
new = '''        let map = self.read_full_map_header(&inode)?;
        let inline_mode = map.advise == ADVISE_INLINE_PCLUSTER;
        if map.advise != 0 && !inline_mode {
            return Err(CoreError::UnsupportedInode(
                "full-index core accepts only ordinary or verified INLINE_PCLUSTER map advice",
            ));
        }
        if inline_mode != (self.sb.incompat == FEATURE_ZTAILPACKING) {
            return Err(CoreError::UnsupportedFilesystem(
                "full-index inline map advice and ZTAILPACKING superblock feature disagree",
            ));
        }
        if !inline_mode && map.idata_size != 0 {
            return Err(CoreError::InvalidFilesystem(
                "ordinary full-index map header unexpectedly reports inline data size",
            ));
        }
        if map.algorithm != LZ4_ALGORITHM || map.secondary_algorithm != 0 || map.cluster_bits != 0 {
            return Err(CoreError::UnsupportedInode(
                "full-index core requires HEAD1 LZ4 with 4 KiB logical clusters",
            ));
        }

        let entries = self.read_all_full_entries(map.ebase, logical_lclusters)?;
        let eof_plain_clusterofs =
            validate_full_eof_plain_sentinel(&entries, logical_lclusters, inode.size)?;
        let heads = recover_full_data_heads(&entries, logical_lclusters, eof_plain_clusterofs)?;
        if heads.first().map(|head| head.lcn) != Some(0) {
            return Err(CoreError::InvalidFilesystem(
                "first full-index compressed extent does not begin at lcluster zero",
            ));
        }

        let inline_tail = if inline_mode {
            if map.idata_size == 0 {
                return Err(CoreError::InvalidFilesystem(
                    "inline pcluster map header reports zero encoded tail size",
                ));
            }
            let tail = *heads.last().ok_or(CoreError::InvalidFilesystem(
                "inline pcluster topology contains no tail HEAD",
            ))?;
            if tail.kind != HeadKind::Lz4 {
                return Err(CoreError::UnsupportedInode(
                    "inline pcluster support requires a final LZ4 HEAD1",
                ));
            }
            if heads.len() != compressed_blocks.saturating_add(1) {
                return Err(CoreError::InvalidFilesystem(
                    "inline full-index inode data_word does not match non-inline HEAD count",
                ));
            }
            let index_bytes = u64::try_from(
                logical_lclusters
                    .checked_mul(8)
                    .ok_or(CoreError::ArithmeticOverflow)?,
            )
            .map_err(|_| CoreError::ArithmeticOverflow)?;
            let data_offset = map
                .ebase
                .checked_add(index_bytes)
                .ok_or(CoreError::ArithmeticOverflow)?;
            let capacity = usize::from(map.idata_size);
            ensure_range(
                self.bytes,
                data_offset,
                u64::try_from(capacity).map_err(|_| CoreError::ArithmeticOverflow)?,
            )?;
            let payload_block = data_offset / u64::from(BLOCK_SIZE);
            let header_block = map.header_offset / u64::from(BLOCK_SIZE);
            let block_offset = usize::try_from(data_offset % u64::from(BLOCK_SIZE))
                .map_err(|_| CoreError::ArithmeticOverflow)?;
            if payload_block != header_block
                || block_offset
                    .checked_add(capacity)
                    .ok_or(CoreError::ArithmeticOverflow)?
                    > BLOCK_BYTES
            {
                return Err(CoreError::UnsupportedInode(
                    "Stage 38 inline pcluster requires header and encoded tail inside one metadata block",
                ));
            }
            validate_head_blocks(&heads[..heads.len() - 1], self.bytes)?;
            Some(InlineTail {
                head_lcn: tail.lcn,
                header_offset: map.header_offset,
                data_offset,
                capacity,
            })
        } else {
            if heads.len() != compressed_blocks {
                return Err(CoreError::InvalidFilesystem(
                    "full-index encoded physical-block count does not match recovered data HEAD count",
                ));
            }
            validate_head_blocks(&heads, self.bytes)?;
            None
        };

        validate_full_plain_data_heads(&heads, logical_lclusters, eof_plain_clusterofs)?;
        validate_full_nonheads(&entries, &heads, logical_lclusters, eof_plain_clusterofs)?;

        Ok(Topology {
            nid: inode.nid,
            logical_size: inode.size,
            algorithm: map.algorithm,
            advise: map.advise,
            placement: Lz4Placement::LegacyStart,
            logical_lclusters,
            compact_2b_entries: 0,
            eof_plain_clusterofs,
            inline_tail,
            heads,
        })
'''
assert old in s
s = s.replace(old, new, 1)

# Map header now exposes the verified inline fields.
old = '''        Ok(FullMapHeader {
            ebase,
            advise: read_u16(&header, 4)?,
            algorithm: header[6] & 0x0f,
            secondary_algorithm: header[6] >> 4,
            cluster_bits: header[7],
        })
'''
new = '''        Ok(FullMapHeader {
            header_offset,
            ebase,
            idata_size: read_u16(&header, 2)?,
            advise: read_u16(&header, 4)?,
            algorithm: header[6] & 0x0f,
            secondary_algorithm: header[6] >> 4,
            cluster_bits: header[7],
        })
'''
assert old in s
s = s.replace(old, new, 1)

# Ordinary/compact Topology literals and tests get no inline tail unless explicitly set above.
pattern = re.compile(r'(?<!struct )Topology \{(?:(?!Topology \{).)*?\n(?P<indent>\s*)\}', re.S)
def add_inline_none(m):
    block = m.group(0)
    if 'inline_tail:' in block:
        return block
    marker = 'eof_plain_clusterofs,'
    if marker in block:
        return block.replace(marker, marker + '\n            inline_tail: None,', 1)
    marker = 'eof_plain_clusterofs: None,'
    if marker in block:
        return block.replace(marker, marker + '\n            inline_tail: None,', 1)
    return block
s = pattern.sub(add_inline_none, s)

# Oracle mode stays fail-closed for inline metadata until a real replacement-image oracle is proven.
old = '''    let origin_topology = origin.read_topology(origin_nid)?;
    let replacement_topology = replacement.read_topology(replacement_nid)?;
    validate_compatible_topology(&origin_topology, &replacement_topology)?;
'''
new = '''    let origin_topology = origin.read_topology(origin_nid)?;
    let replacement_topology = replacement.read_topology(replacement_nid)?;
    if origin_topology.inline_tail.is_some() || replacement_topology.inline_tail.is_some() {
        return Err(CoreError::UnsupportedInode(
            "inline pcluster oracle mode is not enabled; use Loom self-encode",
        ));
    }
    validate_compatible_topology(&origin_topology, &replacement_topology)?;
'''
assert old in s
s = s.replace(old, new, 1)

# Compatibility tracks inline presence/shape for non-inline callers and future oracle work.
old = '''    if origin.eof_plain_clusterofs != replacement.eof_plain_clusterofs {
        return Err(CoreError::IncompatibleReplacement(
            "partial-EOF PLAIN sentinel offsets differ",
        ));
    }
'''
new = old + '''    if origin.inline_tail != replacement.inline_tail {
        return Err(CoreError::IncompatibleReplacement(
            "inline pcluster metadata footprint differs",
        ));
    }
'''
assert old in s
s = s.replace(old, new, 1)

p.write_text(s)
