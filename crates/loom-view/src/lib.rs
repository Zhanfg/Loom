#![forbid(unsafe_code)]

use loom_map::{LoomMap, MapError, ReplacementExtent};
use loom_types::{Sector, SectorCount, Source};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const SECTOR_SIZE: u64 = 512;

#[derive(Debug)]
pub struct CompiledView {
    pub map: LoomMap,
    pub shadow: Vec<u8>,
    pub block_size: u32,
    pub shadow_blocks: usize,
}

/// Transaction-owned effective block view.
///
/// The origin is opened read-only. Blocks enter `dirty` only when an operation mutates
/// them. First mutation promotes stock bytes into transaction-owned memory; subsequent
/// mutations coalesce on that same block. Finalization emits dirty blocks in logical-block
/// order so the resulting shadow pack and Loom map are deterministic.
pub struct EffectiveBlockStore {
    origin: File,
    image_bytes: u64,
    block_size: u32,
    dirty: BTreeMap<u64, Vec<u8>>,
}

impl EffectiveBlockStore {
    /// Opens a read-only origin as an empty effective view.
    ///
    /// # Errors
    /// Returns [`ViewError`] when the origin geometry is unsuitable or I/O fails.
    pub fn open(origin_path: &Path, block_size: u32) -> Result<Self, ViewError> {
        validate_block_size(block_size)?;
        let origin = File::open(origin_path).map_err(ViewError::Io)?;
        let image_bytes = origin.metadata().map_err(ViewError::Io)?.len();
        if image_bytes == 0 || image_bytes % u64::from(block_size) != 0 {
            return Err(ViewError::OriginSizeNotBlockAligned {
                bytes: image_bytes,
                block_size,
            });
        }
        Ok(Self {
            origin,
            image_bytes,
            block_size,
            dirty: BTreeMap::new(),
        })
    }

    /// Rehydrates a previously compiled Loom map/shadow pair into a mutable effective view.
    ///
    /// Only shadow-backed filesystem blocks are imported into transaction-owned memory;
    /// origin-backed ranges remain lazy and continue to read from the authoritative origin.
    ///
    /// # Errors
    /// Returns [`ViewError`] for size/alignment mismatches, malformed shadow references,
    /// duplicate logical blocks, or I/O errors.
    pub fn from_compiled(
        origin_path: &Path,
        block_size: u32,
        map: &LoomMap,
        shadow: &[u8],
    ) -> Result<Self, ViewError> {
        let mut store = Self::open(origin_path, block_size)?;
        let total_bytes = map
            .total_sectors()
            .0
            .checked_mul(SECTOR_SIZE)
            .ok_or(ViewError::ArithmeticOverflow)?;
        if total_bytes != store.image_bytes {
            return Err(ViewError::MapSizeMismatch {
                origin_bytes: store.image_bytes,
                map_bytes: total_bytes,
            });
        }
        let block_size_usize =
            usize::try_from(block_size).map_err(|_| ViewError::ArithmeticOverflow)?;
        if shadow.len() % block_size_usize != 0 {
            return Err(ViewError::ShadowSizeNotBlockAligned {
                bytes: shadow.len(),
                block_size,
            });
        }
        let sectors_per_block = u64::from(block_size) / SECTOR_SIZE;

        for extent in map.extents() {
            if extent.source != Source::Shadow {
                continue;
            }
            if extent.logical_start.0 % sectors_per_block != 0
                || extent.sector_count.0 % sectors_per_block != 0
                || extent.source_start.0 % sectors_per_block != 0
            {
                return Err(ViewError::UnalignedShadowExtent);
            }
            let block_count = extent.sector_count.0 / sectors_per_block;
            let logical_block_start = extent.logical_start.0 / sectors_per_block;
            for index in 0..block_count {
                let logical_block = logical_block_start
                    .checked_add(index)
                    .ok_or(ViewError::ArithmeticOverflow)?;
                let shadow_sector = extent
                    .source_start
                    .0
                    .checked_add(
                        index
                            .checked_mul(sectors_per_block)
                            .ok_or(ViewError::ArithmeticOverflow)?,
                    )
                    .ok_or(ViewError::ArithmeticOverflow)?;
                let byte_start_u64 = shadow_sector
                    .checked_mul(SECTOR_SIZE)
                    .ok_or(ViewError::ArithmeticOverflow)?;
                let byte_end_u64 = byte_start_u64
                    .checked_add(u64::from(block_size))
                    .ok_or(ViewError::ArithmeticOverflow)?;
                let byte_start =
                    usize::try_from(byte_start_u64).map_err(|_| ViewError::ArithmeticOverflow)?;
                let byte_end =
                    usize::try_from(byte_end_u64).map_err(|_| ViewError::ArithmeticOverflow)?;
                let bytes = shadow
                    .get(byte_start..byte_end)
                    .ok_or(ViewError::ShadowOutOfBounds)?
                    .to_vec();
                if store.dirty.insert(logical_block, bytes).is_some() {
                    return Err(ViewError::DuplicateLogicalBlock(logical_block));
                }
            }
        }
        Ok(store)
    }

