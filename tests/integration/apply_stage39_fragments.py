from pathlib import Path
import re

p = Path('crates/loom-erofs/src/compact_core.rs')
s = p.read_text()

# Constants and superblock feature coverage.
s = s.replace(
    'const ADVISE_INLINE_PCLUSTER: u16 = 0x0008;\n',
    'const ADVISE_INLINE_PCLUSTER: u16 = 0x0008;\nconst ADVISE_INTERLACED_PCLUSTER: u16 = 0x0010;\nconst ADVISE_FRAGMENT_PCLUSTER: u16 = 0x0020;\nconst FRAGMENT_ADVISE: u16 = ADVISE_INTERLACED_PCLUSTER | ADVISE_FRAGMENT_PCLUSTER;\n',
    1,
)
s = s.replace(
    'const FEATURE_ZTAILPACKING: u32 = 0x0000_0010;\n',
    'const FEATURE_ZTAILPACKING: u32 = 0x0000_0010;\nconst FEATURE_FRAGMENTS: u32 = 0x0000_0020;\n',
    1,
)
s = s.replace(
    'const SUPPORTED_INCOMPAT: u32 = FEATURE_LZ4_0PADDING | FEATURE_BIG_PCLUSTER | FEATURE_ZTAILPACKING;\n',
    'const SUPPORTED_INCOMPAT: u32 =\n    FEATURE_LZ4_0PADDING | FEATURE_BIG_PCLUSTER | FEATURE_ZTAILPACKING | FEATURE_FRAGMENTS;\n',
    1,
)

# Fragment topology is a logical target HEAD backed by the special packed inode.
anchor = '''#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InlineTail {
    head_lcn: usize,
    header_offset: u64,
    data_offset: u64,
    capacity: usize,
}
'''
assert anchor in s
s = s.replace(
    anchor,
    anchor + '''
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FragmentTail {
    head_lcn: usize,
    packed_nid: u64,
    pcluster: u64,
}
''',
    1,
)
s = s.replace(
    '    inline_tail: Option<InlineTail>,\n    heads: Vec<Head>,\n',
    '    inline_tail: Option<InlineTail>,\n    fragment_tail: Option<FragmentTail>,\n    heads: Vec<Head>,\n',
    1,
)

# Preserve the special packed inode NID and the full 32-bit fragment offset low word.
s = s.replace(
    '''struct Superblock {
    root_nid: u64,
    meta_block: u64,
    feature_compat: u32,
    incompat: u32,
}''',
    '''struct Superblock {
    root_nid: u64,
    meta_block: u64,
    packed_nid: u64,
    feature_compat: u32,
    incompat: u32,
}''',
    1,
)
s = s.replace(
    '''struct FullMapHeader {
    header_offset: u64,
    ebase: u64,
    idata_size: u16,''',
    '''struct FullMapHeader {
    header_offset: u64,
    ebase: u64,
    fragment_offset_low: u32,
    idata_size: u16,''',
    1,
)

# Oracle remains fail-closed for cross-inode fragment topology.
s = s.replace(
    '''    if origin_topology.inline_tail.is_some() || replacement_topology.inline_tail.is_some() {
        return Err(CoreError::UnsupportedInode(
            "inline pcluster oracle mode is not enabled; use Loom self-encode",
        ));
    }''',
    '''    if origin_topology.inline_tail.is_some()
        || replacement_topology.inline_tail.is_some()
        || origin_topology.fragment_tail.is_some()
        || replacement_topology.fragment_tail.is_some()
    {
        return Err(CoreError::UnsupportedInode(
            "inline/fragment pcluster oracle mode is not enabled; use Loom self-encode",
        ));
    }''',
    1,
)

# Self-encode dispatch can now route the exact FRAGMENTS-only filesystem.
s = s.replace(
    '        0 | FEATURE_LZ4_0PADDING | FEATURE_ZTAILPACKING => {\n',
    '        0 | FEATURE_LZ4_0PADDING | FEATURE_ZTAILPACKING | FEATURE_FRAGMENTS => {\n',
    1,
)

