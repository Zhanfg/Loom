from pathlib import Path
import re

path = Path('crates/loom-erofs/src/compact_core.rs')
text = path.read_text()

old = '''struct BigExtent {
    lcn: usize,
    pcluster: u64,
    physical_blocks: usize,
}'''
new = '''struct BigExtent {
    lcn: usize,
    pcluster: u64,
    physical_blocks: usize,
    kind: HeadKind,
}'''
assert old in text
text = text.replace(old, new, 1)

old = '''        let (span, encoded_len) =
            encode_big_extent(extent.lcn, logical_extent, capacity, topology.placement)?;
        encoded_bytes.push(encoded_len);
        encoded_spans.push(span);'''
new = '''        let (span, encoded_len) = match extent.kind {
            HeadKind::Lz4 => {
                encode_big_extent(extent.lcn, logical_extent, capacity, topology.placement)?
            }
            HeadKind::Plain => {
                if extent.physical_blocks != 1 {
                    return Err(CoreError::InvalidFilesystem(
                        "full big-pcluster PLAIN data extent must occupy one physical block",
                    ));
                }
                encode_plain_extent(extent.lcn, logical_extent)?
            }
        };
        encoded_bytes.push(encoded_len);
        encoded_spans.push(span);'''
assert old in text
text = text.replace(old, new, 1)

old = '''        if origin_extent.lcn != replacement_extent.lcn
            || origin_extent.physical_blocks != replacement_extent.physical_blocks
        {
            return Err(CoreError::IncompatibleReplacement(
                "big-pcluster HEAD/physical-block footprint differs",
            ));
        }'''
new = '''        if origin_extent.lcn != replacement_extent.lcn
            || origin_extent.physical_blocks != replacement_extent.physical_blocks
            || origin_extent.kind != replacement_extent.kind
        {
            return Err(CoreError::IncompatibleReplacement(
                "big-pcluster extent type/HEAD/physical-block footprint differs",
            ));
        }'''
assert old in text
text = text.replace(old, new, 1)

start = text.index('fn recover_full_big_extents(')
end = text.index('\nfn validate_full_big_extent(', start)
replacement = r'''fn recover_full_big_extents(
    entries: &[FullEntry],
    total: usize,
    eof_plain_clusterofs: Option<usize>,
) -> Result<Vec<BigExtent>, CoreError> {
    if entries.len() != total {
        return Err(CoreError::InvalidFilesystem(
            "full big-pcluster index vector length differs from logical lcluster count",
        ));
    }

    let data_end = if eof_plain_clusterofs.is_some() {
        total.saturating_sub(1)
    } else {
        total
    };
    let mut starts = Vec::new();
    for (lcn, entry) in entries.iter().enumerate() {
        if entry.advise & !LCLUSTER_TYPE_MASK != 0 {
            return Err(CoreError::UnsupportedInode(
                "full big-pcluster entries do not accept auxiliary advice bits",
            ));
        }
        let is_eof_sentinel = eof_plain_clusterofs.is_some() && lcn + 1 == total;
        if is_eof_sentinel {
            if entry.kind != LCLUSTER_PLAIN {
                return Err(CoreError::InvalidFilesystem(
                    "full big-pcluster EOF sentinel is not PLAIN",
                ));
            }
            continue;
        }
        if entry.clusterofs != 0 {
            return Err(CoreError::UnsupportedInode(
                "full big-pcluster data entries require zero cluster offsets",
            ));
        }
        match entry.kind {
            LCLUSTER_HEAD1 => starts.push((lcn, HeadKind::Lz4)),
            LCLUSTER_PLAIN => {
                if entry.word == 0 {
                    return Err(CoreError::InvalidFilesystem(
                        "full big-pcluster PLAIN data extent records zero physical block",
                    ));
                }
                starts.push((lcn, HeadKind::Plain));
            }
            LCLUSTER_NONHEAD => {}
            _ => {
                return Err(CoreError::UnsupportedInode(
                    "full big-pcluster supports HEAD1/NONHEAD, aligned one-block PLAIN data, and the verified EOF PLAIN sentinel",
                ));
            }
        }
    }
    if starts.first().map(|(lcn, _)| *lcn) != Some(0) {
        return Err(CoreError::InvalidFilesystem(
            "first full big-pcluster extent does not begin at lcluster zero",
        ));
    }

    let mut extents = Vec::with_capacity(starts.len());
    for (index, &(head_lcn, kind)) in starts.iter().enumerate() {
        let next_head = starts
            .get(index + 1)
            .map(|(lcn, _)| *lcn)
            .unwrap_or(data_end);
        if next_head <= head_lcn {
            return Err(CoreError::InvalidFilesystem(
                "full big-pcluster extent lclusters are not strictly increasing",
            ));
        }
        let head = entries
            .get(head_lcn)
            .ok_or(CoreError::UnexpectedEndOfStructure)?;
        let physical_blocks = match kind {
            HeadKind::Lz4 => validate_full_big_extent(entries, head_lcn, next_head)?,
            HeadKind::Plain => {
                if next_head != head_lcn.saturating_add(1) {
                    return Err(CoreError::UnsupportedInode(
                        "full big-pcluster PLAIN data extent must occupy exactly one logical lcluster",
                    ));
                }
                1
            }
        };
        extents.push(BigExtent {
            lcn: head_lcn,
            pcluster: u64::from(head.word),
            physical_blocks,
            kind,
        });
    }
    if extents.is_empty() {
        return Err(CoreError::InvalidFilesystem(
            "full big-pcluster topology contains no data extent",
        ));
    }
    Ok(extents)
}
'''
text = text[:start] + replacement + text[end:]

