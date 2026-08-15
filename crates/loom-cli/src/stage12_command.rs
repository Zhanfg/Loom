#![forbid(unsafe_code)]

use loom_erofs::{
    compile_compact_pcluster_swap, compile_multi_pcluster_swap, CompiledCompactSwap,
};
use std::error::Error;
use std::fs;
use std::path::Path;

pub(crate) fn command(args: &mut impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let first = required(args, "--encode, --multi, or origin compact EROFS image")?;
    if first == "--multi" {
        return command_multi(args);
    }

    let (encode, origin_image) = if first == "--encode" {
        (true, required(args, "origin compact EROFS image")?)
    } else {
        (false, first)
    };
    let target_path = required(args, "target path")?;
    let replacement = required(
        args,
        if encode {
            "plain replacement payload"
        } else {
            "encoded replacement compact EROFS image"
        },
    )?;
    let shadow_output = required(args, "shadow pack output")?;
    let origin_device = required(args, "origin block device")?;
    let shadow_device = required(args, "shadow block device")?;
    let table_output = required(args, "dm table output")?;
    ensure_no_extra_args(args)?;

    let compiled = if encode {
        CompiledCompactSwap::compile_lz4_replacement(
            Path::new(&origin_image),
            &target_path,
            Path::new(&replacement),
        )?
    } else {
        compile_compact_pcluster_swap(
            Path::new(&origin_image),
            &target_path,
            Path::new(&replacement),
        )?
    };
    fs::write(&shadow_output, &compiled.shadow)?;
    let table = compiled
        .map
        .to_dm_linear_table(&origin_device, &shadow_device)?;
    fs::write(&table_output, table)?;

    println!(
        "erofs compact pcluster compiled: mode={} origin_nid={} origin_pcluster={} replacement_pcluster={} block_size={} encoded_bytes={} logical_lclusters={} compact_2b_entries={} shadow_blocks={} shadow_bytes={}",
        if encode { "encode" } else { "oracle" },
        compiled.origin_nid,
        compiled.origin_pcluster,
        compiled.replacement_pcluster,
        compiled.block_size,
        compiled.encoded_bytes,
        compiled.logical_lclusters,
        compiled.compact_2b_entries,
        compiled.shadow_blocks,
        compiled.shadow.len()
    );
    Ok(())
}

fn command_multi(args: &mut impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let origin_image = required(args, "origin multi-pcluster compact EROFS image")?;
    let target_path = required(args, "target path")?;
    let replacement_image = required(args, "replacement multi-pcluster compact EROFS image")?;
    let shadow_output = required(args, "shadow pack output")?;
    let origin_device = required(args, "origin block device")?;
    let shadow_device = required(args, "shadow block device")?;
    let table_output = required(args, "dm table output")?;
    ensure_no_extra_args(args)?;

    let compiled = compile_multi_pcluster_swap(
        Path::new(&origin_image),
        &target_path,
        Path::new(&replacement_image),
    )?;
    fs::write(&shadow_output, &compiled.shadow)?;
    let table = compiled
        .map
        .to_dm_linear_table(&origin_device, &shadow_device)?;
    fs::write(&table_output, table)?;

    println!(
        "erofs compact pcluster compiled: mode=multi origin_nid={} origin_pcluster={} replacement_pcluster={} block_size={} physical_pclusters={} logical_lclusters={} compact_2b_entries={} head_lclusters={:?} origin_pclusters={:?} replacement_pclusters={:?} shadow_blocks={} shadow_bytes={}",
        compiled.origin_nid,
        compiled.origin_pcluster,
        compiled.replacement_pcluster,
        compiled.block_size,
        compiled.physical_pclusters,
        compiled.logical_lclusters,
        compiled.compact_2b_entries,
        compiled.head_lclusters,
        compiled.origin_pclusters,
        compiled.replacement_pclusters,
        compiled.shadow_blocks,
        compiled.shadow.len()
    );
    Ok(())
}

fn required(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String, Box<dyn Error>> {
    args.next()
        .ok_or_else(|| format!("missing required argument: {name}").into())
}

fn ensure_no_extra_args(args: &mut impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    if let Some(extra) = args.next() {
        return Err(format!("unexpected extra argument: {extra}").into());
    }
    Ok(())
}