# Materialize a fragment HEAD into the packed inode pcluster rather than target word=fragmentoff_hi.
old = '''        if let Some(inline) = topology
            .inline_tail
            .filter(|inline| inline.head_lcn == origin_head.lcn)
        {
            let encoded_len = *encoded_bytes
                .get(index)
                .ok_or(CoreError::UnexpectedEndOfStructure)?;
            materialize_inline_tail(&mut view, inline, &encoded, encoded_len)?;
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
        head_lclusters.push(origin_head.lcn);'''
new = '''        let materialized_pcluster = if let Some(fragment) = topology
            .fragment_tail
            .filter(|fragment| fragment.head_lcn == origin_head.lcn)
        {
            if encoded.len() != BLOCK_BYTES || origin_head.kind != HeadKind::Lz4 {
                return Err(CoreError::InvalidFilesystem(
                    "fragment replacement must encode as exactly one LZ4 packed-inode pcluster",
                ));
            }
            view.block_mut(fragment.pcluster)
                .map_err(CoreError::View)?
                .copy_from_slice(&encoded);
            fragment.pcluster
        } else if let Some(inline) = topology
            .inline_tail
            .filter(|inline| inline.head_lcn == origin_head.lcn)
        {
            let encoded_len = *encoded_bytes
                .get(index)
                .ok_or(CoreError::UnexpectedEndOfStructure)?;
            materialize_inline_tail(&mut view, inline, &encoded, encoded_len)?;
            origin_head.pcluster
        } else {
            if encoded.len() != BLOCK_BYTES {
                return Err(CoreError::InvalidFilesystem(
                    "encoded extent does not occupy exactly one physical block",
                ));
            }
            view.block_mut(origin_head.pcluster)
                .map_err(CoreError::View)?
                .copy_from_slice(&encoded);
            origin_head.pcluster
        };
        origin_pclusters.push(materialized_pcluster);
        replacement_pclusters.push(if topology.fragment_tail.is_some_and(|fragment| {
            fragment.head_lcn == replacement_head.lcn
        }) {
            materialized_pcluster
        } else {
            replacement_head.pcluster
        });
        head_lclusters.push(origin_head.lcn);'''
assert old in s
s = s.replace(old, new, 1)

# Read packed_nid from the on-disk superblock.
s = s.replace(
    '''        root_nid: u64::from(read_u16(&raw, 0x0e)?),
        meta_block: u64::from(read_u32(&raw, 0x28)?),
        feature_compat: read_u32(&raw, 0x08)?,''',
    '''        root_nid: u64::from(read_u16(&raw, 0x0e)?),
        meta_block: u64::from(read_u32(&raw, 0x28)?),
        packed_nid: read_u64(&raw, 0x60)?,
        feature_compat: read_u32(&raw, 0x08)?,''',
    1,
)

# Allow the full-index reader to enter only the real FRAGMENTS feature mode.
s = s.replace(
    '''            if self.sb.incompat != 0 && self.sb.incompat != FEATURE_ZTAILPACKING {
                return Err(CoreError::UnsupportedFilesystem(
                    "legacy full-index ordinary mode requires no incompat feature; inline mode requires only ZTAILPACKING",
                ));
            }''',
    '''            if self.sb.incompat != 0
                && self.sb.incompat != FEATURE_ZTAILPACKING
                && self.sb.incompat != FEATURE_FRAGMENTS
            {
                return Err(CoreError::UnsupportedFilesystem(
                    "legacy full-index mode supports only ordinary, ZTAILPACKING, or the verified FRAGMENTS feature",
                ));
            }''',
    1,
)

# Replace full-index mode selection / consistency checks.
old = '''        let map = self.read_full_map_header(&inode)?;
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
        }'''
