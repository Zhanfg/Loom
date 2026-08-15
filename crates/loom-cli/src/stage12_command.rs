#![forbid(unsafe_code)]

use loom_erofs::compile_compact_pcluster_swap;
use std::error::Error;
use std::fs;
use std::path::Path;

pub(crate) fn command(args: &mut impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let origin_image = required(args, "origin compact EROFS image")?;
    let target_path = required(args, "target path")?;
    let replacement_image = required(args, "encoded replacement compact EROFS image")?;
    let shadow_output = required(args, "shadow pack output")?;
    let origin_device = required(args, "origin block device")?;
    let shadow_device = required(args, "shadow block device")?;
    let table_output = required(args, "dm table output")?;
    ensure_no_extra_args(args)?;

    let compiled = compile_compact_pcluster_swap(
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
        "erofs compact pcluster swap compiled: origin_nid={} origin_pcluster={} replacement_pcluster={} block_size={} shadow_blocks={} shadow_bytes={}",
        compiled.origin_nid,
        compiled.origin_pcluster,
        compiled.replacement_pcluster,
        compiled.block_size,
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
