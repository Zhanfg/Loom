use super::{merge_adjacent, LoomMap, MapError};
use core::fmt;
use loom_types::{Extent, Sector, SectorCount, Source};

/// Parses a complete `dm-linear` table produced by Loom back into a typed map.
///
/// The parser is deliberately strict: only five-field `linear` rows are accepted and every
/// backing device must match exactly one of the expected origin/shadow device names.
///
/// # Errors
/// Returns [`ComposeError`] for malformed rows, unknown backing devices, invalid map geometry,
/// or ambiguous origin/shadow device names.
pub fn parse_dm_linear_table(
    table: &str,
    origin_device: &str,
    shadow_device: &str,
) -> Result<LoomMap, ComposeError> {
    if origin_device.is_empty() || shadow_device.is_empty() || origin_device == shadow_device {
        return Err(ComposeError::AmbiguousBackingDevice);
    }

    let mut extents = Vec::new();
    for (index, raw_line) in table.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != 5 || fields[2] != "linear" {
            return Err(ComposeError::MalformedTableLine(index + 1));
        }

        let logical_start = parse_u64(fields[0], index + 1)?;
        let sector_count = parse_u64(fields[1], index + 1)?;
        let source_start = parse_u64(fields[4], index + 1)?;
        let source = if fields[3] == origin_device {
            Source::Origin
        } else if fields[3] == shadow_device {
            Source::Shadow
        } else {
            return Err(ComposeError::UnknownBackingDevice {
                line: index + 1,
                device: fields[3].to_owned(),
            });
        };

        extents.push(Extent {
            logical_start: Sector(logical_start),
            sector_count: SectorCount(sector_count),
            source,
            source_start: Sector(source_start),
        });
    }

    let final_extent = extents.last().ok_or(ComposeError::EmptyTable)?;
    let total_sectors = final_extent
        .logical_start
        .0
        .checked_add(final_extent.sector_count.0)
        .ok_or(ComposeError::ArithmeticOverflow)?;
    let extents = merge_adjacent(extents).map_err(ComposeError::Map)?;
    let map = LoomMap {
        total_sectors: SectorCount(total_sectors),
        extents,
    };
    map.validate().map_err(ComposeError::Map)?;
    Ok(map)
}

/// Composes one complete Loom layer over an already-flattened base map.
///
/// `base` maps the virtual device to the authoritative origin plus an aggregate shadow pack.
/// `layer` maps the same virtual device to `base` (as its origin) plus one new shadow pack.
/// `layer_shadow_offset` is the sector offset at which that new shadow pack will be appended to
/// the aggregate shadow file. The returned map therefore references only two logical sources:
/// the original authoritative device and the aggregate shadow.
///
/// # Errors
/// Returns [`ComposeError`] when either map is invalid, device geometry differs, arithmetic
/// overflows, or an origin range in `layer` cannot be resolved through `base`.
pub fn compose_layer(
    base: &LoomMap,
    layer: &LoomMap,
    layer_shadow_offset: Sector,
) -> Result<LoomMap, ComposeError> {
    base.validate().map_err(ComposeError::Map)?;
    layer.validate().map_err(ComposeError::Map)?;
    if base.total_sectors() != layer.total_sectors() {
        return Err(ComposeError::MismatchedDeviceSize {
            base: base.total_sectors().0,
            layer: layer.total_sectors().0,
        });
    }

    let mut composed = Vec::new();
    for layer_extent in layer.extents() {
        match layer_extent.source {
            Source::Shadow => {
                let source_start = layer_shadow_offset
                    .0
                    .checked_add(layer_extent.source_start.0)
                    .ok_or(ComposeError::ArithmeticOverflow)?;
                composed.push(Extent {
                    logical_start: layer_extent.logical_start,
                    sector_count: layer_extent.sector_count,
                    source: Source::Shadow,
                    source_start: Sector(source_start),
                });
            }
            Source::Origin => compose_origin_extent(base, *layer_extent, &mut composed)?,
        }
    }

    let extents = merge_adjacent(composed).map_err(ComposeError::Map)?;
    let map = LoomMap {
        total_sectors: base.total_sectors(),
        extents,
    };
    map.validate().map_err(ComposeError::Map)?;
    Ok(map)
}

fn compose_origin_extent(
    base: &LoomMap,
    layer_extent: Extent,
    output: &mut Vec<Extent>,
) -> Result<(), ComposeError> {
    let mut cursor = layer_extent.source_start.0;
    let source_end = layer_extent
        .source_start
        .0
        .checked_add(layer_extent.sector_count.0)
        .ok_or(ComposeError::ArithmeticOverflow)?;

    while cursor < source_end {
        let base_extent = base
            .extents()
            .iter()
            .find(|extent| {
                let end = extent
                    .logical_start
                    .0
                    .checked_add(extent.sector_count.0)
                    .unwrap_or(u64::MAX);
                extent.logical_start.0 <= cursor && cursor < end
            })
            .ok_or(ComposeError::BaseCoverageGap(cursor))?;
        let base_end = base_extent
            .logical_start
            .0
            .checked_add(base_extent.sector_count.0)
            .ok_or(ComposeError::ArithmeticOverflow)?;
        let chunk_end = base_end.min(source_end);
        let chunk_count = chunk_end
            .checked_sub(cursor)
            .ok_or(ComposeError::ArithmeticOverflow)?;
        if chunk_count == 0 {
            return Err(ComposeError::BaseCoverageGap(cursor));
        }

        let layer_delta = cursor
            .checked_sub(layer_extent.source_start.0)
            .ok_or(ComposeError::ArithmeticOverflow)?;
        let logical_start = layer_extent
            .logical_start
            .0
            .checked_add(layer_delta)
            .ok_or(ComposeError::ArithmeticOverflow)?;
        let base_delta = cursor
            .checked_sub(base_extent.logical_start.0)
            .ok_or(ComposeError::ArithmeticOverflow)?;
        let source_start = base_extent
            .source_start
            .0
            .checked_add(base_delta)
            .ok_or(ComposeError::ArithmeticOverflow)?;

        output.push(Extent {
            logical_start: Sector(logical_start),
            sector_count: SectorCount(chunk_count),
            source: base_extent.source,
            source_start: Sector(source_start),
        });
        cursor = chunk_end;
    }
    Ok(())
}