new = '''        let map = self.read_full_map_header(&inode)?;
        let inline_mode = map.advise == ADVISE_INLINE_PCLUSTER;
        let fragment_mode = map.advise == FRAGMENT_ADVISE;
        if map.advise != 0 && !inline_mode && !fragment_mode {
            return Err(CoreError::UnsupportedInode(
                "full-index core accepts only ordinary, verified INLINE_PCLUSTER, or exact FRAGMENT+INTERLACED advice",
            ));
        }
        let feature_matches = match self.sb.incompat {
            0 => !inline_mode && !fragment_mode,
            FEATURE_ZTAILPACKING => inline_mode && !fragment_mode,
            FEATURE_FRAGMENTS => fragment_mode && !inline_mode,
            _ => false,
        };
        if !feature_matches {
            return Err(CoreError::UnsupportedFilesystem(
                "full-index map advice and superblock incompatible feature disagree",
            ));
        }
        if !inline_mode && !fragment_mode && map.idata_size != 0 {
            return Err(CoreError::InvalidFilesystem(
                "ordinary full-index map header unexpectedly reports inline data size",
            ));
        }'''
assert old in s
s = s.replace(old, new, 1)

# Fragment and inline tails are mutually exclusive; fragment validation also resolves packed inode pcluster.
old = '''        let inline_tail = self.recover_full_inline_tail(
            &map,
            &heads,
            compressed_blocks,
            logical_lclusters,
            inline_mode,
        )?;

        validate_full_plain_data_heads(&heads, logical_lclusters, eof_plain_clusterofs)?;'''
new = '''        let inline_tail = if fragment_mode {
            None
        } else {
            self.recover_full_inline_tail(
                &map,
                &heads,
                compressed_blocks,
                logical_lclusters,
                inline_mode,
            )?
        };
        let fragment_tail = if fragment_mode {
            Some(self.recover_full_fragment_tail(
                &map,
                &heads,
                compressed_blocks,
                logical_lclusters,
                inode.size,
            )?)
        } else {
            None
        };

        validate_full_plain_data_heads(&heads, logical_lclusters, eof_plain_clusterofs)?;'''
assert old in s
s = s.replace(old, new, 1)

# Add fragment_tail to the full topology literal.
s = s.replace(
    '''            eof_plain_clusterofs,
            inline_tail,
            heads,
        })''',
    '''            eof_plain_clusterofs,
            inline_tail,
            fragment_tail,
            heads,
        })''',
    1,
)

