from pathlib import Path

p = Path('crates/loom-erofs/src/compact_core.rs')
s = p.read_text()

start_marker = '''        let packed_nid = self.sb.packed_nid;
        if packed_nid == 0 {'''
end_marker = '''        Ok(FragmentTail {
            head_lcn: tail.lcn,
            packed_nid,
            pcluster: packed_heads[0].pcluster,
        })
    }

'''
start = s.index(start_marker)
end = s.index(end_marker, start) + len(end_marker)
replacement = '''        let packed_nid = self.sb.packed_nid;
        if packed_nid == 0 {
            return Err(CoreError::InvalidFilesystem(
                "FRAGMENTS feature is enabled without a packed inode",
            ));
        }
        let pcluster = self.recover_stage39_packed_pcluster(packed_nid, fragment_size)?;
        Ok(FragmentTail {
            head_lcn: tail.lcn,
            packed_nid,
            pcluster,
        })
    }

    fn recover_stage39_packed_pcluster(
        &mut self,
        packed_nid: u64,
        fragment_size: u64,
    ) -> Result<u64, CoreError> {
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
        let packed_eof =
            validate_full_eof_plain_sentinel(&packed_entries, packed_lclusters, packed.size)?;
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
        Ok(packed_heads[0].pcluster)
    }

'''
s = s[:start] + replacement + s[end:]
p.write_text(s)