    #[must_use]
    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    #[must_use]
    pub fn block_count(&self) -> u64 {
        self.image_bytes / u64::from(self.block_size)
    }

    #[must_use]
    pub fn dirty_blocks(&self) -> usize {
        self.dirty.len()
    }

    /// Returns a copy of the current effective block, preferring transaction-owned bytes.
    ///
    /// # Errors
    /// Returns [`ViewError`] for out-of-range blocks or origin I/O failures.
    pub fn read_block(&mut self, block: u64) -> Result<Vec<u8>, ViewError> {
        self.validate_block(block)?;
        if let Some(bytes) = self.dirty.get(&block) {
            return Ok(bytes.clone());
        }
        read_origin_block(&mut self.origin, self.block_size, block)
    }

    /// Returns mutable effective bytes, promoting the origin block on first mutation.
    ///
    /// # Errors
    /// Returns [`ViewError`] for out-of-range blocks or origin I/O failures.
    pub fn block_mut(&mut self, block: u64) -> Result<&mut [u8], ViewError> {
        self.validate_block(block)?;
        if !self.dirty.contains_key(&block) {
            let bytes = read_origin_block(&mut self.origin, self.block_size, block)?;
            self.dirty.insert(block, bytes);
        }
        self.dirty
            .get_mut(&block)
            .map(Vec::as_mut_slice)
            .ok_or(ViewError::MissingPromotedBlock(block))
    }

    /// Finalizes the transaction into a deterministic Loom map and shadow pack.
    /// Blocks whose final bytes equal stock are elided.
    ///
    /// # Errors
    /// Returns [`ViewError`] for arithmetic, map, or origin I/O failures.
    pub fn finalize(self) -> Result<CompiledView, ViewError> {
        let Self {
            mut origin,
            image_bytes,
            block_size,
            dirty,
        } = self;
        let sectors_per_block = u64::from(block_size) / SECTOR_SIZE;
        let mut shadow = Vec::new();
        let mut replacements = Vec::with_capacity(dirty.len());

        for (logical_block, effective) in dirty {
            let stock = read_origin_block(&mut origin, block_size, logical_block)?;
            if effective == stock {
                continue;
            }
            let shadow_start = u64::try_from(shadow.len())
                .map_err(|_| ViewError::ArithmeticOverflow)?
                / SECTOR_SIZE;
            let logical_start = logical_block
                .checked_mul(sectors_per_block)
                .ok_or(ViewError::ArithmeticOverflow)?;
            replacements.push(ReplacementExtent {
                logical_start: Sector(logical_start),
                sector_count: SectorCount(sectors_per_block),
                shadow_start: Sector(shadow_start),
            });
            shadow.extend_from_slice(&effective);
        }

        let total_sectors = image_bytes / SECTOR_SIZE;
        let map = LoomMap::from_replacements(SectorCount(total_sectors), &replacements)
            .map_err(ViewError::Map)?;
        let block_size_usize =
            usize::try_from(block_size).map_err(|_| ViewError::ArithmeticOverflow)?;
        let shadow_blocks = shadow.len() / block_size_usize;
        Ok(CompiledView {
            map,
            shadow,
            block_size,
            shadow_blocks,
        })
    }

