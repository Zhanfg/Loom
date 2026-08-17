#![forbid(unsafe_code)]

use std::env;
use std::error::Error;
use std::fmt;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

const SECTOR_SIZE: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Origin,
    Shadow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Extent {
    logical_start: u64,
    sector_count: u64,
    source: Source,
    source_start: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FlatMap {
    total_sectors: u64,
    extents: Vec<Extent>,
}

#[derive(Debug)]
struct LayerSpec {
    table_path: PathBuf,
    origin_device: String,
    shadow_device: String,
    shadow_path: PathBuf,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("loom-flatten: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = env::args();
    let _program = args.next();
    let manifest = required(&mut args, "layer manifest")?;
    let shadow_output = required(&mut args, "aggregate shadow output")?;
    let origin_device = required(&mut args, "authoritative origin device")?;
    let aggregate_shadow_device = required(&mut args, "aggregate shadow device")?;
    let table_output = required(&mut args, "flattened dm table output")?;
    if let Some(extra) = args.next() {
        return Err(format!("unexpected extra argument: {extra}").into());
    }
    if origin_device.is_empty()
        || aggregate_shadow_device.is_empty()
        || origin_device == aggregate_shadow_device
    {
        return Err("origin and aggregate shadow devices must be distinct and non-empty".into());
    }

    let specs = read_manifest(Path::new(&manifest))?;
    if specs.is_empty() {
        return Err("layer manifest contains no materialized layers".into());
    }

    let mut flat: Option<FlatMap> = None;
    let mut aggregate = Vec::new();
    for (index, spec) in specs.iter().enumerate() {
        let table = fs::read_to_string(&spec.table_path)?;
        let layer = FlatMap::parse_dm_table(&table, &spec.origin_device, &spec.shadow_device)
            .map_err(|error| format!("layer {} table invalid: {error}", index + 1))?;
        let shadow = fs::read(&spec.shadow_path)?;
        if shadow.is_empty() || shadow.len() % SECTOR_SIZE != 0 {
            return Err(format!(
                "layer {} shadow pack size {} is not a non-zero multiple of {}",
                index + 1,
                shadow.len(),
                SECTOR_SIZE
            )
            .into());
        }

        let shadow_offset = u64::try_from(aggregate.len() / SECTOR_SIZE)
            .map_err(|_| FlattenError::ArithmeticOverflow)?;
        let base = match flat.take() {
            Some(map) => map,
            None => FlatMap::identity(layer.total_sectors)?,
        };
        flat = Some(base.compose(&layer, shadow_offset)?);
        aggregate.extend_from_slice(&shadow);
    }

    let flat = flat.ok_or("no flattened map was produced")?;
    let (flat, compact_shadow) = flat.compact_shadow(&aggregate)?;
    let table = flat.to_dm_table(&origin_device, &aggregate_shadow_device)?;
    write_outputs_atomically(
        Path::new(&shadow_output),
        &compact_shadow,
        Path::new(&table_output),
        table.as_bytes(),
    )?;

    println!(
        "Loom generation flattened: layers={} sectors={} extents={} shadow_bytes={} shadow_sectors={}",
        specs.len(),
        flat.total_sectors,
        flat.extents.len(),
        compact_shadow.len(),
        compact_shadow.len() / SECTOR_SIZE
    );
    Ok(())
}

impl FlatMap {
    fn identity(total_sectors: u64) -> Result<Self, FlattenError> {
        if total_sectors == 0 {
            return Err(FlattenError::EmptyDevice);
        }
        Ok(Self {
            total_sectors,
            extents: vec![Extent {
                logical_start: 0,
                sector_count: total_sectors,
                source: Source::Origin,
                source_start: 0,
            }],
        })
    }

    fn parse_dm_table(
        table: &str,
        origin_device: &str,
        shadow_device: &str,
    ) -> Result<Self, FlattenError> {
        if origin_device.is_empty() || shadow_device.is_empty() || origin_device == shadow_device {
            return Err(FlattenError::AmbiguousBackingDevice);
        }
        let mut extents = Vec::new();
        for (line_index, raw) in table.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() != 5 || fields[2] != "linear" {
                return Err(FlattenError::MalformedTableLine(line_index + 1));
            }
            let logical_start = parse_u64(fields[0], line_index + 1)?;
            let sector_count = parse_u64(fields[1], line_index + 1)?;
            let source_start = parse_u64(fields[4], line_index + 1)?;
            let source = if fields[3] == origin_device {
                Source::Origin
            } else if fields[3] == shadow_device {
                Source::Shadow
            } else {
                return Err(FlattenError::UnknownBackingDevice {
                    line: line_index + 1,
                    device: fields[3].to_owned(),
                });
            };
            extents.push(Extent {
                logical_start,
                sector_count,
                source,
                source_start,
            });
        }
        let last = extents.last().ok_or(FlattenError::EmptyTable)?;
        let total_sectors = checked_add(last.logical_start, last.sector_count)?;
        let map = Self {
            total_sectors,
            extents: merge_adjacent(extents)?,
        };
        map.validate()?;
        Ok(map)
    }

    fn compose(&self, layer: &Self, new_shadow_offset: u64) -> Result<Self, FlattenError> {
        self.validate()?;
        layer.validate()?;
        if self.total_sectors != layer.total_sectors {
            return Err(FlattenError::MismatchedDeviceSize {
                base: self.total_sectors,
                layer: layer.total_sectors,
            });
        }

        let mut output = Vec::new();
        for layer_extent in &layer.extents {
            if layer_extent.source == Source::Shadow {
                output.push(Extent {
                    logical_start: layer_extent.logical_start,
                    sector_count: layer_extent.sector_count,
                    source: Source::Shadow,
                    source_start: checked_add(new_shadow_offset, layer_extent.source_start)?,
                });
                continue;
            }

            let source_end = checked_add(layer_extent.source_start, layer_extent.sector_count)?;
            let mut cursor = layer_extent.source_start;
            while cursor < source_end {
                let base_extent = self
                    .extents
                    .iter()
                    .find(|extent| {
                        extent.logical_start <= cursor
                            && checked_add(extent.logical_start, extent.sector_count)
                                .is_ok_and(|end| cursor < end)
                    })
                    .ok_or(FlattenError::BaseCoverageGap(cursor))?;
                let base_end = checked_add(base_extent.logical_start, base_extent.sector_count)?;
                let chunk_end = base_end.min(source_end);
                let chunk_count = chunk_end
                    .checked_sub(cursor)
                    .ok_or(FlattenError::ArithmeticOverflow)?;
                if chunk_count == 0 {
                    return Err(FlattenError::BaseCoverageGap(cursor));
                }
                let layer_delta = cursor
                    .checked_sub(layer_extent.source_start)
                    .ok_or(FlattenError::ArithmeticOverflow)?;
                let base_delta = cursor
                    .checked_sub(base_extent.logical_start)
                    .ok_or(FlattenError::ArithmeticOverflow)?;
                output.push(Extent {
                    logical_start: checked_add(layer_extent.logical_start, layer_delta)?,
                    sector_count: chunk_count,
                    source: base_extent.source,
                    source_start: checked_add(base_extent.source_start, base_delta)?,
                });
                cursor = chunk_end;
            }
        }

        let map = Self {
            total_sectors: self.total_sectors,
            extents: merge_adjacent(output)?,
        };
        map.validate()?;
        Ok(map)
    }

    fn compact_shadow(&self, aggregate: &[u8]) -> Result<(Self, Vec<u8>), FlattenError> {
        self.validate()?;
        let mut compact = Vec::new();
        let mut extents = Vec::with_capacity(self.extents.len());
        for extent in &self.extents {
            if extent.source == Source::Origin {
                extents.push(*extent);
                continue;
            }
            let byte_start_u64 = extent
                .source_start
                .checked_mul(SECTOR_SIZE as u64)
                .ok_or(FlattenError::ArithmeticOverflow)?;
            let byte_len_u64 = extent
                .sector_count
                .checked_mul(SECTOR_SIZE as u64)
                .ok_or(FlattenError::ArithmeticOverflow)?;
            let byte_end_u64 = checked_add(byte_start_u64, byte_len_u64)?;
            let byte_start =
                usize::try_from(byte_start_u64).map_err(|_| FlattenError::ArithmeticOverflow)?;
            let byte_end =
                usize::try_from(byte_end_u64).map_err(|_| FlattenError::ArithmeticOverflow)?;
            let bytes = aggregate
                .get(byte_start..byte_end)
                .ok_or(FlattenError::ShadowOutOfBounds)?;
            let compact_start = u64::try_from(compact.len() / SECTOR_SIZE)
                .map_err(|_| FlattenError::ArithmeticOverflow)?;
            compact.extend_from_slice(bytes);
            extents.push(Extent {
                logical_start: extent.logical_start,
                sector_count: extent.sector_count,
                source: Source::Shadow,
                source_start: compact_start,
            });
        }
        let map = Self {
            total_sectors: self.total_sectors,
            extents: merge_adjacent(extents)?,
        };
        map.validate()?;
        Ok((map, compact))
    }

    fn to_dm_table(
        &self,
        origin_device: &str,
        shadow_device: &str,
    ) -> Result<String, FlattenError> {
        self.validate()?;
        validate_device_name(origin_device)?;
        validate_device_name(shadow_device)?;
        let mut output = String::new();
        for extent in &self.extents {
            let device = match extent.source {
                Source::Origin => origin_device,
                Source::Shadow => shadow_device,
            };
            writeln!(
                &mut output,
                "{} {} linear {} {}",
                extent.logical_start, extent.sector_count, device, extent.source_start
            )
            .map_err(|_| FlattenError::Formatting)?;
        }
        Ok(output)
    }

    fn validate(&self) -> Result<(), FlattenError> {
        if self.total_sectors == 0 {
            return Err(FlattenError::EmptyDevice);
        }
        if self.extents.is_empty() {
            return Err(FlattenError::EmptyTable);
        }
        let mut expected = 0_u64;
        for extent in &self.extents {
            if extent.sector_count == 0 {
                return Err(FlattenError::ZeroLengthExtent);
            }
            if extent.logical_start != expected {
                return Err(FlattenError::NonContiguous {
                    expected,
                    actual: extent.logical_start,
                });
            }
            checked_add(extent.source_start, extent.sector_count)?;
            expected = checked_add(extent.logical_start, extent.sector_count)?;
        }
        if expected != self.total_sectors {
            return Err(FlattenError::WrongFinalSize {
                expected: self.total_sectors,
                actual: expected,
            });
        }
        Ok(())
    }
}

fn read_manifest(path: &Path) -> Result<Vec<LayerSpec>, Box<dyn Error>> {
    let text = fs::read_to_string(path)?;
    let mut specs = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim_end();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 4 || fields.iter().any(|field| field.is_empty()) {
            return Err(format!(
                "malformed layer manifest line {}: expected table<TAB>origin<TAB>shadow<TAB>pack",
                index + 1
            )
            .into());
        }
        specs.push(LayerSpec {
            table_path: PathBuf::from(fields[0]),
            origin_device: fields[1].to_owned(),
            shadow_device: fields[2].to_owned(),
            shadow_path: PathBuf::from(fields[3]),
        });
    }
    Ok(specs)
}

