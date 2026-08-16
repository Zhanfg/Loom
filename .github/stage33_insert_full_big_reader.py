from pathlib import Path

path = Path('crates/loom-erofs/src/compact_core.rs')
s = path.read_text()
assert 'fn read_full_big_topology_from_inode' not in s
marker = '    fn read_map_header(\n'
assert s.count(marker) == 1, s.count(marker)
method = '''    fn read_full_big_topology_from_inode(
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
s = s.replace(marker, method + marker)
path.write_text(s)
