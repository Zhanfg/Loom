#![forbid(unsafe_code)]

use core::fmt;
use loom_types::{Extent, ExtentError, Sector, SectorCount, Source};
use std::fmt::Write as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplacementExtent {
    pub logical_start: Sector,
    pub sector_count: SectorCount,
    pub shadow_start: Sector,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoomMap {
    total_sectors: SectorCount,
    extents: Vec<Extent>,
}

impl LoomMap {
    /// Builds a complete virtual-device map containing one shadow replacement.
    ///
    /// # Errors
    /// Returns [`MapError`] when the replacement is empty, out of bounds, or overflows.
    pub fn single_replacement(
        total_sectors: SectorCount,
        logical_start: Sector,
        sector_count: SectorCount,
        shadow_start: Sector,
    ) -> Result<Self, MapError> {
        Self::from_replacements(
            total_sectors,
            &[ReplacementExtent {
                logical_start,
                sector_count,
                shadow_start,
            }],
        )
    }

    /// Builds a complete virtual-device map from sparse shadow replacements.
    ///
    /// # Errors
    /// Returns [`MapError`] for invalid, overlapping, out-of-bounds, or overflowing extents.
    pub fn from_replacements(
        total_sectors: SectorCount,
        replacements: &[ReplacementExtent],
    ) -> Result<Self, MapError> {
        if total_sectors.0 == 0 {
            return Err(MapError::EmptyDevice);
        }

        let mut replacements = replacements.to_vec();
        replacements.sort_by_key(|replacement| replacement.logical_start.0);

        let mut extents =
            Vec::with_capacity(replacements.len().saturating_mul(2).saturating_add(1));
        let mut cursor = 0_u64;

        for replacement in replacements {
            if replacement.sector_count.0 == 0 {
                return Err(MapError::Extent(ExtentError::ZeroLength));
            }

            let logical_end = replacement
                .logical_start
                .0
                .checked_add(replacement.sector_count.0)
                .ok_or(MapError::Extent(ExtentError::Overflow))?;
            if logical_end > total_sectors.0 {
                return Err(MapError::ReplacementOutOfBounds {
                    total: total_sectors.0,
                    start: replacement.logical_start.0,
                    length: replacement.sector_count.0,
                });
            }
            if replacement.logical_start.0 < cursor {
                return Err(MapError::OverlappingReplacements {
                    previous_end: cursor,
                    next_start: replacement.logical_start.0,
                });
            }

            if replacement.logical_start.0 > cursor {
                extents.push(Extent {
                    logical_start: Sector(cursor),
                    sector_count: SectorCount(replacement.logical_start.0 - cursor),
                    source: Source::Origin,
                    source_start: Sector(cursor),
                });
            }

            extents.push(Extent {
                logical_start: replacement.logical_start,
                sector_count: replacement.sector_count,
                source: Source::Shadow,
                source_start: replacement.shadow_start,
            });
            cursor = logical_end;
        }

        if cursor < total_sectors.0 {
            extents.push(Extent {
                logical_start: Sector(cursor),
                sector_count: SectorCount(total_sectors.0 - cursor),
                source: Source::Origin,
                source_start: Sector(cursor),
            });
        }

        if extents.is_empty() {
            extents.push(Extent {
                logical_start: Sector(0),
                sector_count: total_sectors,
                source: Source::Origin,
                source_start: Sector(0),
            });
        }

        let map = Self {
            total_sectors,
            extents: merge_adjacent(extents)?,
        };
        map.validate()?;
        Ok(map)
    }

    pub fn total_sectors(&self) -> SectorCount {
        self.total_sectors
    }

    pub fn extents(&self) -> &[Extent] {
        &self.extents
    }

    /// Validates that the map covers the virtual device exactly once and without gaps.
    ///
    /// # Errors
    /// Returns [`MapError`] if any extent is invalid or the map is not contiguous.
    pub fn validate(&self) -> Result<(), MapError> {
        if self.total_sectors.0 == 0 {
            return Err(MapError::EmptyDevice);
        }
        if self.extents.is_empty() {
            return Err(MapError::EmptyMap);
        }

        let mut expected_start = 0_u64;
        for extent in &self.extents {
            extent.validate().map_err(MapError::Extent)?;
            if extent.logical_start.0 != expected_start {
                return Err(MapError::NonContiguous {
                    expected: expected_start,
                    actual: extent.logical_start.0,
                });
            }
            expected_start = extent.logical_end().map_err(MapError::Extent)?.0;
        }

        if expected_start != self.total_sectors.0 {
            return Err(MapError::WrongFinalSize {
                expected: self.total_sectors.0,
                actual: expected_start,
            });
        }
        Ok(())
    }

    /// Lowers this map to a Linux device-mapper linear table.
    ///
    /// # Errors
    /// Returns [`MapError`] if the map is invalid or a backing-device name is unsafe.
    pub fn to_dm_linear_table(
        &self,
        origin_device: &str,
        shadow_device: &str,
    ) -> Result<String, MapError> {
        self.validate()?;
        validate_device_name(origin_device)?;
        validate_device_name(shadow_device)?;

        let mut table = String::new();
        for extent in &self.extents {
            let device = match extent.source {
                Source::Origin => origin_device,
                Source::Shadow => shadow_device,
            };
            writeln!(
                table,
                "{} {} linear {} {}",
                extent.logical_start.0, extent.sector_count.0, device, extent.source_start.0
            )
            .expect("writing to String cannot fail");
        }
        Ok(table)
    }
}

fn merge_adjacent(extents: Vec<Extent>) -> Result<Vec<Extent>, MapError> {
    let mut merged: Vec<Extent> = Vec::with_capacity(extents.len());

    for extent in extents {
        extent.validate().map_err(MapError::Extent)?;
        if let Some(previous) = merged.last_mut() {
            let previous_logical_end = previous.logical_end().map_err(MapError::Extent)?.0;
            let previous_source_end = previous
                .source_start
                .0
                .checked_add(previous.sector_count.0)
                .ok_or(MapError::Extent(ExtentError::Overflow))?;

            if previous.source == extent.source
                && previous_logical_end == extent.logical_start.0
                && previous_source_end == extent.source_start.0
            {
                previous.sector_count = SectorCount(
                    previous
                        .sector_count
                        .0
                        .checked_add(extent.sector_count.0)
                        .ok_or(MapError::Extent(ExtentError::Overflow))?,
                );
                continue;
            }
        }
        merged.push(extent);
    }

    Ok(merged)
}

fn validate_device_name(device: &str) -> Result<(), MapError> {
    if device.is_empty() || device.chars().any(char::is_whitespace) {
        return Err(MapError::InvalidDeviceName(device.to_owned()));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapError {
    EmptyDevice,
    EmptyMap,
    Extent(ExtentError),
    ReplacementOutOfBounds { total: u64, start: u64, length: u64 },
    OverlappingReplacements { previous_end: u64, next_start: u64 },
    NonContiguous { expected: u64, actual: u64 },
    WrongFinalSize { expected: u64, actual: u64 },
    InvalidDeviceName(String),
}

impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyDevice => write!(f, "virtual device must contain at least one sector"),
            Self::EmptyMap => write!(f, "map must contain at least one extent"),
            Self::Extent(error) => write!(f, "invalid extent: {error}"),
            Self::ReplacementOutOfBounds {
                total,
                start,
                length,
            } => write!(
                f,
                "replacement [{start}, {}) exceeds device size {total}",
                start.saturating_add(*length)
            ),
            Self::OverlappingReplacements {
                previous_end,
                next_start,
            } => write!(
                f,
                "replacement at sector {next_start} overlaps previous replacement ending at {previous_end}"
            ),
            Self::NonContiguous { expected, actual } => write!(
                f,
                "map is not contiguous: expected logical sector {expected}, got {actual}"
            ),
            Self::WrongFinalSize { expected, actual } => write!(
                f,
                "map covers {actual} sectors but virtual device requires {expected}"
            ),
            Self::InvalidDeviceName(device) => {
                write!(f, "invalid device-mapper backing device name: {device:?}")
            }
        }
    }
}

