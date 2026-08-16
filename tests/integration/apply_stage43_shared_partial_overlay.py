from pathlib import Path

core = Path('crates/loom-erofs/src/compact_core.rs')
s = core.read_text()

old = '''#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FragmentTail {
    head_lcn: usize,
    packed_nid: u64,
    pcluster: u64,
}
'''
new = '''#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PackedFragmentExtent {
    logical_start: usize,
    logical_end: usize,
    pcluster: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SharedFragmentOverlay {
    fragment_offset: usize,
    fragment_size: usize,
    extent_count: usize,
    extents: [Option<PackedFragmentExtent>; 2],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FragmentTail {
    head_lcn: usize,
    packed_nid: u64,
    pcluster: u64,
    overlay: Option<SharedFragmentOverlay>,
}

struct EncodedFragmentBlock {
    pcluster: u64,
    block: Vec<u8>,
    encoded_len: usize,
}

struct EncodedFragmentOverlay {
    head_lcn: usize,
    blocks: Vec<EncodedFragmentBlock>,
}
'''
assert old in s
s = s.replace(old, new, 1)

# compile_oracle must pass no overlay materialization.
s = s.replace(
    '''        vec![BLOCK_BYTES; replacement_topology.heads.len()],
        origin.sb.feature_compat,
    )''',
    '''        vec![BLOCK_BYTES; replacement_topology.heads.len()],
        origin.sb.feature_compat,
        None,
    )''',
    1,
)

# compile_lz4 prepares shared partial packed extents before the transaction store opens.
old = '''    let mut encoded_blocks = Vec::with_capacity(topology.heads.len());
    let mut encoded_bytes = Vec::with_capacity(topology.heads.len());
    for (index, head) in topology.heads.iter().enumerate() {'''
new = '''    let mut encoded_blocks = Vec::with_capacity(topology.heads.len());
    let mut encoded_bytes = Vec::with_capacity(topology.heads.len());
    let mut fragment_overlay = None;
    for (index, head) in topology.heads.iter().enumerate() {'''
assert old in s
s = s.replace(old, new, 1)

old = '''        let extent = replacement
            .get(start..end)
            .ok_or(CoreError::UnexpectedEndOfStructure)?;
        let (block, encoded_len) = if topology
            .inline_tail
            .is_some_and(|inline| inline.head_lcn == head.lcn)
        {'''
new = '''        let extent = replacement
            .get(start..end)
            .ok_or(CoreError::UnexpectedEndOfStructure)?;
        if let Some(fragment) = topology.fragment_tail.filter(|fragment| {
            fragment.head_lcn == head.lcn && fragment.overlay.is_some()
        }) {
            if head.kind != HeadKind::Lz4 || fragment_overlay.is_some() {
                return Err(CoreError::InvalidFilesystem(
                    "shared partial fragment overlay disagrees with recovered LZ4 topology",
                ));
            }
            fragment_overlay = Some(encode_shared_fragment_overlay(
                &mut origin,
                fragment,
                extent,
            )?);
            encoded_blocks.push(Vec::new());
            encoded_bytes.push(0);
            continue;
        }
        let (block, encoded_len) = if topology
            .inline_tail
            .is_some_and(|inline| inline.head_lcn == head.lcn)
        {'''
assert old in s
s = s.replace(old, new, 1)

s = s.replace(
    '''        encoded_bytes,
        origin.sb.feature_compat,
    )
}''',
    '''        encoded_bytes,
        origin.sb.feature_compat,
        fragment_overlay,
    )
}''',
    1,
)

