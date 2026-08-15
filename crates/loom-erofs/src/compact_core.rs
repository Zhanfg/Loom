#![forbid(unsafe_code)]

#[path = "multi_lz4.rs"]
mod lz4;

use loom_map::LoomMap;
use loom_view::{EffectiveBlockStore, ViewError};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const SUPERBLOCK_OFFSET: u64 = 1024;
const SUPERBLOCK_SIZE: usize = 128;
const EROFS_MAGIC: u32 = 0xe0f5_e1e2;
const BLOCK_SIZE: u32 = 4096;
const BLOCK_BYTES: usize = 4096;
const BLOCK_BITS: u16 = 12;
const OFFSET_MASK: u16 = (1 << BLOCK_BITS) - 1;
const D0_CBLKCNT: u16 = 1 << 11;
const DIRENT_SIZE: usize = 12;
const MODE_TYPE_MASK: u16 = 0o170_000;
const MODE_DIRECTORY: u16 = 0o040_000;
const MODE_REGULAR: u16 = 0o100_000;
const DATA_FLAT_PLAIN: u8 = 0;
const DATA_FLAT_INLINE: u8 = 2;
const DATA_COMPRESSED_COMPACT: u8 = 3;
const MAP_HEADER_SIZE: u64 = 8;
const LCLUSTER_HEAD1: u16 = 1;
const LCLUSTER_NONHEAD: u16 = 2;
const LCLUSTER_TYPE_MASK: u16 = 3;
const ADVISE_COMPACTED_2B: u16 = 0x0001;
const LZ4_ALGORITHM: u8 = 0;
const FEATURE_LZ4_0PADDING: u32 = 0x0000_0001;