impl std::error::Error for MapError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn middle_replacement_is_woven_between_origin_extents() {
        let map =
            LoomMap::single_replacement(SectorCount(100), Sector(40), SectorCount(8), Sector(0))
                .unwrap();

        assert_eq!(map.extents().len(), 3);
        assert_eq!(map.extents()[0].source, Source::Origin);
        assert_eq!(map.extents()[0].sector_count, SectorCount(40));
        assert_eq!(map.extents()[1].source, Source::Shadow);
        assert_eq!(map.extents()[1].logical_start, Sector(40));
        assert_eq!(map.extents()[2].source_start, Sector(48));
    }

    #[test]
    fn multiple_replacements_are_sorted_and_woven() {
        let map = LoomMap::from_replacements(
            SectorCount(64),
            &[
                ReplacementExtent {
                    logical_start: Sector(32),
                    sector_count: SectorCount(8),
                    shadow_start: Sector(8),
                },
                ReplacementExtent {
                    logical_start: Sector(8),
                    sector_count: SectorCount(8),
                    shadow_start: Sector(0),
                },
            ],
        )
        .unwrap();

        assert_eq!(map.extents().len(), 5);
        assert_eq!(map.extents()[1].source, Source::Shadow);
        assert_eq!(map.extents()[1].logical_start, Sector(8));
        assert_eq!(map.extents()[3].source, Source::Shadow);
        assert_eq!(map.extents()[3].logical_start, Sector(32));
    }

    #[test]
    fn adjacent_shadow_replacements_are_merged() {
        let map = LoomMap::from_replacements(
            SectorCount(32),
            &[
                ReplacementExtent {
                    logical_start: Sector(8),
                    sector_count: SectorCount(8),
                    shadow_start: Sector(0),
                },
                ReplacementExtent {
                    logical_start: Sector(16),
                    sector_count: SectorCount(8),
                    shadow_start: Sector(8),
                },
            ],
        )
        .unwrap();

        assert_eq!(map.extents().len(), 3);
        assert_eq!(map.extents()[1].sector_count, SectorCount(16));
    }

    #[test]
    fn overlapping_replacements_are_rejected() {
        let error = LoomMap::from_replacements(
            SectorCount(32),
            &[
                ReplacementExtent {
                    logical_start: Sector(8),
                    sector_count: SectorCount(12),
                    shadow_start: Sector(0),
                },
                ReplacementExtent {
                    logical_start: Sector(16),
                    sector_count: SectorCount(8),
                    shadow_start: Sector(12),
                },
            ],
        )
        .unwrap_err();

        assert!(matches!(error, MapError::OverlappingReplacements { .. }));
    }

    #[test]
    fn dm_table_uses_origin_and_shadow_devices() {
        let map =
            LoomMap::single_replacement(SectorCount(32), Sector(8), SectorCount(8), Sector(0))
                .unwrap();

        let table = map.to_dm_linear_table("/dev/loop0", "/dev/loop1").unwrap();
        assert_eq!(
            table,
            "0 8 linear /dev/loop0 0\n8 8 linear /dev/loop1 0\n16 16 linear /dev/loop0 16\n"
        );
    }

    #[test]
    fn out_of_bounds_replacement_is_rejected() {
        let error =
            LoomMap::single_replacement(SectorCount(16), Sector(12), SectorCount(8), Sector(0))
                .unwrap_err();

        assert!(matches!(error, MapError::ReplacementOutOfBounds { .. }));
    }
}