    fn validate_block(&self, block: u64) -> Result<(), ViewError> {
        if block >= self.block_count() {
            return Err(ViewError::BlockOutOfBounds {
                block,
                block_count: self.block_count(),
            });
        }
        Ok(())
    }
}

fn validate_block_size(block_size: u32) -> Result<(), ViewError> {
    if block_size == 0 || u64::from(block_size) % SECTOR_SIZE != 0 {
        return Err(ViewError::InvalidBlockSize(block_size));
    }
    Ok(())
}

fn read_origin_block(origin: &mut File, block_size: u32, block: u64) -> Result<Vec<u8>, ViewError> {
    let offset = block
        .checked_mul(u64::from(block_size))
        .ok_or(ViewError::ArithmeticOverflow)?;
    origin
        .seek(SeekFrom::Start(offset))
        .map_err(ViewError::Io)?;
    let mut bytes =
        vec![0_u8; usize::try_from(block_size).map_err(|_| ViewError::ArithmeticOverflow)?];
    origin.read_exact(&mut bytes).map_err(ViewError::Io)?;
    Ok(bytes)
}

#[derive(Debug)]
pub enum ViewError {
    Io(io::Error),
    InvalidBlockSize(u32),
    OriginSizeNotBlockAligned { bytes: u64, block_size: u32 },
    MapSizeMismatch { origin_bytes: u64, map_bytes: u64 },
    ShadowSizeNotBlockAligned { bytes: usize, block_size: u32 },
    UnalignedShadowExtent,
    ShadowOutOfBounds,
    DuplicateLogicalBlock(u64),
    BlockOutOfBounds { block: u64, block_count: u64 },
    MissingPromotedBlock(u64),
    ArithmeticOverflow,
    Map(MapError),
}

impl fmt::Display for ViewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "effective-view I/O error: {error}"),
            Self::InvalidBlockSize(size) => write!(f, "invalid effective-view block size {size}"),
            Self::OriginSizeNotBlockAligned { bytes, block_size } => write!(
                f,
                "origin size {bytes} is not a non-zero multiple of block size {block_size}"
            ),
            Self::MapSizeMismatch {
                origin_bytes,
                map_bytes,
            } => write!(
                f,
                "Loom map covers {map_bytes} bytes but origin contains {origin_bytes} bytes"
            ),
            Self::ShadowSizeNotBlockAligned { bytes, block_size } => write!(
                f,
                "shadow size {bytes} is not aligned to block size {block_size}"
            ),
            Self::UnalignedShadowExtent => {
                write!(f, "shadow extent is not filesystem-block aligned")
            }
            Self::ShadowOutOfBounds => {
                write!(f, "Loom map references bytes beyond the shadow pack")
            }
            Self::DuplicateLogicalBlock(block) => {
                write!(f, "compiled map contains duplicate logical block {block}")
            }
            Self::BlockOutOfBounds { block, block_count } => write!(
                f,
                "logical block {block} lies outside effective view with {block_count} blocks"
            ),
            Self::MissingPromotedBlock(block) => {
                write!(
                    f,
                    "promoted logical block {block} disappeared from transaction state"
                )
            }
            Self::ArithmeticOverflow => write!(f, "effective-view arithmetic overflow"),
            Self::Map(error) => write!(f, "effective-view Loom map error: {error}"),
        }
    }
}

impl std::error::Error for ViewError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Map(error) => Some(error),
            _ => None,
        }
    }
}