#[derive(Debug)]
pub(crate) struct CompiledCore {
    pub(crate) map: LoomMap,
    pub(crate) shadow: Vec<u8>,
    pub(crate) block_size: u32,
    pub(crate) origin_nid: u64,
    pub(crate) origin_pclusters: Vec<u64>,
    pub(crate) replacement_pclusters: Vec<u64>,
    pub(crate) head_lclusters: Vec<usize>,
    pub(crate) encoded_bytes: Vec<usize>,
    pub(crate) logical_lclusters: usize,
    pub(crate) compact_2b_entries: usize,
    pub(crate) shadow_blocks: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Head {
    lcn: usize,
    pcluster: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Topology {
    nid: u64,
    logical_size: u64,
    algorithm: u8,
    advise: u16,
    logical_lclusters: usize,
    compact_2b_entries: usize,
    heads: Vec<Head>,
}

#[derive(Debug, Clone, Copy)]
struct Superblock {
    root_nid: u64,
    meta_block: u64,
}

#[derive(Debug, Clone, Copy)]
struct Inode {
    nid: u64,
    offset: u64,
    isize: u64,
    xattr_size: u64,
    mode: u16,
    size: u64,
    layout: u8,
    data_word: u32,
}

impl Inode {
    fn file_type(self) -> u16 {
        self.mode & MODE_TYPE_MASK
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CompactRegions {
    initial_4b: usize,
    compact_2b: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CompactEntry {
    kind: u16,
    low: u16,
    slot: usize,
    slots: usize,
    base_pblk: u64,
}

struct Image {
    file: File,
    bytes: u64,
    sb: Superblock,
}

pub(crate) fn compile_oracle(
    origin_path: &Path,
    target_path: &str,
    replacement_image_path: &Path,
) -> Result<CompiledCore, CoreError> {
    let mut origin = Image::open(origin_path)?;
    let mut replacement = Image::open(replacement_image_path)?;
    let origin_nid = origin.resolve_path(target_path)?;
    let replacement_nid = replacement.resolve_path(target_path)?;
    let origin_topology = origin.read_topology(origin_nid)?;
    let replacement_topology = replacement.read_topology(replacement_nid)?;
    validate_compatible_topology(&origin_topology, &replacement_topology)?;

    let mut encoded_blocks = Vec::with_capacity(replacement_topology.heads.len());
    for head in &replacement_topology.heads {
        encoded_blocks.push(replacement.read_block(head.pcluster)?);
    }
    compile_blocks(
        origin_path,
        &origin_topology,
        &replacement_topology.heads,
        encoded_blocks,
        vec![BLOCK_BYTES; replacement_topology.heads.len()],
    )
}

pub(crate) fn compile_lz4(
    origin_path: &Path,
    target_path: &str,
    replacement_path: &Path,
) -> Result<CompiledCore, CoreError> {
    let replacement = fs::read(replacement_path).map_err(CoreError::Io)?;
    let mut origin = Image::open(origin_path)?;
    let origin_nid = origin.resolve_path(target_path)?;
    let topology = origin.read_topology(origin_nid)?;
    let actual = u64::try_from(replacement.len()).map_err(|_| CoreError::ArithmeticOverflow)?;
    if actual != topology.logical_size {
        return Err(CoreError::ReplacementSizeMismatch {
            expected: topology.logical_size,
            actual,
        });
    }

    // All extents are encoded and validated before the effective block store is opened.
    let mut encoded_blocks = Vec::with_capacity(topology.heads.len());
    let mut encoded_bytes = Vec::with_capacity(topology.heads.len());
    for (index, head) in topology.heads.iter().enumerate() {
        let next_lcn = topology
            .heads
            .get(index + 1)
            .map_or(topology.logical_lclusters, |next| next.lcn);
        let start = head
            .lcn
            .checked_mul(BLOCK_BYTES)
            .ok_or(CoreError::ArithmeticOverflow)?;
        let end = next_lcn
            .checked_mul(BLOCK_BYTES)
            .ok_or(CoreError::ArithmeticOverflow)?;
        let extent = replacement
            .get(start..end)
            .ok_or(CoreError::InvalidFilesystem(
                "recovered logical extent lies beyond replacement payload",
            ))?;
        let (block, encoded_len) = encode_extent(head.lcn, extent)?;
        encoded_blocks.push(block);
        encoded_bytes.push(encoded_len);
    }

    let generated_heads = topology.heads.clone();
    compile_blocks(
        origin_path,
        &topology,
        &generated_heads,
        encoded_blocks,
        encoded_bytes,
    )
}

fn encode_extent(head_lcn: usize, extent: &[u8]) -> Result<(Vec<u8>, usize), CoreError> {
    let compressed = lz4::encode(extent).map_err(|_| CoreError::CompressionValidationFailed)?;
    if compressed.len() > BLOCK_BYTES {
        return Err(CoreError::CompressionDoesNotFit {
            head_lcn,
            encoded: compressed.len(),
            capacity: BLOCK_BYTES,
        });
    }
    if compressed.first().copied().unwrap_or(0) == 0 {
        return Err(CoreError::CompressionValidationFailed);
    }
    if lz4::decode(&compressed, extent.len()).map_err(|_| CoreError::CompressionValidationFailed)?
        != extent
    {
        return Err(CoreError::CompressionValidationFailed);
    }

    let mut pcluster = vec![0_u8; BLOCK_BYTES];
    let start = BLOCK_BYTES
        .checked_sub(compressed.len())
        .ok_or(CoreError::ArithmeticOverflow)?;
    pcluster[start..].copy_from_slice(&compressed);
    if lz4::decode_0padding(&pcluster, extent.len())
        .map_err(|_| CoreError::CompressionValidationFailed)?
        != extent
    {
        return Err(CoreError::CompressionValidationFailed);
    }
    Ok((pcluster, compressed.len()))
}

fn compile_blocks(
    origin_path: &Path,
    topology: &Topology,
    replacement_heads: &[Head],
    encoded_blocks: Vec<Vec<u8>>,
    encoded_bytes: Vec<usize>,
) -> Result<CompiledCore, CoreError> {
    if replacement_heads.len() != topology.heads.len()
        || encoded_blocks.len() != topology.heads.len()
        || encoded_bytes.len() != topology.heads.len()
    {
        return Err(CoreError::InvalidFilesystem(
            "compact compiler received inconsistent extent vectors",
        ));
    }

    let mut view = EffectiveBlockStore::open(origin_path, BLOCK_SIZE).map_err(CoreError::View)?;
    let mut origin_pclusters = Vec::with_capacity(topology.heads.len());
    let mut replacement_pclusters = Vec::with_capacity(topology.heads.len());
    let mut head_lclusters = Vec::with_capacity(topology.heads.len());

    for ((origin_head, replacement_head), encoded) in topology
        .heads
        .iter()
        .zip(replacement_heads)
        .zip(encoded_blocks)
    {
        if encoded.len() != BLOCK_BYTES {
            return Err(CoreError::InvalidFilesystem(
                "encoded extent does not occupy exactly one physical block",
            ));
        }
        view.block_mut(origin_head.pcluster)
            .map_err(CoreError::View)?
            .copy_from_slice(&encoded);
        origin_pclusters.push(origin_head.pcluster);
        replacement_pclusters.push(replacement_head.pcluster);
        head_lclusters.push(origin_head.lcn);
    }

    let compiled = view.finalize().map_err(CoreError::View)?;
    if compiled.shadow_blocks != topology.heads.len() {
        return Err(CoreError::InvalidFilesystem(
            "compact shadow block count does not match recovered HEAD count",
        ));
    }

    Ok(CompiledCore {
        map: compiled.map,
        shadow: compiled.shadow,
        block_size: compiled.block_size,
        origin_nid: topology.nid,
        origin_pclusters,
        replacement_pclusters,
        head_lclusters,
        encoded_bytes,
        logical_lclusters: topology.logical_lclusters,
        compact_2b_entries: topology.compact_2b_entries,
        shadow_blocks: compiled.shadow_blocks,
    })
}

fn validate_compatible_topology(
    origin: &Topology,
    replacement: &Topology,
) -> Result<(), CoreError> {
    if origin.logical_size != replacement.logical_size {
        return Err(CoreError::IncompatibleReplacement(
            "logical file sizes differ",
        ));
    }
    if origin.algorithm != replacement.algorithm {
        return Err(CoreError::IncompatibleReplacement(
            "compression algorithms differ",
        ));
    }
    if origin.advise != replacement.advise {
        return Err(CoreError::IncompatibleReplacement(
            "compact map advice differs",
        ));
    }
    if origin.logical_lclusters != replacement.logical_lclusters {
        return Err(CoreError::IncompatibleReplacement(
            "logical compact-index lengths differ",
        ));
    }
    if origin.heads.len() != replacement.heads.len() {
        return Err(CoreError::IncompatibleReplacement(
            "physical pcluster counts differ",
        ));
    }
    if origin
        .heads
        .iter()
        .map(|head| head.lcn)
        .ne(replacement.heads.iter().map(|head| head.lcn))
    {
        return Err(CoreError::IncompatibleReplacement(
            "compressed HEAD-lcluster topology differs",
        ));
    }
    Ok(())
}

impl Image {
    fn open(path: &Path) -> Result<Self, CoreError> {
        let mut file = File::open(path).map_err(CoreError::Io)?;
        let bytes = file.metadata().map_err(CoreError::Io)?.len();
        let sb = read_superblock(&mut file, bytes)?;
        Ok(Self { file, bytes, sb })
    }

    fn read_inode(&mut self, nid: u64) -> Result<Inode, CoreError> {
        let metadata_base = self
            .sb
            .meta_block
            .checked_mul(u64::from(BLOCK_SIZE))
            .ok_or(CoreError::ArithmeticOverflow)?;
        let offset = metadata_base
            .checked_add(nid.checked_mul(32).ok_or(CoreError::ArithmeticOverflow)?)
            .ok_or(CoreError::ArithmeticOverflow)?;
        ensure_range(self.bytes, offset, 32)?;

        let mut compact = [0_u8; 32];
        read_exact_at(&mut self.file, offset, &mut compact)?;
        let format = read_u16(&compact, 0)?;
        if format & !0x1f != 0 {
            return Err(CoreError::UnsupportedInode(
                "unknown EROFS inode format bits",
            ));
        }
        let extended = format & 1 != 0;
        let layout = u8::try_from((format >> 1) & 7).map_err(|_| CoreError::ArithmeticOverflow)?;
        if layout > 4 {
            return Err(CoreError::UnsupportedInode(
                "reserved EROFS inode data layout",
            ));
        }
        let isize = if extended { 64 } else { 32 };
        let xattr_size = xattr_ibody_size(read_u16(&compact, 2)?)?;
        let mode = read_u16(&compact, 4)?;
        let size = if extended {
            ensure_range(self.bytes, offset, 64)?;
            let mut raw = [0_u8; 64];
            read_exact_at(&mut self.file, offset, &mut raw)?;
            read_u64(&raw, 8)?
        } else {
            u64::from(read_u32(&compact, 8)?)
        };
        Ok(Inode {
            nid,
            offset,
            isize,
            xattr_size,
            mode,
            size,
            layout,
            data_word: read_u32(&compact, 0x10)?,
        })
    }

    fn resolve_path(&mut self, path: &str) -> Result<u64, CoreError> {
        let components = parse_absolute_path(path)?;
        let mut current = self.sb.root_nid;
        for component in components {
            let inode = self.read_inode(current)?;
            if inode.file_type() != MODE_DIRECTORY {
                return Err(CoreError::NotDirectory(current));
            }
            current = self.find_child(&inode, component.as_bytes())?;
        }
        Ok(current)
    }

    fn find_child(&mut self, directory: &Inode, name: &[u8]) -> Result<u64, CoreError> {
        if directory.xattr_size != 0 {
            return Err(CoreError::UnsupportedInode(
                "compact path traversal refuses directory xattrs",
            ));
        }
        let block_size = u64::from(BLOCK_SIZE);
        let full_blocks = match directory.layout {
            DATA_FLAT_PLAIN => div_ceil(directory.size, block_size)?,
            DATA_FLAT_INLINE => directory.size / block_size,
            _ => {
                return Err(CoreError::UnsupportedInode(
                    "compact path traversal requires flat directories",
                ))
            }
        };
        for index in 0..full_blocks {
            let block = u64::from(directory.data_word)
                .checked_add(index)
                .ok_or(CoreError::ArithmeticOverflow)?;
            let bytes = self.read_block(block)?;
            if let Some(nid) = find_in_directory_block(&bytes, name)? {
                return Ok(nid);
            }
        }
        if directory.layout == DATA_FLAT_INLINE && directory.size % block_size != 0 {
            return Err(CoreError::UnsupportedInode(
                "target may lie in unsupported inline directory tail",
            ));
        }
        Err(CoreError::PathNotFound(
            String::from_utf8_lossy(name).into_owned(),
        ))
    }

    fn read_topology(&mut self, nid: u64) -> Result<Topology, CoreError> {
        let inode = self.read_inode(nid)?;
        let logical_lclusters = validate_target_inode(&inode)?;
        let compressed_blocks =
            usize::try_from(inode.data_word).map_err(|_| CoreError::ArithmeticOverflow)?;
        if compressed_blocks == 0 {
            return Err(CoreError::InvalidFilesystem(
                "compact inode reports zero encoded physical blocks",
            ));
        }

        let header_offset = align8(
            inode
                .offset
                .checked_add(inode.isize)
                .and_then(|value| value.checked_add(inode.xattr_size))
                .ok_or(CoreError::ArithmeticOverflow)?,
        )?;
        ensure_range(self.bytes, header_offset, MAP_HEADER_SIZE)?;
        let mut header = [0_u8; 8];
        read_exact_at(&mut self.file, header_offset, &mut header)?;
        let advise = read_u16(&header, 4)?;
        if advise != ADVISE_COMPACTED_2B {
            return Err(CoreError::UnsupportedInode(
                "compact core requires only COMPACTED_2B advice",
            ));
        }
        let algorithm = header[6] & 0x0f;
        if algorithm != LZ4_ALGORITHM || header[6] >> 4 != 0 {
            return Err(CoreError::UnsupportedInode(
                "compact core supports only HEAD1 LZ4",
            ));
        }
        if header[7] != 0 {
            return Err(CoreError::UnsupportedInode(
                "compact core requires 4 KiB logical clusters without packed fragments",
            ));
        }

        let ebase = header_offset
            .checked_add(MAP_HEADER_SIZE)
            .ok_or(CoreError::ArithmeticOverflow)?;
        let regions = compact_regions(ebase, logical_lclusters)?;
        let entries = self.read_all_entries(ebase, logical_lclusters)?;
        let heads = self.recover_heads(ebase, logical_lclusters, &entries)?;
        if heads.len() != compressed_blocks {
            return Err(CoreError::InvalidFilesystem(
                "compressed block count does not match recovered HEAD count",
            ));
        }
        validate_nonheads(&entries, &heads, logical_lclusters)?;
        validate_head_blocks(&heads, self.bytes)?;

        Ok(Topology {
            nid,
            logical_size: inode.size,
            algorithm,
            advise,
            logical_lclusters,
            compact_2b_entries: regions.compact_2b,
            heads,
        })
    }

    fn read_all_entries(
        &mut self,
        ebase: u64,
        total: usize,
    ) -> Result<Vec<CompactEntry>, CoreError> {
        let mut entries = Vec::with_capacity(total);
        for lcn in 0..total {
            entries.push(self.read_compact_entry(ebase, total, lcn)?);
        }
        Ok(entries)
    }

    fn recover_heads(
        &mut self,
        ebase: u64,
        total: usize,
        entries: &[CompactEntry],
    ) -> Result<Vec<Head>, CoreError> {
        let mut heads = Vec::new();
        for (lcn, entry) in entries.iter().enumerate() {
            match entry.kind {
                LCLUSTER_HEAD1 => {
                    if entry.low != 0 {
                        return Err(CoreError::UnsupportedInode(
                            "compact core requires zero-offset HEAD1 lclusters",
                        ));
                    }
                    let pcluster = self.reconstruct_head_pcluster(ebase, total, lcn, *entry)?;
                    heads.push(Head { lcn, pcluster });
                }
                LCLUSTER_NONHEAD => {}
                _ => {
                    return Err(CoreError::UnsupportedInode(
                        "compact core supports only HEAD1 and NONHEAD entries",
                    ))
                }
            }
        }
        if heads.first().map(|head| head.lcn) != Some(0) {
            return Err(CoreError::InvalidFilesystem(
                "first compressed extent does not begin at lcluster zero",
            ));
        }
        Ok(heads)
    }

    fn reconstruct_head_pcluster(
        &mut self,
        ebase: u64,
        total: usize,
        lcn: usize,
        head: CompactEntry,
    ) -> Result<u64, CoreError> {
        let pack_first_lcn = lcn
            .checked_sub(head.slot)
            .ok_or(CoreError::ArithmeticOverflow)?;
        let mut slot = isize::try_from(head.slot).map_err(|_| CoreError::ArithmeticOverflow)?;
        let mut nblk = 1_u64;

        while slot > 0 {
            slot -= 1;
            let slot_usize = usize::try_from(slot).map_err(|_| CoreError::ArithmeticOverflow)?;
            let previous_lcn = pack_first_lcn
                .checked_add(slot_usize)
                .ok_or(CoreError::ArithmeticOverflow)?;
            if previous_lcn >= total {
                return Err(CoreError::InvalidFilesystem(
                    "compact pack refers beyond logical file",
                ));
            }
            let previous = self.read_compact_entry(ebase, total, previous_lcn)?;
            if previous.kind == LCLUSTER_NONHEAD {
                if previous.slot + 1 == previous.slots {
                    return Err(CoreError::InvalidFilesystem(
                        "head pblk reconstruction crossed a final-slot delta1 entry",
                    ));
                }
                let lookback =
                    isize::try_from(previous.low).map_err(|_| CoreError::ArithmeticOverflow)?;
                slot -= lookback;
            }
            if slot >= 0 {
                nblk = nblk.checked_add(1).ok_or(CoreError::ArithmeticOverflow)?;
            }
        }
        head.base_pblk
            .checked_add(nblk)
            .ok_or(CoreError::ArithmeticOverflow)
    }

    fn read_compact_entry(
        &mut self,
        ebase: u64,
        total: usize,
        lcn: usize,
    ) -> Result<CompactEntry, CoreError> {
        if lcn >= total {
            return Err(CoreError::InvalidFilesystem(
                "compact lcluster index lies beyond logical file",
            ));
        }
        let regions = compact_regions(ebase, total)?;
        let (shift, pos) = compact_entry_position(ebase, regions, lcn)?;
        let entry_bytes = 1_usize
            .checked_shl(shift)
            .ok_or(CoreError::ArithmeticOverflow)?;
        let slots = if entry_bytes == 4 { 2 } else { 16 };
        let pack_bytes = entry_bytes
            .checked_mul(slots)
            .ok_or(CoreError::ArithmeticOverflow)?;
        let pack_bytes_u64 =
            u64::try_from(pack_bytes).map_err(|_| CoreError::ArithmeticOverflow)?;
        let pack_start = pos - (pos % pack_bytes_u64);
        ensure_range(self.bytes, pack_start, pack_bytes_u64)?;
        let mut pack = vec![0_u8; pack_bytes];
        read_exact_at(&mut self.file, pack_start, &mut pack)?;

        let entry_bytes_u64 =
            u64::try_from(entry_bytes).map_err(|_| CoreError::ArithmeticOverflow)?;
        let slot = usize::try_from((pos - pack_start) / entry_bytes_u64)
            .map_err(|_| CoreError::ArithmeticOverflow)?;
        if slot >= slots {
            return Err(CoreError::InvalidFilesystem(
                "compact entry slot lies beyond pack",
            ));
        }
        let encode_bits = (pack_bytes
            .checked_mul(8)
            .and_then(|bits| bits.checked_sub(32))
            .ok_or(CoreError::ArithmeticOverflow)?)
            / slots;
        let bit_pos = encode_bits
            .checked_mul(slot)
            .ok_or(CoreError::ArithmeticOverflow)?;
        let word = read_u32(&pack, bit_pos / 8)? >> (bit_pos & 7);
        let low = u16::try_from(word & u32::from(OFFSET_MASK))
            .map_err(|_| CoreError::ArithmeticOverflow)?;
        let kind = u16::try_from((word >> BLOCK_BITS) & u32::from(LCLUSTER_TYPE_MASK))
            .map_err(|_| CoreError::ArithmeticOverflow)?;
        let base_pblk = u64::from(read_u32(&pack, pack_bytes - 4)?);
        Ok(CompactEntry {
            kind,
            low,
            slot,
            slots,
            base_pblk,
        })
    }

    fn read_block(&mut self, block: u64) -> Result<Vec<u8>, CoreError> {
        let offset = block
            .checked_mul(u64::from(BLOCK_SIZE))
            .ok_or(CoreError::ArithmeticOverflow)?;
        ensure_range(self.bytes, offset, u64::from(BLOCK_SIZE))?;
        let mut bytes = vec![0_u8; BLOCK_BYTES];
        read_exact_at(&mut self.file, offset, &mut bytes)?;
        Ok(bytes)
    }
}

fn validate_target_inode(inode: &Inode) -> Result<usize, CoreError> {
    if inode.file_type() != MODE_REGULAR {
        return Err(CoreError::NotRegularFile(inode.nid));
    }
    if inode.layout != DATA_COMPRESSED_COMPACT {
        return Err(CoreError::UnsupportedInode(
            "compact core requires EROFS_INODE_COMPRESSED_COMPACT",
        ));
    }
    if inode.xattr_size != 0 {
        return Err(CoreError::UnsupportedInode(
            "compact target must not carry xattrs",
        ));
    }
    if inode.size < u64::from(BLOCK_SIZE) * 2 || inode.size % u64::from(BLOCK_SIZE) != 0 {
        return Err(CoreError::UnsupportedInode(
            "compact core requires a whole-block file of at least two lclusters",
        ));
    }
    usize::try_from(inode.size / u64::from(BLOCK_SIZE)).map_err(|_| CoreError::ArithmeticOverflow)
}

fn validate_nonheads(
    entries: &[CompactEntry],
    heads: &[Head],
    total: usize,
) -> Result<(), CoreError> {
    for (head_index, head) in heads.iter().enumerate() {
        let next_head = heads.get(head_index + 1).map_or(total, |next| next.lcn);
        if next_head <= head.lcn {
            return Err(CoreError::InvalidFilesystem(
                "compressed HEAD lclusters are not strictly increasing",
            ));
        }
        for lcn in head.lcn + 1..next_head {
            let entry = entries.get(lcn).ok_or(CoreError::InvalidFilesystem(
                "missing compact NONHEAD entry",
            ))?;
            if entry.kind != LCLUSTER_NONHEAD {
                return Err(CoreError::InvalidFilesystem(
                    "compressed extent contains an unexpected non-NONHEAD entry",
                ));
            }
            let expected = if entry.slot + 1 == entry.slots {
                next_head
                    .checked_sub(lcn)
                    .ok_or(CoreError::ArithmeticOverflow)?
            } else {
                lcn.checked_sub(head.lcn)
                    .ok_or(CoreError::ArithmeticOverflow)?
            };
            if expected >= usize::from(D0_CBLKCNT) {
                return Err(CoreError::UnsupportedInode(
                    "compact core refuses CBLKCNT/long-distance NONHEAD encoding",
                ));
            }
            let expected = u16::try_from(expected).map_err(|_| CoreError::ArithmeticOverflow)?;
            if entry.low != expected {
                return Err(CoreError::InvalidFilesystem(
                    "compact NONHEAD lookback/lookahead does not match recovered HEAD topology",
                ));
            }
        }
    }
    Ok(())
}

fn validate_head_blocks(heads: &[Head], image_bytes: u64) -> Result<(), CoreError> {
    let block_count = image_bytes / u64::from(BLOCK_SIZE);
    let mut previous = None;
    for head in heads {
        if head.pcluster >= block_count {
            return Err(CoreError::InvalidFilesystem(
                "compressed pcluster lies beyond image",
            ));
        }
        if let Some(previous) = previous {
            if head.pcluster <= previous {
                return Err(CoreError::InvalidFilesystem(
                    "compressed pclusters are not strictly increasing",
                ));
            }
        }
        previous = Some(head.pcluster);
    }
    Ok(())
}

fn compact_regions(ebase: u64, total: usize) -> Result<CompactRegions, CoreError> {
    let modulo = usize::try_from(ebase % 32).map_err(|_| CoreError::ArithmeticOverflow)?;
    let mut initial_4b = (32_usize
        .checked_sub(modulo)
        .ok_or(CoreError::ArithmeticOverflow)?)
        / 4;
    if initial_4b == 8 {
        initial_4b = 0;
    }
    let compact_2b = if initial_4b < total {
        ((total - initial_4b) / 16) * 16
    } else {
        0
    };
    Ok(CompactRegions {
        initial_4b,
        compact_2b,
    })
}

fn compact_entry_position(
    ebase: u64,
    regions: CompactRegions,
    lcn: usize,
) -> Result<(u32, u64), CoreError> {
    if lcn < regions.initial_4b {
        let delta = u64::try_from(lcn.checked_mul(4).ok_or(CoreError::ArithmeticOverflow)?)
            .map_err(|_| CoreError::ArithmeticOverflow)?;
        return Ok((
            2,
            ebase
                .checked_add(delta)
                .ok_or(CoreError::ArithmeticOverflow)?,
        ));
    }

    let initial_bytes = u64::try_from(
        regions
            .initial_4b
            .checked_mul(4)
            .ok_or(CoreError::ArithmeticOverflow)?,
    )
    .map_err(|_| CoreError::ArithmeticOverflow)?;
    let mut pos = ebase
        .checked_add(initial_bytes)
        .ok_or(CoreError::ArithmeticOverflow)?;
    let relative = lcn
        .checked_sub(regions.initial_4b)
        .ok_or(CoreError::ArithmeticOverflow)?;
    if relative < regions.compact_2b {
        let delta = u64::try_from(
            relative
                .checked_mul(2)
                .ok_or(CoreError::ArithmeticOverflow)?,
        )
        .map_err(|_| CoreError::ArithmeticOverflow)?;
        return Ok((
            1,
            pos.checked_add(delta)
                .ok_or(CoreError::ArithmeticOverflow)?,
        ));
    }

    let compact_bytes = u64::try_from(
        regions
            .compact_2b
            .checked_mul(2)
            .ok_or(CoreError::ArithmeticOverflow)?,
    )
    .map_err(|_| CoreError::ArithmeticOverflow)?;
    pos = pos
        .checked_add(compact_bytes)
        .ok_or(CoreError::ArithmeticOverflow)?;
    let trailing = relative
        .checked_sub(regions.compact_2b)
        .ok_or(CoreError::ArithmeticOverflow)?;
    let delta = u64::try_from(
        trailing
            .checked_mul(4)
            .ok_or(CoreError::ArithmeticOverflow)?,
    )
    .map_err(|_| CoreError::ArithmeticOverflow)?;
    Ok((
        2,
        pos.checked_add(delta)
            .ok_or(CoreError::ArithmeticOverflow)?,
    ))
}

fn read_superblock(file: &mut File, bytes: u64) -> Result<Superblock, CoreError> {
    ensure_range(
        bytes,
        SUPERBLOCK_OFFSET,
        u64::try_from(SUPERBLOCK_SIZE).map_err(|_| CoreError::ArithmeticOverflow)?,
    )?;
    let mut raw = [0_u8; SUPERBLOCK_SIZE];
    read_exact_at(file, SUPERBLOCK_OFFSET, &mut raw)?;
    let magic = read_u32(&raw, 0)?;
    if magic != EROFS_MAGIC {
        return Err(CoreError::BadMagic(magic));
    }
    if raw[0x0c] != 12 {
        return Err(CoreError::UnsupportedFilesystem(
            "compact core supports only 4 KiB EROFS blocks",
        ));
    }
    let incompat = read_u32(&raw, 0x50)?;
    if incompat & !FEATURE_LZ4_0PADDING != 0 {
        return Err(CoreError::UnsupportedFilesystem(
            "compact image enables unsupported incompatible EROFS features",
        ));
    }
    if incompat & FEATURE_LZ4_0PADDING == 0 {
        return Err(CoreError::UnsupportedFilesystem(
            "compact core expects LZ4_0PADDING layout",
        ));
    }
    if raw[0x5a] != 0 || read_u16(&raw, 0x56)? != 0 {
        return Err(CoreError::UnsupportedFilesystem(
            "compact core requires primary-device core directories",
        ));
    }
    Ok(Superblock {
        root_nid: u64::from(read_u16(&raw, 0x0e)?),
        meta_block: u64::from(read_u32(&raw, 0x28)?),
    })
}

fn xattr_ibody_size(count: u16) -> Result<u64, CoreError> {
    if count == 0 {
        return Ok(0);
    }
    12_u64
        .checked_add(
            u64::from(count - 1)
                .checked_mul(4)
                .ok_or(CoreError::ArithmeticOverflow)?,
        )
        .ok_or(CoreError::ArithmeticOverflow)
}

fn find_in_directory_block(block: &[u8], target: &[u8]) -> Result<Option<u64>, CoreError> {
    if block.len() < DIRENT_SIZE {
        return Err(CoreError::CorruptDirectory);
    }
    let first_name_offset = usize::from(read_u16(block, 8)?);
    if first_name_offset == 0
        || first_name_offset % DIRENT_SIZE != 0
        || first_name_offset > block.len()
    {
        return Err(CoreError::CorruptDirectory);
    }
    let count = first_name_offset / DIRENT_SIZE;
    for index in 0..count {
        let entry = index
            .checked_mul(DIRENT_SIZE)
            .ok_or(CoreError::ArithmeticOverflow)?;
        let name_offset = usize::from(read_u16(block, entry + 8)?);
        if name_offset < first_name_offset || name_offset >= block.len() {
            return Err(CoreError::CorruptDirectory);
        }
        let name_end = if index + 1 < count {
            usize::from(read_u16(block, entry + DIRENT_SIZE + 8)?)
        } else {
            let tail = block
                .get(name_offset..)
                .ok_or(CoreError::CorruptDirectory)?;
            name_offset
                .checked_add(
                    tail.iter()
                        .position(|byte| *byte == 0)
                        .unwrap_or(tail.len()),
                )
                .ok_or(CoreError::ArithmeticOverflow)?
        };
        if name_end < name_offset || name_end > block.len() {
            return Err(CoreError::CorruptDirectory);
        }
        if block
            .get(name_offset..name_end)
            .ok_or(CoreError::CorruptDirectory)?
            == target
        {
            return Ok(Some(read_u64(block, entry)?));
        }
    }
    Ok(None)
}

fn parse_absolute_path(path: &str) -> Result<Vec<&str>, CoreError> {
    if !path.starts_with('/') {
        return Err(CoreError::InvalidPath("path must be absolute"));
    }
    if path == "/" {
        return Ok(Vec::new());
    }
    let mut components = Vec::new();
    for component in path[1..].split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(CoreError::InvalidPath(
                "empty, dot, and parent components are forbidden",
            ));
        }
        components.push(component);
    }
    Ok(components)
}

fn align8(value: u64) -> Result<u64, CoreError> {
    value
        .checked_add(7)
        .map(|rounded| rounded & !7_u64)
        .ok_or(CoreError::ArithmeticOverflow)
}

fn div_ceil(value: u64, divisor: u64) -> Result<u64, CoreError> {
    value
        .checked_add(divisor.saturating_sub(1))
        .map(|rounded| rounded / divisor)
        .ok_or(CoreError::ArithmeticOverflow)
}

fn ensure_range(bytes: u64, offset: u64, length: u64) -> Result<(), CoreError> {
    let end = offset
        .checked_add(length)
        .ok_or(CoreError::ArithmeticOverflow)?;
    if end > bytes {
        return Err(CoreError::UnexpectedEndOfImage);
    }
    Ok(())
}

fn read_exact_at(file: &mut File, offset: u64, buffer: &mut [u8]) -> Result<(), CoreError> {
    file.seek(SeekFrom::Start(offset)).map_err(CoreError::Io)?;
    file.read_exact(buffer).map_err(CoreError::Io)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, CoreError> {
    let end = offset.checked_add(2).ok_or(CoreError::ArithmeticOverflow)?;
    let raw: [u8; 2] = bytes
        .get(offset..end)
        .ok_or(CoreError::UnexpectedEndOfStructure)?
        .try_into()
        .map_err(|_| CoreError::UnexpectedEndOfStructure)?;
    Ok(u16::from_le_bytes(raw))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, CoreError> {
    let end = offset.checked_add(4).ok_or(CoreError::ArithmeticOverflow)?;
    let raw: [u8; 4] = bytes
        .get(offset..end)
        .ok_or(CoreError::UnexpectedEndOfStructure)?
        .try_into()
        .map_err(|_| CoreError::UnexpectedEndOfStructure)?;
    Ok(u32::from_le_bytes(raw))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, CoreError> {
    let end = offset.checked_add(8).ok_or(CoreError::ArithmeticOverflow)?;
    let raw: [u8; 8] = bytes
        .get(offset..end)
        .ok_or(CoreError::UnexpectedEndOfStructure)?
        .try_into()
        .map_err(|_| CoreError::UnexpectedEndOfStructure)?;
    Ok(u64::from_le_bytes(raw))
}

#[derive(Debug)]
pub enum CoreError {
    Io(io::Error),
    View(ViewError),
    BadMagic(u32),
    InvalidFilesystem(&'static str),
    UnsupportedFilesystem(&'static str),
    UnsupportedInode(&'static str),
    IncompatibleReplacement(&'static str),
    ReplacementSizeMismatch {
        expected: u64,
        actual: u64,
    },
    CompressionDoesNotFit {
        head_lcn: usize,
        encoded: usize,
        capacity: usize,
    },
    CompressionValidationFailed,
    InvalidPath(&'static str),
    PathNotFound(String),
    NotDirectory(u64),
    NotRegularFile(u64),
    CorruptDirectory,
    UnexpectedEndOfImage,
    UnexpectedEndOfStructure,
    ArithmeticOverflow,
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "EROFS compact-core I/O error: {error}"),
            Self::View(error) => write!(f, "Loom effective-view error: {error}"),
            Self::BadMagic(magic) => write!(f, "invalid EROFS magic {magic:#010x}"),
            Self::InvalidFilesystem(reason) => write!(f, "invalid compact EROFS: {reason}"),
            Self::UnsupportedFilesystem(reason) => write!(f, "unsupported compact EROFS: {reason}"),
            Self::UnsupportedInode(reason) => write!(f, "unsupported compact inode: {reason}"),
            Self::IncompatibleReplacement(reason) => {
                write!(f, "incompatible compact replacement: {reason}")
            }
            Self::ReplacementSizeMismatch { expected, actual } => write!(
                f,
                "replacement size mismatch: expected {expected} bytes, got {actual}"
            ),
            Self::CompressionDoesNotFit {
                head_lcn,
                encoded,
                capacity,
            } => write!(
                f,
                "raw LZ4 extent at HEAD lcluster {head_lcn} does not fit existing pcluster: encoded {encoded} bytes, capacity {capacity}"
            ),
            Self::CompressionValidationFailed => write!(f, "compact raw LZ4 round-trip validation failed"),
            Self::InvalidPath(reason) => write!(f, "invalid EROFS path: {reason}"),
            Self::PathNotFound(name) => write!(f, "EROFS path component not found: {name:?}"),
            Self::NotDirectory(nid) => write!(f, "EROFS nid {nid} is not a directory"),
            Self::NotRegularFile(nid) => write!(f, "EROFS nid {nid} is not a regular file"),
            Self::CorruptDirectory => write!(f, "malformed EROFS directory block"),
            Self::UnexpectedEndOfImage => write!(f, "EROFS reference lies beyond image bytes"),
            Self::UnexpectedEndOfStructure => write!(f, "unexpected end of EROFS structure"),
            Self::ArithmeticOverflow => write!(f, "integer overflow while parsing compact EROFS"),
        }
    }
}

impl std::error::Error for CoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::View(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_regions_guarantee_real_2b_pack_for_24_lclusters() {
        for modulo in [0_u64, 8, 16, 24] {
            let regions = compact_regions(modulo, 24).unwrap();
            assert_eq!(regions.compact_2b, 16);
        }
    }

    #[test]
    fn single_and_multi_topologies_share_compatibility_rules() {
        let single = Topology {
            nid: 1,
            logical_size: 8192,
            algorithm: 0,
            advise: ADVISE_COMPACTED_2B,
            logical_lclusters: 2,
            compact_2b_entries: 0,
            heads: vec![Head {
                lcn: 0,
                pcluster: 10,
            }],
        };
        let mut relocated = single.clone();
        relocated.heads[0].pcluster = 100;
        assert!(validate_compatible_topology(&single, &relocated).is_ok());
    }

    #[test]
    fn later_extent_codec_failure_happens_before_view_construction() {
        let good = vec![b'Z'; 32768];
        assert!(encode_extent(0, &good).is_ok());

        let mut state = 0x5354_3137_u32;
        let mut bad = vec![0_u8; 32768];
        for byte in &mut bad {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *byte = state.to_le_bytes()[0];
        }
        assert!(matches!(
            encode_extent(8, &bad),
            Err(CoreError::CompressionDoesNotFit { head_lcn: 8, .. })
        ));
    }
}
