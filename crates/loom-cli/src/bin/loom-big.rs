#![forbid(unsafe_code)]

use loom_erofs::CompiledMultiSwap;
use std::error::Error;
use std::fs;
use std::path::Path;

fn main() {
    if let Err(error) = run() {
        eprintln!("loom-big: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let origin_image = required(&mut args, "origin big-pcluster EROFS image")?;
    let target_path = required(&mut args, "target path")?;
    let replacement_image = required(&mut args, "replacement big-pcluster EROFS image")?;
    let shadow_output = required(&mut args, "shadow pack output")?;
    let origin_device = required(&mut args, "origin block device")?;
    let shadow_device = required(&mut args, "shadow block device")?;
    let table_output = required(&mut args, "dm table output")?;
    ensure_no_extra_args(&mut args)?;

    let compiled = CompiledMultiSwap::compile_big_pcluster_swap(
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
        "erofs big pcluster compiled: mode=big origin_nid={} origin_pcluster={} replacement_pcluster={} block_size={} physical_blocks={} logical_lclusters={} compact_2b_entries={} head_lclusters={:?} origin_blocks={:?} replacement_blocks={:?} shadow_blocks={} shadow_bytes={}",
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

fn required(
    args: &mut impl Iterator<Item = String>,
    name: &str,
) -> Result<String, Box<dyn Error>> {
    args.next()
        .ok_or_else(|| format!("missing required argument: {name}").into())
}

fn ensure_no_extra_args(args: &mut impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    if let Some(extra) = args.next() {
        return Err(format!("unexpected extra argument: {extra}").into());
    }
    Ok(())
}
