from pathlib import Path

p = Path('crates/loom-erofs/src/compact_core.rs')
s = p.read_text()

# Borrow read-only compiler inputs.
s = s.replace(
    '''        vec![BLOCK_BYTES; replacement_topology.heads.len()],
        origin.sb.feature_compat,
        None,
    )''',
    '''        &vec![BLOCK_BYTES; replacement_topology.heads.len()],
        origin.sb.feature_compat,
        None,
    )''',
    1,
)
s = s.replace(
    '''        encoded_bytes,
        origin.sb.feature_compat,
        fragment_overlay,
    )''',
    '''        &encoded_bytes,
        origin.sb.feature_compat,
        fragment_overlay.as_ref(),
    )''',
    1,
)
s = s.replace(
    '''    encoded_bytes: Vec<usize>,
    feature_compat: u32,
    fragment_overlay: Option<EncodedFragmentOverlay>,''',
    '''    encoded_bytes: &[usize],
    feature_compat: u32,
    fragment_overlay: Option<&EncodedFragmentOverlay>,''',
    1,
)

# Replace the long per-extent loop body with two focused helpers.
start = s.index('    for (index, ((origin_head, replacement_head), encoded)) in topology\n', s.index('fn compile_blocks('))
end_marker = '''        reported_encoded_bytes.push(
            *encoded_bytes
                .get(index)
                .ok_or(CoreError::UnexpectedEndOfStructure)?,
        );
    }
'''
end = s.index(end_marker, start) + len(end_marker)
loop = '''    for (index, ((origin_head, replacement_head), encoded)) in topology
        .heads
        .iter()
        .zip(replacement_heads)
        .zip(encoded_blocks)
        .enumerate()
    {
        let encoded_len = *encoded_bytes
            .get(index)
            .ok_or(CoreError::UnexpectedEndOfStructure)?;
        if let Some(materialized) = materialize_shared_fragment_blocks(
            &mut view,
            fragment_overlay,
            *origin_head,
            &encoded,
            encoded_len,
        )? {
            for (pcluster, physical_encoded_len) in materialized {
                origin_pclusters.push(pcluster);
                replacement_pclusters.push(pcluster);
                head_lclusters.push(origin_head.lcn);
                reported_encoded_bytes.push(physical_encoded_len);
            }
            continue;
        }
        let materialized_pcluster = materialize_direct_block(
            &mut view,
            topology,
            *origin_head,
            &encoded,
            encoded_len,
        )?;
        origin_pclusters.push(materialized_pcluster);
        replacement_pclusters.push(
            if topology
                .fragment_tail
                .is_some_and(|fragment| fragment.head_lcn == replacement_head.lcn)
            {
                materialized_pcluster
            } else {
                replacement_head.pcluster
            },
        );
        head_lclusters.push(origin_head.lcn);
        reported_encoded_bytes.push(encoded_len);
    }
'''
s = s[:start] + loop + s[end:]

# Add helper functions immediately before materialize_inline_tail.
anchor = '\nfn materialize_inline_tail('
assert anchor in s
helpers = r'''
fn materialize_shared_fragment_blocks(
    view: &mut EffectiveBlockStore,
    overlay: Option<&EncodedFragmentOverlay>,
    origin_head: Head,
    encoded: &[u8],
    encoded_len: usize,
) -> Result<Option<Vec<(u64, usize)>>, CoreError> {
    let Some(overlay) = overlay.filter(|overlay| overlay.head_lcn == origin_head.lcn) else {
        return Ok(None);
    };
    if !encoded.is_empty() || encoded_len != 0 {
        return Err(CoreError::InvalidFilesystem(
            "shared partial fragment overlay unexpectedly carried a direct encoded block",
        ));
    }
    let mut materialized = Vec::with_capacity(overlay.blocks.len());
    for block in &overlay.blocks {
        if block.block.len() != BLOCK_BYTES {
            return Err(CoreError::InvalidFilesystem(
                "shared partial fragment encoded block has invalid physical size",
            ));
        }
        view.block_mut(block.pcluster)
            .map_err(CoreError::View)?
            .copy_from_slice(&block.block);
        materialized.push((block.pcluster, block.encoded_len));
    }
    Ok(Some(materialized))
}

fn materialize_direct_block(
    view: &mut EffectiveBlockStore,
    topology: &Topology,
    origin_head: Head,
    encoded: &[u8],
    encoded_len: usize,
) -> Result<u64, CoreError> {
    if let Some(fragment) = topology
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
            .copy_from_slice(encoded);
        return Ok(fragment.pcluster);
    }
    if let Some(inline) = topology
        .inline_tail
        .filter(|inline| inline.head_lcn == origin_head.lcn)
    {
        materialize_inline_tail(view, inline, encoded, encoded_len)?;
        return Ok(origin_head.pcluster);
    }
    if encoded.len() != BLOCK_BYTES {
        return Err(CoreError::InvalidFilesystem(
            "encoded extent does not occupy exactly one physical block",
        ));
    }
    view.block_mut(origin_head.pcluster)
        .map_err(CoreError::View)?
        .copy_from_slice(encoded);
    Ok(origin_head.pcluster)
}

'''
s = s.replace(anchor, '\n' + helpers + 'fn materialize_inline_tail(', 1)

p.write_text(s)