# Add overlay encoder before big oracle compiler.
anchor = '\npub(crate) fn compile_big_oracle('
assert anchor in s
overlay_encoder = r'''
fn encode_shared_fragment_overlay(
    origin: &mut Image,
    fragment: FragmentTail,
    replacement_fragment: &[u8],
) -> Result<EncodedFragmentOverlay, CoreError> {
    let overlay = fragment.overlay.ok_or(CoreError::InvalidFilesystem(
        "shared partial fragment overlay disappeared during encoding",
    ))?;
    if replacement_fragment.len() != overlay.fragment_size || overlay.extent_count == 0 {
        return Err(CoreError::InvalidFilesystem(
            "shared partial fragment replacement length disagrees with packed overlay",
        ));
    }
    let fragment_end = overlay
        .fragment_offset
        .checked_add(overlay.fragment_size)
        .ok_or(CoreError::ArithmeticOverflow)?;
    let mut blocks = Vec::with_capacity(overlay.extent_count);
    for extent in overlay.extents.iter().flatten().take(overlay.extent_count) {
        let logical_len = extent
            .logical_end
            .checked_sub(extent.logical_start)
            .ok_or(CoreError::ArithmeticOverflow)?;
        if logical_len == 0 {
            return Err(CoreError::InvalidFilesystem(
                "shared partial packed extent has zero logical length",
            ));
        }
        let raw = origin.read_block(extent.pcluster)?;
        let mut decoded = lz4::decode_partial(&raw, logical_len)
            .map_err(|_| CoreError::CompressionValidationFailed)?;
        let overlap_start = extent.logical_start.max(overlay.fragment_offset);
        let overlap_end = extent.logical_end.min(fragment_end);
        if overlap_start >= overlap_end {
            return Err(CoreError::InvalidFilesystem(
                "shared partial packed extent does not overlap target fragment",
            ));
        }
        let source_start = overlap_start
            .checked_sub(overlay.fragment_offset)
            .ok_or(CoreError::ArithmeticOverflow)?;
        let source_end = overlap_end
            .checked_sub(overlay.fragment_offset)
            .ok_or(CoreError::ArithmeticOverflow)?;
        let target_start = overlap_start
            .checked_sub(extent.logical_start)
            .ok_or(CoreError::ArithmeticOverflow)?;
        let target_end = overlap_end
            .checked_sub(extent.logical_start)
            .ok_or(CoreError::ArithmeticOverflow)?;
        decoded
            .get_mut(target_start..target_end)
            .ok_or(CoreError::UnexpectedEndOfStructure)?
            .copy_from_slice(
                replacement_fragment
                    .get(source_start..source_end)
                    .ok_or(CoreError::UnexpectedEndOfStructure)?,
            );
        let (block, encoded_len) = encode_extent(
            fragment.head_lcn,
            &decoded,
            Lz4Placement::LegacyStart,
        )?;
        blocks.push(EncodedFragmentBlock {
            pcluster: extent.pcluster,
            block,
            encoded_len,
        });
    }
    if blocks.len() != overlay.extent_count {
        return Err(CoreError::InvalidFilesystem(
            "shared partial fragment overlay extent count is inconsistent",
        ));
    }
    Ok(EncodedFragmentOverlay {
        head_lcn: fragment.head_lcn,
        blocks,
    })
}

'''
s = s.replace(anchor, '\n' + overlay_encoder + 'pub(crate) fn compile_big_oracle(', 1)

# compile_blocks accepts and reports multiple packed blocks for one fragment HEAD.
old = '''fn compile_blocks(
    origin_path: &Path,
    topology: &Topology,
    replacement_heads: &[Head],
    encoded_blocks: Vec<Vec<u8>>,
    encoded_bytes: Vec<usize>,
    feature_compat: u32,
) -> Result<CompiledCore, CoreError> {'''
new = '''fn compile_blocks(
    origin_path: &Path,
    topology: &Topology,
    replacement_heads: &[Head],
    encoded_blocks: Vec<Vec<u8>>,
    encoded_bytes: Vec<usize>,
    feature_compat: u32,
    fragment_overlay: Option<EncodedFragmentOverlay>,
) -> Result<CompiledCore, CoreError> {'''
assert old in s
s = s.replace(old, new, 1)

old = '''    let mut origin_pclusters = Vec::with_capacity(topology.heads.len());
    let mut replacement_pclusters = Vec::with_capacity(topology.heads.len());
    let mut head_lclusters = Vec::with_capacity(topology.heads.len());

    for (index, ((origin_head, replacement_head), encoded)) in topology'''