fn write_outputs_atomically(
    shadow_path: &Path,
    shadow: &[u8],
    table_path: &Path,
    table: &[u8],
) -> Result<(), Box<dyn Error>> {
    if shadow_path == table_path {
        return Err("shadow and table outputs must be different paths".into());
    }
    if shadow_path.exists() || table_path.exists() {
        return Err(
            "flatten outputs already exist; refusing to overwrite generation artifacts".into(),
        );
    }
    ensure_parent_exists(shadow_path)?;
    ensure_parent_exists(table_path)?;

    let pid = std::process::id();
    let shadow_tmp = temp_path(shadow_path, pid, "shadow");
    let table_tmp = temp_path(table_path, pid, "table");
    let _ = fs::remove_file(&shadow_tmp);
    let _ = fs::remove_file(&table_tmp);

    let result = (|| -> Result<(), Box<dyn Error>> {
        fs::write(&shadow_tmp, shadow)?;
        fs::write(&table_tmp, table)?;
        fs::rename(&shadow_tmp, shadow_path)?;
        if let Err(error) = fs::rename(&table_tmp, table_path) {
            let _ = fs::remove_file(shadow_path);
            return Err(error.into());
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&shadow_tmp);
        let _ = fs::remove_file(&table_tmp);
    }
    result
}

fn ensure_parent_exists(path: &Path) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.is_dir() {
            return Err(format!("output parent does not exist: {}", parent.display()).into());
        }
    }
    Ok(())
}

