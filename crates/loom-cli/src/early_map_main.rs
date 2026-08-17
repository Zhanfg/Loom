#![forbid(unsafe_code)]

use std::env;
use std::error::Error;
use std::fmt;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

const ORIGIN_TOKEN: &str = "__LOOM_ORIGIN__";
const METADATA_TOKEN: &str = "__LOOM_METADATA_DEVICE__";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlatSource {
    Origin,
    Shadow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FlatExtent {
    logical_start: u64,
    sector_count: u64,
    source: FlatSource,
    source_start: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileExtent {
    logical_start: u64,
    physical_start: u64,
    sector_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EarlySource {
    Origin,
    Metadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EarlyExtent {
    logical_start: u64,
    sector_count: u64,
    source: EarlySource,
    source_start: u64,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("loom-early-map: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = env::args();
    let _program = args.next();
    let flat_table_path = required(&mut args, "flat dm table")?;
    let origin_device = required(&mut args, "origin device used by flat table")?;
    let shadow_device = required(&mut args, "aggregate shadow device used by flat table")?;
    let extent_map_path = required(&mut args, "shadow FIEMAP extent map")?;
    let output_path = required(&mut args, "early raw dm table output")?;
    if let Some(extra) = args.next() {
        return Err(format!("unexpected extra argument: {extra}").into());
    }
    if origin_device.is_empty() || shadow_device.is_empty() || origin_device == shadow_device {
        return Err(EarlyMapError::AmbiguousBackingDevice.into());
    }

    let flat_table = fs::read_to_string(&flat_table_path)?;
    let file_extents_text = fs::read_to_string(&extent_map_path)?;
    let flat = parse_flat_table(&flat_table, &origin_device, &shadow_device)?;
    let file_extents = parse_file_extents(&file_extents_text)?;
    let early = lower_to_metadata(&flat, &file_extents)?;
    let table = format_early_table(&early)?;
    write_new(Path::new(&output_path), table.as_bytes())?;

    println!(
        "Loom early map prepared: flat_extents={} early_extents={} metadata_file_extents={} sectors={}",
        flat.len(),
        early.len(),
        file_extents.len(),
        total_sectors(&early)?
    );
    Ok(())
}

fn parse_flat_table(
    table: &str,
    origin_device: &str,
    shadow_device: &str,
) -> Result<Vec<FlatExtent>, EarlyMapError> {
    let mut extents = Vec::new();
    for (line_index, raw) in table.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != 5 || fields[2] != "linear" {
            return Err(EarlyMapError::MalformedFlatTableLine(line_index + 1));
        }
        let source = if fields[3] == origin_device {
            FlatSource::Origin
        } else if fields[3] == shadow_device {
            FlatSource::Shadow
        } else {
            return Err(EarlyMapError::UnknownFlatBacking {
                line: line_index + 1,
                device: fields[3].to_owned(),
            });
        };
        extents.push(FlatExtent {
            logical_start: parse_u64(fields[0], line_index + 1)?,
            sector_count: parse_u64(fields[1], line_index + 1)?,
            source,
            source_start: parse_u64(fields[4], line_index + 1)?,
        });
    }
    validate_flat(&extents)?;
    Ok(extents)
}

fn parse_file_extents(text: &str) -> Result<Vec<FileExtent>, EarlyMapError> {
    let mut extents = Vec::new();
    for (line_index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != 3 {
            return Err(EarlyMapError::MalformedFileExtentLine(line_index + 1));
        }
        extents.push(FileExtent {
            logical_start: parse_u64(fields[0], line_index + 1)?,
            physical_start: parse_u64(fields[1], line_index + 1)?,
            sector_count: parse_u64(fields[2], line_index + 1)?,
        });
    }
    validate_file_extents(&extents)?;
    Ok(extents)
}

fn lower_to_metadata(
    flat: &[FlatExtent],
    file_extents: &[FileExtent],
) -> Result<Vec<EarlyExtent>, EarlyMapError> {
    validate_flat(flat)?;
    validate_file_extents(file_extents)?;
    let mut output = Vec::new();

    for flat_extent in flat {
        match flat_extent.source {
            FlatSource::Origin => output.push(EarlyExtent {
                logical_start: flat_extent.logical_start,
                sector_count: flat_extent.sector_count,
                source: EarlySource::Origin,
                source_start: flat_extent.source_start,
            }),
            FlatSource::Shadow => lower_shadow_extent(*flat_extent, file_extents, &mut output)?,
        }
    }

    let output = merge_adjacent(output)?;
    validate_early(&output)?;
    Ok(output)
}

fn lower_shadow_extent(
    flat: FlatExtent,
    file_extents: &[FileExtent],
    output: &mut Vec<EarlyExtent>,
) -> Result<(), EarlyMapError> {
    let shadow_end = checked_add(flat.source_start, flat.sector_count)?;
    let mut cursor = flat.source_start;

    while cursor < shadow_end {
        let file_extent = file_extents
            .iter()
            .find(|extent| {
                extent.logical_start <= cursor
                    && checked_add(extent.logical_start, extent.sector_count)
                        .is_ok_and(|end| cursor < end)
            })
            .ok_or(EarlyMapError::ShadowCoverageGap(cursor))?;
        let file_end = checked_add(file_extent.logical_start, file_extent.sector_count)?;
        let chunk_end = file_end.min(shadow_end);
        let chunk_count = chunk_end
            .checked_sub(cursor)
            .ok_or(EarlyMapError::ArithmeticOverflow)?;
        if chunk_count == 0 {
            return Err(EarlyMapError::ShadowCoverageGap(cursor));
        }

        let flat_delta = cursor
            .checked_sub(flat.source_start)
            .ok_or(EarlyMapError::ArithmeticOverflow)?;
        let file_delta = cursor
            .checked_sub(file_extent.logical_start)
            .ok_or(EarlyMapError::ArithmeticOverflow)?;
        output.push(EarlyExtent {
            logical_start: checked_add(flat.logical_start, flat_delta)?,
            sector_count: chunk_count,
            source: EarlySource::Metadata,
            source_start: checked_add(file_extent.physical_start, file_delta)?,
        });
        cursor = chunk_end;
    }
    Ok(())
}

fn format_early_table(extents: &[EarlyExtent]) -> Result<String, EarlyMapError> {
    validate_early(extents)?;
    let mut output = String::new();
    for extent in extents {
        let device = match extent.source {
            EarlySource::Origin => ORIGIN_TOKEN,
            EarlySource::Metadata => METADATA_TOKEN,
        };
        writeln!(
            &mut output,
            "{} {} linear {} {}",
            extent.logical_start, extent.sector_count, device, extent.source_start
        )
        .map_err(|_| EarlyMapError::Formatting)?;
    }
    Ok(output)
}

fn validate_flat(extents: &[FlatExtent]) -> Result<(), EarlyMapError> {
    if extents.is_empty() {
        return Err(EarlyMapError::EmptyFlatTable);
    }
    let mut expected = 0_u64;
    for extent in extents {
        if extent.sector_count == 0 {
            return Err(EarlyMapError::ZeroLengthExtent);
        }
        if extent.logical_start != expected {
            return Err(EarlyMapError::NonContiguousFlat {
                expected,
                actual: extent.logical_start,
            });
        }
        checked_add(extent.source_start, extent.sector_count)?;
        expected = checked_add(extent.logical_start, extent.sector_count)?;
    }
    Ok(())
}

fn validate_file_extents(extents: &[FileExtent]) -> Result<(), EarlyMapError> {
    if extents.is_empty() {
        return Err(EarlyMapError::EmptyFileExtentMap);
    }
    let mut expected = 0_u64;
    for extent in extents {
        if extent.sector_count == 0 {
            return Err(EarlyMapError::ZeroLengthExtent);
        }
        if extent.logical_start != expected {
            return Err(EarlyMapError::NonContiguousFile {
                expected,
                actual: extent.logical_start,
            });
        }
        checked_add(extent.physical_start, extent.sector_count)?;
        expected = checked_add(extent.logical_start, extent.sector_count)?;
    }
    Ok(())
}

fn validate_early(extents: &[EarlyExtent]) -> Result<(), EarlyMapError> {
    if extents.is_empty() {
        return Err(EarlyMapError::EmptyEarlyTable);
    }
    let mut expected = 0_u64;
    for extent in extents {
        if extent.sector_count == 0 {
            return Err(EarlyMapError::ZeroLengthExtent);
        }
        if extent.logical_start != expected {
            return Err(EarlyMapError::NonContiguousEarly {
                expected,
                actual: extent.logical_start,
            });
        }
        checked_add(extent.source_start, extent.sector_count)?;
        expected = checked_add(extent.logical_start, extent.sector_count)?;
    }
    Ok(())
}

fn merge_adjacent(extents: Vec<EarlyExtent>) -> Result<Vec<EarlyExtent>, EarlyMapError> {
    let mut merged: Vec<EarlyExtent> = Vec::with_capacity(extents.len());
    for extent in extents {
        if let Some(previous) = merged.last_mut() {
            let previous_logical_end = checked_add(previous.logical_start, previous.sector_count)?;
            let previous_source_end = checked_add(previous.source_start, previous.sector_count)?;
            if previous.source == extent.source
                && previous_logical_end == extent.logical_start
                && previous_source_end == extent.source_start
            {
                previous.sector_count = checked_add(previous.sector_count, extent.sector_count)?;
                continue;
            }
        }
        merged.push(extent);
    }
    Ok(merged)
}

fn total_sectors(extents: &[EarlyExtent]) -> Result<u64, EarlyMapError> {
    let last = extents.last().ok_or(EarlyMapError::EmptyEarlyTable)?;
    checked_add(last.logical_start, last.sector_count)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    if path.exists() {
        return Err(format!("output already exists: {}", path.display()).into());
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.is_dir() {
            return Err(format!("output parent does not exist: {}", parent.display()).into());
        }
    }
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(format!(".loomtmp-{}", std::process::id()));
    let tmp = std::path::PathBuf::from(tmp);
    let _ = fs::remove_file(&tmp);
    fs::write(&tmp, bytes)?;
    if let Err(error) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(error.into());
    }
    Ok(())
}

fn checked_add(left: u64, right: u64) -> Result<u64, EarlyMapError> {
    left.checked_add(right)
        .ok_or(EarlyMapError::ArithmeticOverflow)
}

fn parse_u64(value: &str, line: usize) -> Result<u64, EarlyMapError> {
    value
        .parse::<u64>()
        .map_err(|_| EarlyMapError::InvalidNumber(line))
}

fn required(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String, Box<dyn Error>> {
    args.next()
        .ok_or_else(|| format!("missing required argument: {name}").into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EarlyMapError {
    EmptyFlatTable,
    EmptyFileExtentMap,
    EmptyEarlyTable,
    ZeroLengthExtent,
    MalformedFlatTableLine(usize),
    MalformedFileExtentLine(usize),
    InvalidNumber(usize),
    UnknownFlatBacking { line: usize, device: String },
    AmbiguousBackingDevice,
    NonContiguousFlat { expected: u64, actual: u64 },
    NonContiguousFile { expected: u64, actual: u64 },
    NonContiguousEarly { expected: u64, actual: u64 },
    ShadowCoverageGap(u64),
    ArithmeticOverflow,
    Formatting,
}

impl fmt::Display for EarlyMapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyFlatTable => write!(f, "flat dm table is empty"),
            Self::EmptyFileExtentMap => write!(f, "shadow FIEMAP extent map is empty"),
            Self::EmptyEarlyTable => write!(f, "early dm table is empty"),
            Self::ZeroLengthExtent => write!(f, "mapping contains a zero-length extent"),
            Self::MalformedFlatTableLine(line) => {
                write!(f, "malformed flat dm table row at line {line}")
            }
            Self::MalformedFileExtentLine(line) => {
                write!(f, "malformed shadow extent-map row at line {line}")
            }
            Self::InvalidNumber(line) => write!(f, "invalid numeric field at line {line}"),
            Self::UnknownFlatBacking { line, device } => write!(
                f,
                "unexpected flat-table backing device {device:?} at line {line}"
            ),
            Self::AmbiguousBackingDevice => {
                write!(f, "flat-table origin and shadow devices must be distinct")
            }
            Self::NonContiguousFlat { expected, actual } => write!(
                f,
                "flat table is not contiguous: expected logical sector {expected}, got {actual}"
            ),
            Self::NonContiguousFile { expected, actual } => write!(
                f,
                "shadow file extents are not contiguous: expected logical sector {expected}, got {actual}"
            ),
            Self::NonContiguousEarly { expected, actual } => write!(
                f,
                "early table is not contiguous: expected logical sector {expected}, got {actual}"
            ),
            Self::ShadowCoverageGap(sector) => {
                write!(f, "shadow FIEMAP does not cover file sector {sector}")
            }
            Self::ArithmeticOverflow => write!(f, "early-map arithmetic overflow"),
            Self::Formatting => write!(f, "failed to format early dm table"),
        }
    }
}

impl Error for EarlyMapError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowers_shadow_ranges_across_fragmented_metadata_extents() {
        let flat = vec![
            FlatExtent {
                logical_start: 0,
                sector_count: 8,
                source: FlatSource::Origin,
                source_start: 0,
            },
            FlatExtent {
                logical_start: 8,
                sector_count: 12,
                source: FlatSource::Shadow,
                source_start: 2,
            },
            FlatExtent {
                logical_start: 20,
                sector_count: 12,
                source: FlatSource::Origin,
                source_start: 20,
            },
        ];
        let file = vec![
            FileExtent {
                logical_start: 0,
                physical_start: 100,
                sector_count: 8,
            },
            FileExtent {
                logical_start: 8,
                physical_start: 300,
                sector_count: 8,
            },
        ];
        let early = lower_to_metadata(&flat, &file).unwrap();
        assert_eq!(early.len(), 4);
        assert_eq!(
            early[1],
            EarlyExtent {
                logical_start: 8,
                sector_count: 6,
                source: EarlySource::Metadata,
                source_start: 102,
            }
        );
        assert_eq!(
            early[2],
            EarlyExtent {
                logical_start: 14,
                sector_count: 6,
                source: EarlySource::Metadata,
                source_start: 300,
            }
        );
    }

    #[test]
    fn missing_shadow_coverage_is_rejected() {
        let flat = vec![FlatExtent {
            logical_start: 0,
            sector_count: 8,
            source: FlatSource::Shadow,
            source_start: 4,
        }];
        let file = vec![FileExtent {
            logical_start: 0,
            physical_start: 100,
            sector_count: 4,
        }];
        assert_eq!(
            lower_to_metadata(&flat, &file),
            Err(EarlyMapError::ShadowCoverageGap(4))
        );
    }

    #[test]
    fn formatted_table_uses_only_early_tokens() {
        let early = vec![
            EarlyExtent {
                logical_start: 0,
                sector_count: 4,
                source: EarlySource::Origin,
                source_start: 0,
            },
            EarlyExtent {
                logical_start: 4,
                sector_count: 4,
                source: EarlySource::Metadata,
                source_start: 80,
            },
        ];
        let table = format_early_table(&early).unwrap();
        assert!(table.contains(ORIGIN_TOKEN));
        assert!(table.contains(METADATA_TOKEN));
        assert!(!table.contains("/dev/"));
    }
}