new = '''    let extra_fragment_blocks = fragment_overlay
        .as_ref()
        .map_or(0, |overlay| overlay.blocks.len().saturating_sub(1));
    let physical_capacity = topology
        .heads
        .len()
        .checked_add(extra_fragment_blocks)
        .ok_or(CoreError::ArithmeticOverflow)?;
    let mut origin_pclusters = Vec::with_capacity(physical_capacity);
    let mut replacement_pclusters = Vec::with_capacity(physical_capacity);
    let mut head_lclusters = Vec::with_capacity(physical_capacity);
    let mut reported_encoded_bytes = Vec::with_capacity(physical_capacity);

    for (index, ((origin_head, replacement_head), encoded)) in topology'''
assert old in s
s = s.replace(old, new, 1)

old = '''    {
        let materialized_pcluster = if let Some(fragment) = topology
            .fragment_tail
            .filter(|fragment| fragment.head_lcn == origin_head.lcn)
        {'''
new = '''    {
        if let Some(overlay) = fragment_overlay
            .as_ref()
            .filter(|overlay| overlay.head_lcn == origin_head.lcn)
        {
            if !encoded.is_empty()
                || *encoded_bytes
                    .get(index)
                    .ok_or(CoreError::UnexpectedEndOfStructure)?
                    != 0
            {
                return Err(CoreError::InvalidFilesystem(
                    "shared partial fragment overlay unexpectedly carried a direct encoded block",
                ));
            }
            for block in &overlay.blocks {
                if block.block.len() != BLOCK_BYTES {
                    return Err(CoreError::InvalidFilesystem(
                        "shared partial fragment encoded block has invalid physical size",
                    ));
                }
                view.block_mut(block.pcluster)
                    .map_err(CoreError::View)?
                    .copy_from_slice(&block.block);
                origin_pclusters.push(block.pcluster);
                replacement_pclusters.push(block.pcluster);
                head_lclusters.push(origin_head.lcn);
                reported_encoded_bytes.push(block.encoded_len);
            }
            continue;
        }
        let materialized_pcluster = if let Some(fragment) = topology
            .fragment_tail
            .filter(|fragment| fragment.head_lcn == origin_head.lcn)
        {'''
assert old in s
s = s.replace(old, new, 1)

old = '''        head_lclusters.push(origin_head.lcn);
    }

    if topology.inline_tail.is_some()'''
new = '''        head_lclusters.push(origin_head.lcn);
        reported_encoded_bytes.push(
            *encoded_bytes
                .get(index)
                .ok_or(CoreError::UnexpectedEndOfStructure)?,
        );
    }

    if topology.inline_tail.is_some()'''
assert old in s
s = s.replace(old, new, 1)

s = s.replace(
    '''    if compiled.shadow_blocks > topology.heads.len() {
        return Err(CoreError::InvalidFilesystem(
            "compact shadow block count exceeds recovered extent footprint",
        ));
    }''',
    '''    if compiled.shadow_blocks > physical_capacity {
        return Err(CoreError::InvalidFilesystem(
            "compact shadow block count exceeds recovered physical extent footprint",
        ));
    }''',
    1,
)
s = s.replace(
    '''        encoded_bytes,
        logical_lclusters: topology.logical_lclusters,''',
    '''        encoded_bytes: reported_encoded_bytes,
        logical_lclusters: topology.logical_lclusters,''',
    1,
)

# Direct fragment construction records no overlay; nonaligned shared partial routes to Stage43 resolver.
old = '''        let pcluster = if fragment_offset == 0 && packed_size == fragment_size {
            self.recover_stage39_packed_pcluster(packed_nid, fragment_size)?
        } else {
            self.recover_stage40_shared_packed_pcluster(packed_nid, fragment_offset, fragment_size)?
        };
        Ok(FragmentTail {
            head_lcn: tail.lcn,
            packed_nid,
            pcluster,
        })'''
new = '''        let aligned = fragment_offset % u64::from(BLOCK_SIZE) == 0
            && fragment_size % u64::from(BLOCK_SIZE) == 0;
        let (pcluster, overlay) = if fragment_offset == 0 && packed_size == fragment_size {
            (
                self.recover_stage39_packed_pcluster(packed_nid, fragment_size)?,
                None,
            )
        } else if aligned {
            (
                self.recover_stage40_shared_packed_pcluster(
                    packed_nid,
                    fragment_offset,
                    fragment_size,
                )?,
                None,
            )
        } else {
            let overlay = self.recover_stage43_shared_partial_overlay(
                packed_nid,
                fragment_offset,
                fragment_size,
            )?;
            let first = overlay.extents[0].ok_or(CoreError::InvalidFilesystem(
                "shared partial fragment overlay contains no packed extent",
            ))?;
            (first.pcluster, Some(overlay))
        };
        Ok(FragmentTail {
            head_lcn: tail.lcn,
            packed_nid,
            pcluster,
            overlay,
        })'''
