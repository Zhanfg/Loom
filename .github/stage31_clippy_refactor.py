from pathlib import Path

path = Path('crates/loom-erofs/src/compact_core.rs')
s = path.read_text()

fn_start = s.index('    fn read_full_topology_from_inode(&mut self, inode: Inode) -> Result<Topology, CoreError> {')
loop_start = s.index('        let mut heads = Vec::new();', fn_start)
loop_end = s.index('        if heads.first().map(|head| head.lcn) != Some(0) {', loop_start)
s = (
    s[:loop_start]
    + '        let heads = recover_full_data_heads(\n'
      '            &entries,\n'
      '            logical_lclusters,\n'
      '            eof_plain_clusterofs,\n'
      '        )?;\n'
    + s[loop_end:]
)

anchor = 'fn validate_full_eof_plain_sentinel(\n'
helper = '''fn recover_full_data_heads(
    entries: &[FullEntry],
    logical_lclusters: usize,
    eof_plain_clusterofs: Option<usize>,
) -> Result<Vec<Head>, CoreError> {
    let mut heads = Vec::new();
    for (lcn, entry) in entries.iter().enumerate() {
        if entry.advise & !LCLUSTER_TYPE_MASK != 0 {
            return Err(CoreError::UnsupportedInode(
                "full-index entries do not accept auxiliary advice bits",
            ));
        }
        match entry.kind {
            LCLUSTER_HEAD1 => {
                if entry.clusterofs != 0 {
                    return Err(CoreError::UnsupportedInode(
                        "full-index HEAD1 entries require zero cluster offsets",
                    ));
                }
                heads.push(Head {
                    lcn,
                    pcluster: u64::from(entry.word),
                    kind: HeadKind::Lz4,
                });
            }
            LCLUSTER_NONHEAD => {
                if entry.clusterofs != 0 {
                    return Err(CoreError::UnsupportedInode(
                        "full-index NONHEAD entries require zero cluster offsets",
                    ));
                }
            }
            LCLUSTER_PLAIN => {
                let is_eof_sentinel =
                    eof_plain_clusterofs.is_some() && lcn + 1 == logical_lclusters;
                if !is_eof_sentinel {
                    if entry.clusterofs != 0 {
                        return Err(CoreError::UnsupportedInode(
                            "full-index PLAIN data heads require zero cluster offsets",
                        ));
                    }
                    heads.push(Head {
                        lcn,
                        pcluster: u64::from(entry.word),
                        kind: HeadKind::Plain,
                    });
                }
            }
            _ => {
                return Err(CoreError::UnsupportedInode(
                    "full-index supports only HEAD1, NONHEAD, aligned PLAIN data, and the verified partial-EOF PLAIN sentinel",
                ));
            }
        }
    }
    Ok(heads)
}

fn validate_full_eof_plain_sentinel(
'''
assert s.count(anchor) == 1
s = s.replace(anchor, helper)
path.write_text(s)
