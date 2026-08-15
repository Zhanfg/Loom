#![forbid(unsafe_code)]

#[path = "compact_core.rs"]
pub(crate) mod shared_core;

use loom_map::LoomMap;
use shared_core::{CompiledCore, CoreError};
use std::path::Path;

pub type IndexError = CoreError;

#[derive(Debug)]
pub struct CompiledSwap {
    pub map: LoomMap,
    pub shadow: Vec<u8>,
    pub block_size: u32,
    pub origin_nid: u64,
    pub origin_pcluster: u64,
    pub replacement_pcluster: u64,
    pub encoded_bytes: usize,
    pub logical_lclusters: usize,
    pub compact_2b_entries: usize,
    pub shadow_blocks: usize,
}

/// Compiles the compatibility single-pcluster compact EROFS oracle path.
///
/// Parsing, topology validation and block mapping are performed by the unified compact core;
/// this adapter only preserves the Stage 12-15 scalar API and rejects recovered multi-pcluster
/// files before returning a compiled artifact.
///
/// # Errors
/// Returns [`IndexError`] for malformed/unsupported compact EROFS, incompatible replacement
/// topology, effective-view failures, or when the recovered file has more than one pcluster.
pub fn compile_pcluster_swap(
    origin_path: &Path,
    target_path: &str,
    replacement_image_path: &Path,
) -> Result<CompiledSwap, IndexError> {
    into_single(shared_core::compile_oracle(
        origin_path,
        target_path,
        replacement_image_path,
    )?)
}

impl CompiledSwap {
    /// Compiles the compatibility single-pcluster self-encoding path.
    ///
    /// # Errors
    /// Returns [`IndexError`] for malformed/unsupported topology, replacement-size mismatch,
    /// LZ4 footprint/validation failure, view errors, or a recovered multi-pcluster file.
    pub fn compile_lz4_replacement(
        origin_path: &Path,
        target_path: &str,
        replacement_path: &Path,
    ) -> Result<Self, IndexError> {
        into_single(shared_core::compile_lz4(
            origin_path,
            target_path,
            replacement_path,
        )?)
    }
}

fn into_single(compiled: CompiledCore) -> Result<CompiledSwap, IndexError> {
    if compiled.origin_pclusters.len() != 1
        || compiled.replacement_pclusters.len() != 1
        || compiled.head_lclusters.len() != 1
        || compiled.encoded_bytes.len() != 1
        || compiled.shadow_blocks != 1
    {
        return Err(CoreError::UnsupportedInode(
            "single compact mode requires exactly one encoded physical block; use multi mode",
        ));
    }

    let origin_pcluster = compiled.origin_pclusters[0];
    let replacement_pcluster = compiled.replacement_pclusters[0];
    let encoded_bytes = compiled.encoded_bytes[0];
    Ok(CompiledSwap {
        map: compiled.map,
        shadow: compiled.shadow,
        block_size: compiled.block_size,
        origin_nid: compiled.origin_nid,
        origin_pcluster,
        replacement_pcluster,
        encoded_bytes,
        logical_lclusters: compiled.logical_lclusters,
        compact_2b_entries: compiled.compact_2b_entries,
        shadow_blocks: compiled.shadow_blocks,
    })
}
