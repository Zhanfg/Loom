#![forbid(unsafe_code)]

use loom_erofs::compile_lz4_replacement;
use std::error::Error;
use std::fs;
use std::path::Path;

pub(crate) fn command(args: &mut impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let origin_image = required(args, "origin compressed EROFS image")?;
    let target_path = required(args, "target path")?;
    let replacement_payload = required(args, "replacement payload file")?;
    let shadow_output = required(args, "shadow pack output")?;
    let origin_device = required(args, "origin block device")?;
    let shadow_device = required(args, "shadow block device")?;
    let table_output = required(args, "dm table output")?;
    ensure_no_extra_args(args)?;

    let compiled = compile_lz4_replacement(
        Path::new(&origin_image),
        &target_path,
        Path::new(&replacement_payload),
    )?;
    fs::write(&shadow_output, &compiled.shadow)?;
    let table = compiled
        .map
        .to_dm_linear_table(&origin_device, &shadow_device)?;
    fs::write(&table_output, table)?;

    println!(
        "erofs LZ4 replacement compiled: nid={} pcluster={} block_size={} encoded_bytes={} shadow_blocks={} shadow_bytes={}",
        compiled.nid,
        compiled.pcluster,
        compiled.block_size,
        compiled.encoded_bytes,
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