fn parse_u64(value: &str, line: usize) -> Result<u64, ComposeError> {
    value
        .parse::<u64>()
        .map_err(|_| ComposeError::InvalidNumber(line))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeError {
    Map(MapError),
    EmptyTable,
    MalformedTableLine(usize),
    InvalidNumber(usize),
    UnknownBackingDevice { line: usize, device: String },
    AmbiguousBackingDevice,
    MismatchedDeviceSize { base: u64, layer: u64 },
    BaseCoverageGap(u64),
    ArithmeticOverflow,
}

impl fmt::Display for ComposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Map(error) => write!(f, "invalid Loom map while composing: {error}"),
            Self::EmptyTable => write!(f, "dm-linear table contains no extents"),
            Self::MalformedTableLine(line) => {
                write!(f, "malformed dm-linear table row at line {line}")
            }
            Self::InvalidNumber(line) => {
                write!(f, "invalid numeric field in dm-linear table at line {line}")
            }
            Self::UnknownBackingDevice { line, device } => write!(
                f,
                "unknown backing device {device:?} in dm-linear table at line {line}"
            ),
            Self::AmbiguousBackingDevice => write!(
                f,
                "origin and shadow backing device names must be distinct and non-empty"
            ),
            Self::MismatchedDeviceSize { base, layer } => write!(
                f,
                "cannot compose Loom maps with different sizes: base={base} sectors layer={layer} sectors"
            ),
            Self::BaseCoverageGap(sector) => {
                write!(f, "base Loom map does not cover source sector {sector}")
            }
            Self::ArithmeticOverflow => write!(f, "Loom map composition arithmetic overflow"),
        }
    }
}

impl std::error::Error for ComposeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Map(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ReplacementExtent;

    #[test]
    fn parses_strict_complete_dm_table() {
        let table = concat!(
            "0 8 linear /dev/origin 0\n",
            "8 4 linear /dev/shadow 0\n",
            "12 20 linear /dev/origin 12\n",
        );
        let map = parse_dm_linear_table(table, "/dev/origin", "/dev/shadow").unwrap();
        assert_eq!(map.total_sectors(), SectorCount(32));
        assert_eq!(map.extents().len(), 3);
        assert_eq!(map.extents()[1].source, Source::Shadow);
        assert_eq!(map.extents()[1].source_start, Sector(0));
    }

    #[test]
    fn rejects_unknown_backing_device() {
        let error = parse_dm_linear_table(
            "0 8 linear /dev/unexpected 0\n",
            "/dev/origin",
            "/dev/shadow",
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ComposeError::UnknownBackingDevice { line: 1, .. }
        ));
    }

    #[test]
    fn later_shadow_overrides_previous_shadow_without_losing_neighbors() {
        let base = LoomMap::from_replacements(
            SectorCount(100),
            &[ReplacementExtent {
                logical_start: Sector(20),
                sector_count: SectorCount(10),
                shadow_start: Sector(0),
            }],
        )
        .unwrap();
        let layer = LoomMap::from_replacements(
            SectorCount(100),
            &[ReplacementExtent {
                logical_start: Sector(25),
                sector_count: SectorCount(10),
                shadow_start: Sector(0),
            }],
        )
        .unwrap();

        let composed = compose_layer(&base, &layer, Sector(10)).unwrap();
        assert_eq!(composed.extents().len(), 5);
        assert_eq!(composed.extents()[1].source, Source::Shadow);
        assert_eq!(composed.extents()[1].logical_start, Sector(20));
        assert_eq!(composed.extents()[1].sector_count, SectorCount(5));
        assert_eq!(composed.extents()[1].source_start, Sector(0));
        assert_eq!(composed.extents()[2].source, Source::Shadow);
        assert_eq!(composed.extents()[2].logical_start, Sector(25));
        assert_eq!(composed.extents()[2].sector_count, SectorCount(10));
        assert_eq!(composed.extents()[2].source_start, Sector(10));
    }

    #[test]
    fn origin_ranges_in_new_layer_retain_old_shadow_mapping() {
        let base = LoomMap::from_replacements(
            SectorCount(64),
            &[ReplacementExtent {
                logical_start: Sector(8),
                sector_count: SectorCount(8),
                shadow_start: Sector(0),
            }],
        )
        .unwrap();
        let identity = LoomMap::from_replacements(SectorCount(64), &[]).unwrap();

        let composed = compose_layer(&base, &identity, Sector(8)).unwrap();
        assert_eq!(composed, base);
    }

    #[test]
    fn mismatched_geometry_is_rejected() {
        let base = LoomMap::from_replacements(SectorCount(64), &[]).unwrap();
        let layer = LoomMap::from_replacements(SectorCount(32), &[]).unwrap();
        assert_eq!(
            compose_layer(&base, &layer, Sector(0)),
            Err(ComposeError::MismatchedDeviceSize {
                base: 64,
                layer: 32,
            })
        );
    }
}
