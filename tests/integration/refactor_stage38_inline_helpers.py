from pathlib import Path

p = Path('crates/loom-erofs/src/compact_core.rs')
s = p.read_text()

# Shrink compile_blocks by moving the metadata mutation into a focused helper.
old = '''        if let Some(inline) = topology
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
'''
new = '''        if let Some(inline) = topology
            .inline_tail
            .filter(|inline| inline.head_lcn == origin_head.lcn)
        {
            let encoded_len = *encoded_bytes
                .get(index)
                .ok_or(CoreError::UnexpectedEndOfStructure)?;
            materialize_inline_tail(&mut view, inline, &encoded, encoded_len)?;
        } else {
'''
assert old in s
s = s.replace(old, new, 1)

anchor = '\nfn compile_big_spans('
assert anchor in s
helper = r'''
fn materialize_inline_tail(
    view: &mut EffectiveBlockStore,
    inline: InlineTail,
    encoded: &[u8],
    encoded_len: usize,
) -> Result<(), CoreError> {
    if encoded.len() != inline.capacity || encoded_len == 0 || encoded_len > inline.capacity {
        return Err(CoreError::InvalidFilesystem(
            "inline pcluster encoded bytes disagree with fixed metadata capacity",
        ));
    }
    let encoded_len_u16 =
        u16::try_from(encoded_len).map_err(|_| CoreError::ArithmeticOverflow)?;
    let metadata_block = inline.data_offset / u64::from(BLOCK_SIZE);
    let header_block = inline.header_offset / u64::from(BLOCK_SIZE);
    if header_block != metadata_block {
        return Err(CoreError::InvalidFilesystem(
            "inline pcluster header and payload moved into different metadata blocks",
        ));
    }
    let block_offset = usize::try_from(inline.data_offset % u64::from(BLOCK_SIZE))
        .map_err(|_| CoreError::ArithmeticOverflow)?;
    let end = block_offset
        .checked_add(inline.capacity)
        .ok_or(CoreError::ArithmeticOverflow)?;
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
    let block = view.block_mut(metadata_block).map_err(CoreError::View)?;
    block
        .get_mut(block_offset..end)
        .ok_or(CoreError::UnexpectedEndOfStructure)?
        .copy_from_slice(encoded);
    block
        .get_mut(size_offset..size_end)
        .ok_or(CoreError::UnexpectedEndOfStructure)?
        .copy_from_slice(&encoded_len_u16.to_le_bytes());
    Ok(())
}

'''
s = s.replace(anchor, '\n' + helper + 'fn compile_big_spans(', 1)

# Shrink read_full_topology_from_inode by extracting inline-tail validation/recovery.
start_marker = '''        let inline_tail = if inline_mode {
            if map.idata_size == 0 {
'''
start = s.index(start_marker)
end_marker = '''        validate_full_plain_data_heads(&heads, logical_lclusters, eof_plain_clusterofs)?;
'''
end = s.index(end_marker, start)
replacement = '''        let inline_tail = self.recover_full_inline_tail(
            &map,
            &heads,
            compressed_blocks,
            logical_lclusters,
            inline_mode,
        )?;

'''
s = s[:start] + replacement + s[end:]

anchor = '''    fn read_full_map_header(&mut self, inode: &Inode) -> Result<FullMapHeader, CoreError> {'''
assert anchor in s
method = r'''    fn recover_full_inline_tail(
        &self,
        map: &FullMapHeader,
        heads: &[Head],
        compressed_blocks: usize,
        logical_lclusters: usize,
        inline_mode: bool,
    ) -> Result<Option<InlineTail>, CoreError> {
        if !inline_mode {
            if heads.len() != compressed_blocks {
                return Err(CoreError::InvalidFilesystem(
                    "full-index encoded physical-block count does not match recovered data HEAD count",
                ));
            }
            validate_head_blocks(heads, self.bytes)?;
            return Ok(None);
        }
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
        Ok(Some(InlineTail {
            head_lcn: tail.lcn,
            header_offset: map.header_offset,
            data_offset,
            capacity,
        }))
    }

'''
s = s.replace(anchor, method + anchor, 1)

p.write_text(s)
