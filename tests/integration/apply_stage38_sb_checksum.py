from pathlib import Path

p = Path('crates/loom-erofs/src/compact_core.rs')
s = p.read_text()

# EROFS compatible-feature bit for the block-0 superblock checksum.
anchor = 'const FEATURE_ZTAILPACKING: u32 = 0x0000_0010;\n'
assert anchor in s
s = s.replace(anchor, anchor + 'const FEATURE_SB_CHKSUM: u32 = 0x0000_0001;\n', 1)

# Preserve compatible-feature state from the on-disk superblock.
s = s.replace(
    '''struct Superblock {
    root_nid: u64,
    meta_block: u64,
    incompat: u32,
}''',
    '''struct Superblock {
    root_nid: u64,
    meta_block: u64,
    feature_compat: u32,
    incompat: u32,
}''',
    1,
)

old = '''    Ok(Superblock {
        root_nid: u64::from(read_u16(&raw, 0x0e)?),
        meta_block: u64::from(read_u32(&raw, 0x28)?),
        incompat,
    })'''
new = '''    Ok(Superblock {
        root_nid: u64::from(read_u16(&raw, 0x0e)?),
        meta_block: u64::from(read_u32(&raw, 0x28)?),
        feature_compat: read_u32(&raw, 0x08)?,
        incompat,
    })'''
assert old in s
s = s.replace(old, new, 1)

# Pass origin compatible-feature state into the ordinary compiler materializer.
old = '''    compile_blocks(
        origin_path,
        &origin_topology,
        &replacement_topology.heads,
        encoded_blocks,
        vec![BLOCK_BYTES; replacement_topology.heads.len()],
    )'''
new = '''    compile_blocks(
        origin_path,
        &origin_topology,
        &replacement_topology.heads,
        encoded_blocks,
        vec![BLOCK_BYTES; replacement_topology.heads.len()],
        origin.sb.feature_compat,
    )'''
assert old in s
s = s.replace(old, new, 1)

old = '''    compile_blocks(
        origin_path,
        &topology,
        &generated_heads,
        encoded_blocks,
        encoded_bytes,
    )'''
new = '''    compile_blocks(
        origin_path,
        &topology,
        &generated_heads,
        encoded_blocks,
        encoded_bytes,
        origin.sb.feature_compat,
    )'''
assert old in s
s = s.replace(old, new, 1)

old = '''fn compile_blocks(
    origin_path: &Path,
    topology: &Topology,
    replacement_heads: &[Head],
    encoded_blocks: Vec<Vec<u8>>,
    encoded_bytes: Vec<usize>,
) -> Result<CompiledCore, CoreError> {'''
new = '''fn compile_blocks(
    origin_path: &Path,
    topology: &Topology,
    replacement_heads: &[Head],
    encoded_blocks: Vec<Vec<u8>>,
    encoded_bytes: Vec<usize>,
    feature_compat: u32,
) -> Result<CompiledCore, CoreError> {'''
assert old in s
s = s.replace(old, new, 1)

old = '''    let compiled = view.finalize().map_err(CoreError::View)?;
'''
new = '''    if topology.inline_tail.is_some() && feature_compat & FEATURE_SB_CHKSUM != 0 {
        refresh_erofs_superblock_checksum(&mut view)?;
    }
    let compiled = view.finalize().map_err(CoreError::View)?;
'''
# Only replace the ordinary compile_blocks finalize; it is the first occurrence after compile_blocks.
start = s.index('fn compile_blocks(')
pos = s.index(old, start)
s = s[:pos] + s[pos:].replace(old, new, 1)

# Add checksum helpers before compile_big_spans.
anchor = '\nfn compile_big_spans('
assert anchor in s
helpers = r'''
fn refresh_erofs_superblock_checksum(view: &mut EffectiveBlockStore) -> Result<(), CoreError> {
    const SUPER_CHECKSUM_OFFSET: usize = 1028;
    const SUPER_CHECKSUM_END: usize = SUPER_CHECKSUM_OFFSET + 4;
    const CRC32C_POLY: u32 = 0x82f6_3b78;

    let block = view.block_mut(0).map_err(CoreError::View)?;
    if block.len() != BLOCK_BYTES || SUPERBLOCK_OFFSET as usize >= block.len() {
        return Err(CoreError::InvalidFilesystem(
            "EROFS checksum refresh requires a complete 4 KiB block zero",
        ));
    }
    block
        .get_mut(SUPER_CHECKSUM_OFFSET..SUPER_CHECKSUM_END)
        .ok_or(CoreError::UnexpectedEndOfStructure)?
        .fill(0);
    let crc = crc32c_raw(
        u32::MAX,
        block
            .get(SUPERBLOCK_OFFSET as usize..)
            .ok_or(CoreError::UnexpectedEndOfStructure)?,
        CRC32C_POLY,
    );
    block
        .get_mut(SUPER_CHECKSUM_OFFSET..SUPER_CHECKSUM_END)
        .ok_or(CoreError::UnexpectedEndOfStructure)?
        .copy_from_slice(&crc.to_le_bytes());
    Ok(())
}

fn crc32c_raw(mut crc: u32, bytes: &[u8], polynomial: u32) -> u32 {
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (polynomial & mask);
        }
    }
    crc
}

'''
s = s.replace(anchor, '\n' + helpers + 'fn compile_big_spans(', 1)

# Add focused CRC unit test before the existing 0padding round-trip test.
anchor = '''    #[test]
    fn eight_kib_0padding_span_round_trips() {'''
assert anchor in s
unit = '''    #[test]
    fn erofs_crc32c_uses_raw_seeded_castagnoli_state() {
        assert_eq!(
            crc32c_raw(u32::MAX, b"123456789", 0x82f6_3b78),
            0x1cf9_6d7c
        );
    }

'''
s = s.replace(anchor, unit + anchor, 1)

p.write_text(s)
