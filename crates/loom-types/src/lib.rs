#![forbid(unsafe_code)]

use core::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Sector(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SectorCount(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Origin,
    Shadow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub logical_start: Sector,
    pub sector_count: SectorCount,
    pub source: Source,
    pub source_start: Sector,
}

impl Extent {
    pub fn logical_end(self) -> Result<Sector, ExtentError> {
        self.logical_start
            .0
            .checked_add(self.sector_count.0)
            .map(Sector)
            .ok_or(ExtentError::Overflow)
    }

    pub fn source_end(self) -> Result<Sector, ExtentError> {
        self.source_start
            .0
            .checked_add(self.sector_count.0)
            .map(Sector)
            .ok_or(ExtentError::Overflow)
    }

    pub fn validate(self) -> Result<(), ExtentError> {
        if self.sector_count.0 == 0 {
            return Err(ExtentError::ZeroLength);
        }
        self.logical_end()?;
        self.source_end()?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtentError {
    ZeroLength,
    Overflow,
}

impl fmt::Display for ExtentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroLength => write!(f, "extent length must be non-zero"),
            Self::Overflow => write!(f, "extent arithmetic overflow"),
        }
    }
}

impl std::error::Error for ExtentError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extent_end_is_checked() {
        let extent = Extent {
            logical_start: Sector(8),
            sector_count: SectorCount(8),
            source: Source::Origin,
            source_start: Sector(32),
        };
        assert_eq!(extent.logical_end().unwrap(), Sector(16));
        assert_eq!(extent.source_end().unwrap(), Sector(40));
    }

    #[test]
    fn zero_length_extent_is_rejected() {
        let extent = Extent {
            logical_start: Sector(0),
            sector_count: SectorCount(0),
            source: Source::Origin,
            source_start: Sector(0),
        };
        assert_eq!(extent.validate(), Err(ExtentError::ZeroLength));
    }
}