fn temp_path(path: &Path, pid: u32, kind: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(format!(".loomtmp-{pid}-{kind}"));
    PathBuf::from(value)
}

fn merge_adjacent(extents: Vec<Extent>) -> Result<Vec<Extent>, FlattenError> {
    let mut merged: Vec<Extent> = Vec::with_capacity(extents.len());
    for extent in extents {
        if extent.sector_count == 0 {
            return Err(FlattenError::ZeroLengthExtent);
        }
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

fn validate_device_name(device: &str) -> Result<(), FlattenError> {
    if device.is_empty() || device.chars().any(char::is_whitespace) {
        return Err(FlattenError::InvalidDeviceName(device.to_owned()));
    }
    Ok(())
}

fn checked_add(left: u64, right: u64) -> Result<u64, FlattenError> {
    left.checked_add(right)
        .ok_or(FlattenError::ArithmeticOverflow)
}

fn parse_u64(value: &str, line: usize) -> Result<u64, FlattenError> {
    value
        .parse::<u64>()
        .map_err(|_| FlattenError::InvalidNumber(line))
}

fn required(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String, Box<dyn Error>> {
    args.next()
        .ok_or_else(|| format!("missing required argument: {name}").into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FlattenError {
    EmptyDevice,
    EmptyTable,
    ZeroLengthExtent,
    MalformedTableLine(usize),
    InvalidNumber(usize),
    UnknownBackingDevice { line: usize, device: String },
    AmbiguousBackingDevice,
    MismatchedDeviceSize { base: u64, layer: u64 },
    BaseCoverageGap(u64),
    ShadowOutOfBounds,
    NonContiguous { expected: u64, actual: u64 },
    WrongFinalSize { expected: u64, actual: u64 },
    InvalidDeviceName(String),
    ArithmeticOverflow,
    Formatting,
}

impl fmt::Display for FlattenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyDevice => write!(f, "virtual device contains zero sectors"),
            Self::EmptyTable => write!(f, "dm-linear table contains no extents"),
            Self::ZeroLengthExtent => write!(f, "dm-linear extent has zero length"),
            Self::MalformedTableLine(line) => write!(f, "malformed dm-linear row at line {line}"),
            Self::InvalidNumber(line) => {
                write!(f, "invalid numeric field at dm-linear line {line}")
            }
            Self::UnknownBackingDevice { line, device } => write!(
                f,
                "unknown backing device {device:?} at dm-linear line {line}"
            ),
            Self::AmbiguousBackingDevice => write!(
                f,
                "origin and shadow backing device names must be distinct and non-empty"
            ),
            Self::MismatchedDeviceSize { base, layer } => write!(
                f,
                "layer geometry differs from base: base={base} sectors layer={layer} sectors"
            ),
            Self::BaseCoverageGap(sector) => {
                write!(f, "base mapping does not cover source sector {sector}")
            }
            Self::ShadowOutOfBounds => {
                write!(f, "flattened map references bytes outside aggregate shadow")
            }
            Self::NonContiguous { expected, actual } => write!(
                f,
                "dm-linear map is not contiguous: expected sector {expected}, got {actual}"
            ),
            Self::WrongFinalSize { expected, actual } => write!(
                f,
                "dm-linear map covers {actual} sectors but expected {expected}"
            ),
            Self::InvalidDeviceName(device) => {
                write!(f, "invalid device-mapper backing device name {device:?}")
            }
            Self::ArithmeticOverflow => write!(f, "flattening arithmetic overflow"),
            Self::Formatting => write!(f, "failed to format flattened dm-linear table"),
        }
    }
}

impl Error for FlattenError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn replacement(total: u64, start: u64, count: u64, shadow_start: u64) -> FlatMap {
        let mut extents = Vec::new();
        if start > 0 {
            extents.push(Extent {
                logical_start: 0,
                sector_count: start,
                source: Source::Origin,
                source_start: 0,
            });
        }
        extents.push(Extent {
            logical_start: start,
            sector_count: count,
            source: Source::Shadow,
            source_start: shadow_start,
        });
        let end = start + count;
        if end < total {
            extents.push(Extent {
                logical_start: end,
                sector_count: total - end,
                source: Source::Origin,
                source_start: end,
            });
        }
        FlatMap {
            total_sectors: total,
            extents,
        }
    }

    #[test]
    fn parse_round_trip() {
        let map = replacement(32, 8, 8, 0);
        let table = map.to_dm_table("/dev/origin", "/dev/shadow").unwrap();
        let parsed = FlatMap::parse_dm_table(&table, "/dev/origin", "/dev/shadow").unwrap();
        assert_eq!(parsed, map);
    }

    #[test]
    fn later_layer_overrides_overlap_and_preserves_previous_shadow() {
        let first = replacement(100, 20, 10, 0);
        let second = replacement(100, 25, 10, 0);
        let composed = first.compose(&second, 10).unwrap();
        assert_eq!(composed.extents.len(), 4);
        assert_eq!(
            composed.extents[1],
            Extent {
                logical_start: 20,
                sector_count: 5,
                source: Source::Shadow,
                source_start: 0,
            }
        );
        assert_eq!(
            composed.extents[2],
            Extent {
                logical_start: 25,
                sector_count: 10,
                source: Source::Shadow,
                source_start: 10,
            }
        );
    }

    #[test]
    fn compaction_drops_shadow_bytes_hidden_by_later_layers() {
        let first = replacement(32, 8, 8, 0);
        let second = replacement(32, 8, 8, 0);
        let composed = first.compose(&second, 8).unwrap();
        let mut aggregate = vec![0x11; 8 * SECTOR_SIZE];
        aggregate.extend(vec![0x22; 8 * SECTOR_SIZE]);
        let (map, compact) = composed.compact_shadow(&aggregate).unwrap();
        assert_eq!(compact.len(), 8 * SECTOR_SIZE);
        assert!(compact.iter().all(|byte| *byte == 0x22));
        assert_eq!(map.extents[1].source_start, 0);
    }

    #[test]
    fn rejects_unknown_table_device() {
        let error =
            FlatMap::parse_dm_table("0 32 linear /dev/wrong 0\n", "/dev/origin", "/dev/shadow")
                .unwrap_err();
        assert!(matches!(
            error,
            FlattenError::UnknownBackingDevice { line: 1, .. }
        ));
    }
}
