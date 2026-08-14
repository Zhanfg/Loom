#![forbid(unsafe_code)]

use core::fmt;
use loom_types::{Extent, ExtentError, Sector, SectorCount, Source};
use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoomMap {
    total_sectors: SectorCount,
    extents: Vec<Extent>,
}

impl LoomMap {
    pub fn single_replacement(
        total_sectors: SectorCount,
        logical_start: Sector,
        sector_count: SectorCount,
        shadow_start: Sector,
    ) -> Result<Self, MapError> {
        if total_sectors.0 == 0 {
            return Err(MapError::EmptyDevice);
        }
        if sector_count.0 == 0 {
            return Err(MapError::Extent(ExtentError::ZeroLength));
        }

        let logical_end = logical_start
            .0
            .checked_add(sector_count.0)
            .ok_or(MapError::Extent(ExtentError::Overflow))?;
        if logical_end > total_sectors.0 {
            return Err(MapError::ReplacementOutOfBounds {
                total: total_sectors.0,
                start: logical_start.0,
                length: sector_count.0,
            });
        }

        let mut extents = Vec::with_capacity(3);
        if logical_start.0 != 0 {
            extents.push(Extent {
                logical_start: Sector(0),
                sector_count: SectorCount(logical_start.0),
                source: Source::Origin,
                source_start: Sector(0),
            });
        }

        extents.push(Extent {
            logical_start,
            sector_count,
            source: Source::Shadow,
            source_start: shadow_start,
        });

        if logical_end < total_sectors.0 {
            extents.push(Extent {
                logical_start: Sector(logical_end),
                sector_count: SectorCount(total_sectors.0 - logical_end),
                source: Source::Origin,
                source_start: Sector(logical_end),
            });
        }

        let map = Self {
            total_sectors,
            extents,
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
            expected_start = extent
                .logical_end()
                .map_err(MapError::Extent)?
                .0;
        }

        if expected_start != self.total_sectors.0 {
            return Err(MapError::WrongFinalSize {
                expected: self.total_sectors.0,
                actual: expected_start,
            });
        }
        Ok(())
    }

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
                extent.logical_start.0,
                extent.sector_count.0,
                device,
                extent.source_start.0
            )
            .expect("writing to String cannot fail");
        }
        Ok(table)
    }
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
        let map = LoomMap::single_replacement(
            SectorCount(100),
            Sector(40),
            SectorCount(8),
            Sector(0),
        )
        .unwrap();

        assert_eq!(map.extents().len(), 3);
        assert_eq!(map.extents()[0].source, Source::Origin);
        assert_eq!(map.extents()[0].sector_count, SectorCount(40));
        assert_eq!(map.extents()[1].source, Source::Shadow);
        assert_eq!(map.extents()[1].logical_start, Sector(40));
        assert_eq!(map.extents()[2].source_start, Sector(48));
    }

    #[test]
    fn dm_table_uses_origin_and_shadow_devices() {
        let map = LoomMap::single_replacement(
            SectorCount(32),
            Sector(8),
            SectorCount(8),
            Sector(0),
        )
        .unwrap();

        let table = map.to_dm_linear_table("/dev/loop0", "/dev/loop1").unwrap();
        assert_eq!(
            table,
            "0 8 linear /dev/loop0 0\n8 8 linear /dev/loop1 0\n16 16 linear /dev/loop0 16\n"
        );
    }

    #[test]
    fn out_of_bounds_replacement_is_rejected() {
        let error = LoomMap::single_replacement(
            SectorCount(16),
            Sector(12),
            SectorCount(8),
            Sector(0),
        )
        .unwrap_err();

        assert!(matches!(error, MapError::ReplacementOutOfBounds { .. }));
    }
}
