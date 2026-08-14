#![forbid(unsafe_code)]

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
        "pack-block" => {
            let input = required(&mut args, "input file")?;
            let output = required(&mut args, "output pack")?;
            let block_size = parse_usize(&required(&mut args, "block size")?, "block size")?;
            ensure_no_extra_args(&mut args)?;
            loom_pack::pack_file(Path::new(&input), Path::new(&output), block_size)?;
        }
        "map-single" => {
            let total = parse_u64(&required(&mut args, "total sectors")?, "total sectors")?;
            let start = parse_u64(
                &required(&mut args, "replacement start sector")?,
                "replacement start sector",
            )?;
            let length = parse_u64(
                &required(&mut args, "replacement sector count")?,
                "replacement sector count",
            )?;
            let shadow_start = parse_u64(
                &required(&mut args, "shadow start sector")?,
                "shadow start sector",
            )?;
            let origin_device = required(&mut args, "origin device")?;
            let shadow_device = required(&mut args, "shadow device")?;
            let output = required(&mut args, "output table")?;
            ensure_no_extra_args(&mut args)?;

            let map = LoomMap::single_replacement(
                SectorCount(total),
                Sector(start),
                SectorCount(length),
                Sector(shadow_start),
            )?;
            let table = map.to_dm_linear_table(&origin_device, &shadow_device)?;
            fs::write(output, table)?;
        }
        "help" | "--help" | "-h" => print_usage(),
        other => return Err(format!("unknown command {other:?}").into()),
    }

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
        "Loom Stage 0\n\n\
         Usage:\n\
           loom pack-block <input> <output-pack> <block-size>\n\
           loom map-single <total-sectors> <start-sector> <sector-count> <shadow-start-sector> \\\n<origin-device> <shadow-device> <output-table>\n"
    );
}
