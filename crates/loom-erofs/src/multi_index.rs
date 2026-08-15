#![forbid(unsafe_code)]

use crate::compact_core::{self, CompiledCore, CoreError};
use loom_map::LoomMap;
use std::path::Path;

pub type MultiIndexError = CoreError;

#[derive(Debug)]
pub struct CompiledMultiSwap {
    pub map: LoomMap,
    pub shadow: Vec<u8>,
    pub block_size: u32,
    pub origin_nid: u64,
    pub origin_pcluster: u64,
    pub replacement_pcluster: u64,
    pub origin_pclusters: Vec<u64>,
    pub replacement_pclusters: Vec<u64>,
    pub head_lclusters: Vec<usize>,
    pub encoded_bytes: Vec<usize>,
    pub physical_pclusters: usize,
    pub logical_lclusters: usize,
    pub compact_2b_entries: usize,
    pub shadow_blocks: usize,
}

/// Compiles a compact EROFS oracle replacement over any supported one-block-per-extent
/// topology. The unified compact core handles both the historical single-pcluster shape and
/// the Stage 16+ multi-pcluster shape.
///
/// # Errors
/// Returns [`MultiIndexError`] for malformed/unsupported topology, incompatible replacement
/// images, I/O failures, or effective-view failures.
pub fn compile_multi_pcluster_swap(
    origin_path: &Path,
    target_path: &str,
    replacement_image_path: &Path,
) -> Result<CompiledMultiSwap, MultiIndexError> {
    Ok(from_core(compact_core::compile_oracle(
        origin_path,
        target_path,
        replacement_image_path,
    )?)?)
}

impl CompiledMultiSwap {
    /// Self-encodes all recovered logical extents through the unified compact core.
    ///
    /// # Errors
    /// Returns [`MultiIndexError`] for malformed/unsupported topology, replacement-size
    /// mismatch, per-extent LZ4 footprint/validation failure, I/O, or view errors.
    pub fn compile_lz4_replacement(
        origin_path: &Path,
        target_path: &str,
        replacement_path: &Path,
    ) -> Result<Self, MultiIndexError> {
        from_core(compact_core::compile_lz4(
            origin_path,
            target_path,
            replacement_path,
        )?)
    }
}

fn from_core(compiled: CompiledCore) -> Result<CompiledMultiSwap, MultiIndexError> {
    let origin_pcluster = *compiled
        .origin_pclusters
        .first()
        .ok_or(CoreError::InvalidFilesystem("compressed topology has no HEAD"))?;
    let replacement_pcluster = *compiled
        .replacement_pclusters
        .first()
        .ok_or(CoreError::InvalidFilesystem("replacement topology has no HEAD"))?;
    let physical_pclusters = compiled.origin_pclusters.len();
    if physical_pclusters != compiled.replacement_pclusters.len()
        || physical_pclusters != compiled.head_lclusters.len()
        || physical_pclusters != compiled.encoded_bytes.len()
        || physical_pclusters != compiled.shadow_blocks
    {
        return Err(CoreError::InvalidFilesystem(
            "compact adapter received inconsistent compiled vectors",
        ));
    }

    Ok(CompiledMultiSwap {
        map: compiled.map,
        shadow: compiled.shadow,
        block_size: compiled.block_size,
        origin_nid: compiled.origin_nid,
        origin_pcluster,
        replacement_pcluster,
        origin_pclusters: compiled.origin_pclusters,
        replacement_pclusters: compiled.replacement_pclusters,
        head_lclusters: compiled.head_lclusters,
        encoded_bytes: compiled.encoded_bytes,
        physical_pclusters,
        logical_lclusters: compiled.logical_lclusters,
        compact_2b_entries: compiled.compact_2b_entries,
        shadow_blocks: compiled.shadow_blocks,
    })
}
