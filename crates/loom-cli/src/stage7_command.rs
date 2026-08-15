#![forbid(unsafe_code)]

use loom_ext4::compile_create_with_selinux_transaction;
use std::error::Error;
use std::fs;
use std::path::Path;

pub(crate) fn command(args: &mut impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let origin_image = required(args, "origin ext4 image")?;
    let target_path = required(args, "new target path")?;
    let payload = required(args, "payload file")?;
    let value_file = required(args, "SELinux context bytes file")?;
    let shadow_output = required(args, "shadow pack output")?;
    let origin_device = required(args, "origin block device")?;
    let shadow_device = required(args, "shadow block device")?;
    let table_output = required(args, "dm table output")?;
    ensure_no_extra_args(args)?;

    let compiled = compile_create_with_selinux_transaction(
        Path::new(&origin_image),
        &target_path,
        Path::new(&payload),
        Path::new(&value_file),
    )?;
    fs::write(&shadow_output, &compiled.shadow)?;
    let table = compiled
        .map
        .to_dm_linear_table(&origin_device, &shadow_device)?;
    fs::write(&table_output, table)?;

    println!(
        "ext4 create-selinux transaction compiled: inode={} block_size={} value_bytes={} shadow_blocks={} shadow_bytes={}",
        compiled.inode,
        compiled.block_size,
        compiled.value_bytes,
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
