from pathlib import Path

p = Path('crates/loom-erofs/src/compact_core.rs')
s = p.read_text()

old = '''        if fragment_offset != 0 {
            return Err(CoreError::UnsupportedInode(
                "Stage 39 supports only fragment offset zero",
            ));
        }
        let fragment_start = u64::try_from(tail.lcn)'''
new = '''        let fragment_start = u64::try_from(tail.lcn)'''
assert old in s
s = s.replace(old, new, 1)

old = '''        let pcluster = self.recover_stage39_packed_pcluster(packed_nid, fragment_size)?;
        Ok(FragmentTail {'''
new = '''        let pcluster = if fragment_offset == 0 {
            self.recover_stage39_packed_pcluster(packed_nid, fragment_size)?
        } else {
            self.recover_stage40_shared_packed_pcluster(
                packed_nid,
                fragment_offset,
                fragment_size,
            )?
        };
        Ok(FragmentTail {'''
assert old in s
s = s.replace(old, new, 1)

anchor = '''    fn recover_full_inline_tail(
'''
assert anchor in s
helper = r'''    fn recover_stage40_shared_packed_pcluster(
        &mut self,
        packed_nid: u64,
        fragment_offset: u64,
        fragment_size: u64,
    ) -> Result<u64, CoreError> {
        let block = u64::from(BLOCK_SIZE);
        if fragment_offset == 0
            || fragment_offset % block != 0
            || fragment_size == 0
            || fragment_size % block != 0
        {
            return Err(CoreError::UnsupportedInode(
                "Stage 40 requires a non-zero block-aligned shared fragment extent",
            ));
        }
        let packed = self.read_inode(packed_nid)?;
        if packed.file_type() != MODE_REGULAR || packed.layout != DATA_COMPRESSED_FULL {
            return Err(CoreError::UnsupportedInode(
                "Stage 40 requires a full-index regular packed inode",
            ));
        }
        let fragment_end = fragment_offset
            .checked_add(fragment_size)
            .ok_or(CoreError::ArithmeticOverflow)?;
        if fragment_end > packed.size || packed.size % block != 0 {
            return Err(CoreError::UnsupportedInode(
                "Stage 40 shared fragment lies outside an aligned packed inode",
            ));
        }
        let packed_lclusters = usize::try_from(packed.size / block)
            .map_err(|_| CoreError::ArithmeticOverflow)?;
        let packed_blocks =
            usize::try_from(packed.data_word).map_err(|_| CoreError::ArithmeticOverflow)?;
        if packed_lclusters < 2 || packed_blocks < 2 {
            return Err(CoreError::UnsupportedInode(
                "Stage 40 requires a genuinely shared multi-extent packed inode",
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
                "Stage 40 packed inode requires exact interlaced HEAD1 LZ4 full-index topology",
            ));
        }
        let packed_entries = self.read_all_full_entries(packed_map.ebase, packed_lclusters)?;
        if validate_full_eof_plain_sentinel(&packed_entries, packed_lclusters, packed.size)?.is_some()
        {
            return Err(CoreError::UnsupportedInode(
                "Stage 40 packed inode must be logical-cluster aligned",
            ));
        }
        let packed_heads = recover_full_data_heads(&packed_entries, packed_lclusters, None)?;
        if packed_heads.len() != packed_blocks
            || packed_heads.iter().any(|head| head.kind != HeadKind::Lz4)
        {
            return Err(CoreError::UnsupportedInode(
                "Stage 40 packed inode must contain one physical HEAD1 LZ4 block per packed extent",
            ));
        }
        validate_full_nonheads(&packed_entries, &packed_heads, packed_lclusters, None)?;
        validate_head_blocks(&packed_heads, self.bytes)?;

        let start_lcn = usize::try_from(fragment_offset / block)
            .map_err(|_| CoreError::ArithmeticOverflow)?;
        let end_lcn =
            usize::try_from(fragment_end / block).map_err(|_| CoreError::ArithmeticOverflow)?;
        let head_index = packed_heads
            .iter()
            .position(|head| head.lcn == start_lcn)
            .ok_or(CoreError::UnsupportedInode(
                "Stage 40 shared fragment does not begin at a packed HEAD boundary",
            ))?;
        let extent_end = packed_heads
            .get(head_index + 1)
            .map_or(packed_lclusters, |head| head.lcn);
        if extent_end != end_lcn {
            return Err(CoreError::UnsupportedInode(
                "Stage 40 shared fragment must exactly occupy one independent packed HEAD extent",
            ));
        }
        Ok(packed_heads[head_index].pcluster)
    }

'''
s = s.replace(anchor, helper + anchor, 1)
p.write_text(s)