# Add a strict packed-inode resolver before the inline-tail resolver.
anchor = '    fn recover_full_inline_tail(\n'
assert anchor in s
helper = r'''    fn recover_full_fragment_tail(
        &mut self,
        map: &FullMapHeader,
        heads: &[Head],
        compressed_blocks: usize,
        logical_lclusters: usize,
        logical_size: u64,
    ) -> Result<FragmentTail, CoreError> {
        if heads.len() != compressed_blocks.saturating_add(1) {
            return Err(CoreError::InvalidFilesystem(
                "fragment full-index inode data_word does not match non-fragment HEAD count",
            ));
        }
        let tail = *heads.last().ok_or(CoreError::InvalidFilesystem(
            "fragment topology contains no tail HEAD",
        ))?;
        if tail.kind != HeadKind::Lz4 || tail.lcn + 1 >= logical_lclusters {
            return Err(CoreError::UnsupportedInode(
                "Stage 39 requires a multi-lcluster final LZ4 fragment HEAD",
            ));
        }
        let fragment_offset = tail
            .pcluster
            .checked_shl(32)
            .and_then(|high| high.checked_add(u64::from(map.fragment_offset_low)))
            .ok_or(CoreError::ArithmeticOverflow)?;
        if fragment_offset != 0 {
            return Err(CoreError::UnsupportedInode(
                "Stage 39 supports only fragment offset zero",
            ));
        }
        let fragment_start = u64::try_from(tail.lcn)
            .map_err(|_| CoreError::ArithmeticOverflow)?
            .checked_mul(u64::from(BLOCK_SIZE))
            .ok_or(CoreError::ArithmeticOverflow)?;
        let fragment_size = logical_size
            .checked_sub(fragment_start)
            .ok_or(CoreError::ArithmeticOverflow)?;
        if fragment_size == 0 || fragment_size % u64::from(BLOCK_SIZE) != 0 {
            return Err(CoreError::UnsupportedInode(
                "Stage 39 requires an aligned non-empty fragment tail",
            ));
        }
        validate_head_blocks(&heads[..heads.len() - 1], self.bytes)?;

        let packed_nid = self.sb.packed_nid;
        if packed_nid == 0 {
            return Err(CoreError::InvalidFilesystem(
                "FRAGMENTS feature is enabled without a packed inode",
            ));
        }
        let packed = self.read_inode(packed_nid)?;
        if packed.file_type() != MODE_REGULAR || packed.layout != DATA_COMPRESSED_FULL {
            return Err(CoreError::UnsupportedInode(
                "Stage 39 requires a full-index regular packed inode",
            ));
        }
        if packed.size != fragment_size {
            return Err(CoreError::UnsupportedInode(
                "Stage 39 requires the target fragment to occupy the entire packed inode",
            ));
        }
        if packed.data_word != 1 {
            return Err(CoreError::UnsupportedInode(
                "Stage 39 requires a single-physical-pcluster packed inode",
            ));
        }
        let packed_lclusters_u64 = div_ceil(packed.size, u64::from(BLOCK_SIZE))?;
        let packed_lclusters =
            usize::try_from(packed_lclusters_u64).map_err(|_| CoreError::ArithmeticOverflow)?;
        if packed_lclusters < 2 {
            return Err(CoreError::UnsupportedInode(
                "Stage 39 packed inode must span at least two logical clusters",
            ));
        }
        let packed_map = self.read_full_map_header(&packed)?;
        if packed_map.advise != ADVISE_INTERLACED_PCLUSTER
            || packed_map.fragment_offset_low != 0
            || packed_map.idata_size != 0
            || packed_map.algorithm != LZ4_ALGORITHM
            || packed_map.secondary_algorithm != 0
            || packed_map.cluster_bits != 0
        {
            return Err(CoreError::UnsupportedInode(
                "Stage 39 packed inode requires exact interlaced HEAD1 LZ4 full-index topology",
            ));
        }
        let packed_entries = self.read_all_full_entries(packed_map.ebase, packed_lclusters)?;
        let packed_eof = validate_full_eof_plain_sentinel(
            &packed_entries,
            packed_lclusters,
            packed.size,
        )?;
        if packed_eof.is_some() {
            return Err(CoreError::UnsupportedInode(
                "Stage 39 packed inode must be logical-cluster aligned",
            ));
        }
        let packed_heads = recover_full_data_heads(&packed_entries, packed_lclusters, None)?;
        if packed_heads.len() != 1
            || packed_heads[0].lcn != 0
            || packed_heads[0].kind != HeadKind::Lz4
        {
            return Err(CoreError::UnsupportedInode(
                "Stage 39 packed inode must contain exactly one LZ4 HEAD at lcluster zero",
            ));
        }
        validate_full_nonheads(&packed_entries, &packed_heads, packed_lclusters, None)?;
        validate_head_blocks(&packed_heads, self.bytes)?;
        Ok(FragmentTail {
            head_lcn: tail.lcn,
            packed_nid,
            pcluster: packed_heads[0].pcluster,
        })
    }

'''
s = s.replace(anchor, helper + anchor, 1)

# Full map header exposes the union's entire low fragment offset.
s = s.replace(
    '''        Ok(FullMapHeader {
            header_offset,
            ebase,
            idata_size: read_u16(&header, 2)?,''',
    '''        Ok(FullMapHeader {
            header_offset,
            ebase,
            fragment_offset_low: read_u32(&header, 0)?,
            idata_size: read_u16(&header, 2)?,''',
    1,
)

# Every non-fragment topology literal must explicitly carry None. Use line-level insertion.
s = re.sub(
    r'(?m)^(\s*)inline_tail: None,\n(?!\1fragment_tail:)',
    r'\1inline_tail: None,\n\1fragment_tail: None,\n',
    s,
)

# Keep topology compatibility explicit even though fragment oracle mode is rejected earlier.
old = '''        || origin.eof_plain_clusterofs != replacement.eof_plain_clusterofs
        || origin.heads.len() != replacement.heads.len()'''
if old in s:
    s = s.replace(
        old,
        '''        || origin.eof_plain_clusterofs != replacement.eof_plain_clusterofs
        || origin.fragment_tail != replacement.fragment_tail
        || origin.heads.len() != replacement.heads.len()''',
        1,
    )

p.write_text(s)
