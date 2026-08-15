#![forbid(unsafe_code)]

use loom_ext4::{compile_resize_within_allocation, compile_same_size_replacement};
use loom_map::LoomMap;
use loom_types::{Sector, SectorCount};
use std::env;
use std::error::Error;
use std::fs;
use std::path::Path;

fn main() {
    if let Err(error) = run() {
        eprintln!("loom: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = env::args();
    let _program = args.next();
    let Some(command) = args.next() else {
        print_usage();
        return Ok(());
    };

    match command.as_str() {
        "pack-block" => command_pack_block(&mut args)?,
        "map-single" => command_map_single(&mut args)?,
        "ext4-replace" => command_ext4_replace(&mut args)?,
        "ext4-resize" => command_ext4_resize(&mut args)?,
        "help" | "--help" | "-h" => print_usage(),
        other => return Err(format!("unknown command {other:?}").into()),
    }

    Ok(())
}

fn command_pack_block(args: &mut impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let input = required(args, "input file")?;
    let output = required(args, "output pack")?;
    let block_size = parse_usize(&required(args, "block size")?, "block size")?;
    ensure_no_extra_args(args)?;
    loom_pack::pack_file(Path::new(&input), Path::new(&output), block_size)?;
    Ok(())
}

fn command_map_single(args: &mut impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let total = parse_u64(&required(args, "total sectors")?, "total sectors")?;
    let start = parse_u64(
        &required(args, "replacement start sector")?,
        "replacement start sector",
    )?;
    let length = parse_u64(
        &required(args, "replacement sector count")?,
        "replacement sector count",
    )?;
    let shadow_start = parse_u64(
        &required(args, "shadow start sector")?,
        "shadow start sector",
    )?;
    let origin_device = required(args, "origin device")?;
    let shadow_device = required(args, "shadow device")?;
    let output = required(args, "output table")?;
    ensure_no_extra_args(args)?;

    let map = LoomMap::single_replacement(
        SectorCount(total),
        Sector(start),
        SectorCount(length),
        Sector(shadow_start),
    )?;
    let table = map.to_dm_linear_table(&origin_device, &shadow_device)?;
    fs::write(output, table)?;
    Ok(())
}

fn command_ext4_replace(args: &mut impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let origin_image = required(args, "origin ext4 image")?;
    let target_path = required(args, "target path")?;
    let replacement = required(args, "replacement file")?;
    let shadow_output = required(args, "shadow pack output")?;
    let origin_device = required(args, "origin block device")?;
    let shadow_device = required(args, "shadow block device")?;
    let table_output = required(args, "dm table output")?;
    ensure_no_extra_args(args)?;

    let compiled = compile_same_size_replacement(
        Path::new(&origin_image),
        &target_path,
        Path::new(&replacement),
    )?;
    fs::write(&shadow_output, &compiled.shadow)?;
    let table = compiled
        .map
        .to_dm_linear_table(&origin_device, &shadow_device)?;
    fs::write(&table_output, table)?;

    println!(
        "ext4 replacement compiled: inode={} block_size={} data_blocks={} shadow_blocks={} shadow_bytes={}",
        compiled.inode,
        compiled.block_size,
        compiled.data_blocks,
        compiled.shadow_blocks,
        compiled.shadow.len()
    );
    Ok(())
}

fn command_ext4_resize(args: &mut impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let origin_image = required(args, "origin ext4 image")?;
    let target_path = required(args, "target path")?;
    let replacement = required(args, "replacement file")?;
    let shadow_output = required(args, "shadow pack output")?;
    let origin_device = required(args, "origin block device")?;
    let shadow_device = required(args, "shadow block device")?;
    let table_output = required(args, "dm table output")?;
    ensure_no_extra_args(args)?;

    let compiled = compile_resize_within_allocation(
        Path::new(&origin_image),
        &target_path,
        Path::new(&replacement),
    )?;
    fs::write(&shadow_output, &compiled.shadow)?;
    let table = compiled
        .map
        .to_dm_linear_table(&origin_device, &shadow_device)?;
    fs::write(&table_output, table)?;

    println!(
        "ext4 resize compiled: inode={} original_size={} effective_size={} block_size={} data_blocks={} data_shadow_blocks={} metadata_blocks={} shadow_blocks={} shadow_bytes={}",
        compiled.inode,
        compiled.original_size,
        compiled.effective_size,
        compiled.block_size,
        compiled.data_blocks,
        compiled.data_shadow_blocks,
        compiled.metadata_blocks,
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

fn parse_u64(value: &str, name: &str) -> Result<u64, Box<dyn Error>> {
    value
        .parse::<u64>()
        .map_err(|error| format!("invalid {name} {value:?}: {error}").into())
}

fn parse_usize(value: &str, name: &str) -> Result<usize, Box<dyn Error>> {
    value
        .parse::<usize>()
        .map_err(|error| format!("invalid {name} {value:?}: {error}").into())
}

fn print_usage() {
    eprintln!(
        "Loom Stage 2\n\n\
         Usage:\n\
           loom pack-block <input> <output-pack> <block-size>\n\
           loom map-single <total-sectors> <start-sector> <sector-count> <shadow-start-sector> \\\n<origin-device> <shadow-device> <output-table>\n\
           loom ext4-replace <origin-image> <target-path> <replacement> <shadow-pack> \\\n<origin-device> <shadow-device> <output-table>\n\
           loom ext4-resize <origin-image> <target-path> <replacement> <shadow-pack> \\\n<origin-device> <shadow-device> <output-table>\n"
    );
}