assert old in s
s = s.replace(old, new, 1)

# Add strict Stage43 overlap resolver before Stage40 resolver.
anchor = '''    fn recover_stage40_shared_packed_pcluster(
'''
assert anchor in s
resolver = r'''    fn recover_stage43_shared_partial_overlay(
        &mut self,
        packed_nid: u64,
        fragment_offset: u64,
        fragment_size: u64,
    ) -> Result<SharedFragmentOverlay, CoreError> {
        let block = u64::from(BLOCK_SIZE);
        let fragment_end = fragment_offset
            .checked_add(fragment_size)
            .ok_or(CoreError::ArithmeticOverflow)?;
        let packed = self.read_inode(packed_nid)?;
        if packed.file_type() != MODE_REGULAR || packed.layout != DATA_COMPRESSED_FULL {
            return Err(CoreError::UnsupportedInode(
                "Stage 43 requires a full-index regular packed inode",
            ));
        }
        if fragment_size == 0 || fragment_end > packed.size {
            return Err(CoreError::UnsupportedInode(
                "Stage 43 shared partial fragment lies outside the packed inode",
            ));
        }
        let packed_lclusters_u64 = div_ceil(packed.size, block)?;
        let packed_lclusters =
            usize::try_from(packed_lclusters_u64).map_err(|_| CoreError::ArithmeticOverflow)?;
        let packed_blocks =
            usize::try_from(packed.data_word).map_err(|_| CoreError::ArithmeticOverflow)?;
        if packed_lclusters < 2 || packed_blocks < 2 {
            return Err(CoreError::UnsupportedInode(
                "Stage 43 requires a genuinely shared packed inode",
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
                "Stage 43 packed inode requires exact interlaced HEAD1 LZ4 full-index topology",
            ));
        }
        let packed_entries = self.read_all_full_entries(packed_map.ebase, packed_lclusters)?;
        let packed_eof =
            validate_full_eof_plain_sentinel(&packed_entries, packed_lclusters, packed.size)?;
        let packed_heads = recover_full_data_heads(&packed_entries, packed_lclusters, packed_eof)?;
        if packed_heads.len() != packed_blocks
            || packed_heads.iter().any(|head| head.kind != HeadKind::Lz4)
        {
            return Err(CoreError::UnsupportedInode(
                "Stage 43 packed inode must contain one physical HEAD1 LZ4 block per packed extent",
            ));
        }
        validate_full_nonheads(&packed_entries, &packed_heads, packed_lclusters, packed_eof)?;
        validate_head_blocks(&packed_heads, self.bytes)?;

        let fragment_offset =
            usize::try_from(fragment_offset).map_err(|_| CoreError::ArithmeticOverflow)?;
        let fragment_end =
            usize::try_from(fragment_end).map_err(|_| CoreError::ArithmeticOverflow)?;
        let fragment_size =
            usize::try_from(fragment_size).map_err(|_| CoreError::ArithmeticOverflow)?;
        let packed_size = usize::try_from(packed.size).map_err(|_| CoreError::ArithmeticOverflow)?;
        let mut extents = [None, None];
        let mut extent_count = 0_usize;
        for (index, head) in packed_heads.iter().enumerate() {
            let logical_start = head
                .lcn
                .checked_mul(BLOCK_BYTES)
                .ok_or(CoreError::ArithmeticOverflow)?;
            let logical_end = packed_heads
                .get(index + 1)
                .map_or(packed_size, |next| next.lcn.saturating_mul(BLOCK_BYTES));
            if logical_start >= logical_end {
                return Err(CoreError::InvalidFilesystem(
                    "Stage 43 packed HEAD extents are not strictly increasing",
                ));
            }
            if logical_start < fragment_end && fragment_offset < logical_end {
                if extent_count >= extents.len() {
                    return Err(CoreError::UnsupportedInode(
                        "Stage 43 shared partial fragment spans more than two packed extents",
                    ));
                }
                extents[extent_count] = Some(PackedFragmentExtent {
                    logical_start,
                    logical_end,
                    pcluster: head.pcluster,
                });
                extent_count += 1;
            }
        }
        if extent_count == 0 {
            return Err(CoreError::InvalidFilesystem(
                "Stage 43 shared partial fragment overlaps no packed extent",
            ));
        }
        Ok(SharedFragmentOverlay {
            fragment_offset,
            fragment_size,
            extent_count,
            extents,
        })
    }

'''
s = s.replace(anchor, resolver + anchor, 1)