# All compact-big constructors and older unit-test literals remain LZ4 extents.
pattern = re.compile(r'(?<!struct )BigExtent \{(?:(?!BigExtent \{).)*?\n(?P<indent>\s*)\}', re.S)
def add_kind(match):
    block = match.group(0)
    if 'kind:' in block:
        return block
    close_indent = match.group('indent')
    field_indent = close_indent + '    '
    pos = block.rfind('\n' + close_indent + '}')
    assert pos >= 0
    return block[:pos] + f'\n{field_indent}kind: HeadKind::Lz4,' + block[pos:]
text = pattern.sub(add_kind, text)

# Add a focused mixed full-big topology unit test before the final 0padding test.
anchor = '''    #[test]\n    fn eight_kib_0padding_span_round_trips() {'''.replace('\\n', '\n')
assert anchor in text
unit = '''    #[test]
    fn full_big_mixed_plain_data_extents_preserve_extent_kind() {
        let entries = vec![
            FullEntry { advise: LCLUSTER_HEAD1, kind: LCLUSTER_HEAD1, clusterofs: 0, word: 10 },
            FullEntry { advise: LCLUSTER_NONHEAD, kind: LCLUSTER_NONHEAD, clusterofs: 0, word: (2_u32 << 16) | u32::from(D0_CBLKCNT | 1) },
            FullEntry { advise: LCLUSTER_NONHEAD, kind: LCLUSTER_NONHEAD, clusterofs: 0, word: (1_u32 << 16) | 2 },
            FullEntry { advise: LCLUSTER_PLAIN, kind: LCLUSTER_PLAIN, clusterofs: 0, word: 11 },
            FullEntry { advise: LCLUSTER_HEAD1, kind: LCLUSTER_HEAD1, clusterofs: 0, word: 12 },
            FullEntry { advise: LCLUSTER_NONHEAD, kind: LCLUSTER_NONHEAD, clusterofs: 0, word: (1_u32 << 16) | u32::from(D0_CBLKCNT | 1) },
        ];
        let extents = recover_full_big_extents(&entries, entries.len(), None).unwrap();
        assert_eq!(extents.len(), 3);
        assert_eq!(extents[0].kind, HeadKind::Lz4);
        assert_eq!(extents[0].physical_blocks, 1);
        assert_eq!(extents[1].kind, HeadKind::Plain);
        assert_eq!(extents[1].physical_blocks, 1);
        assert_eq!(extents[2].kind, HeadKind::Lz4);
        assert_eq!(extents[2].physical_blocks, 1);
    }

'''
text = text.replace(anchor, unit + anchor, 1)

path.write_text(text)