core.write_text(s)

codec = Path('crates/loom-erofs/src/multi_lz4.rs')
c = codec.read_text()
anchor = '''pub(crate) fn decode_0padding(pcluster: &[u8], expected: usize) -> Result<Vec<u8>, CodecError> {'''
assert anchor in c
partial = r'''pub(crate) fn decode_partial(encoded: &[u8], expected: usize) -> Result<Vec<u8>, CodecError> {
    let mut input_pos = 0_usize;
    let mut output = Vec::with_capacity(expected);
    while input_pos < encoded.len() && output.len() < expected {
        let token = encoded[input_pos];
        input_pos += 1;
        let mut literal_len = usize::from(token >> 4);
        if literal_len == 15 {
            literal_len = literal_len
                .checked_add(read_length(encoded, &mut input_pos)?)
                .ok_or(CodecError::Overflow)?;
        }
        let literal_end = input_pos
            .checked_add(literal_len)
            .ok_or(CodecError::Overflow)?;
        let literals = encoded
            .get(input_pos..literal_end)
            .ok_or(CodecError::InvalidBlock)?;
        if output.len().saturating_add(literals.len()) > expected {
            return Err(CodecError::InvalidBlock);
        }
        output.extend_from_slice(literals);
        input_pos = literal_end;
        if output.len() == expected {
            return Ok(output);
        }
        if input_pos == encoded.len() {
            break;
        }
        let offset_end = input_pos.checked_add(2).ok_or(CodecError::Overflow)?;
        let raw_offset: [u8; 2] = encoded
            .get(input_pos..offset_end)
            .ok_or(CodecError::InvalidBlock)?
            .try_into()
            .map_err(|_| CodecError::InvalidBlock)?;
        input_pos = offset_end;
        let offset = usize::from(u16::from_le_bytes(raw_offset));
        if offset == 0 || offset > output.len() {
            return Err(CodecError::InvalidBlock);
        }
        let mut match_len = usize::from(token & 0x0f) + MIN_MATCH;
        if token & 0x0f == 15 {
            match_len = match_len
                .checked_add(read_length(encoded, &mut input_pos)?)
                .ok_or(CodecError::Overflow)?;
        }
        if output.len().saturating_add(match_len) > expected {
            return Err(CodecError::InvalidBlock);
        }
        for _ in 0..match_len {
            let source = output
                .len()
                .checked_sub(offset)
                .ok_or(CodecError::InvalidBlock)?;
            let byte = *output.get(source).ok_or(CodecError::InvalidBlock)?;
            output.push(byte);
        }
        if output.len() == expected {
            return Ok(output);
        }
    }
    Err(CodecError::InvalidBlock)
}

'''
c = c.replace(anchor, partial + anchor, 1)

# Focused padded legacy-stream regression test.
test_anchor = '''    #[test]
    fn random_payload_exceeds_one_block() {'''
assert test_anchor in c
unit = '''    #[test]
    fn partial_decode_ignores_legacy_physical_padding() {
        let input = vec![b'P'; 32768];
        let encoded = encode(&input).unwrap();
        let mut block = vec![0_u8; 4096];
        block[..encoded.len()].copy_from_slice(&encoded);
        assert_eq!(decode_partial(&block, input.len()).unwrap(), input);
    }

'''
c = c.replace(test_anchor, unit + test_anchor, 1)
codec.write_text(c)
