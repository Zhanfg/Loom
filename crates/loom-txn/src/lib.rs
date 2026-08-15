#![forbid(unsafe_code)]

use loom_ext4::{
    compile_create_file, compile_grow_with_block_allocation, compile_remove_file,
    compile_resize_within_allocation, compile_same_size_replacement, compile_selinux_xattr,
    Ext4Error,
};
use loom_map::{LoomMap, MapError, ReplacementExtent};
use loom_types::{Sector, SectorCount, Source};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const SECTOR_SIZE: u64 = 512;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    Replace { path: String, source: PathBuf },
    Resize { path: String, source: PathBuf },
    Grow { path: String, source: PathBuf },
    Create { path: String, source: PathBuf },
    Remove { path: String },
    Selinux { path: String, source: PathBuf },
}

#[derive(Debug)]
pub struct CompiledTransaction {
    pub map: LoomMap,
    pub shadow: Vec<u8>,
    pub operation_count: usize,
    pub changed_sectors: usize,
}

/// Compiles a sequence of ext4 operations against one evolving effective view.
///
/// Stage 7 intentionally uses a private temporary image as the transaction workspace.
/// Each existing compiler reads the output of the prior operation, so metadata collisions
/// are naturally resolved into the final state. The authoritative origin is never opened
/// writable. The final product is a sparse sector diff against that origin.
///
/// # Errors
/// Returns [`TransactionError`] for plan parsing, ext4 compilation, map application,
/// temporary-image I/O, or final diff errors.
pub fn compile_transaction(
    origin_path: &Path,
    plan_path: &Path,
) -> Result<CompiledTransaction, TransactionError> {
    let operations = parse_plan(plan_path)?;
    if operations.is_empty() {
        return Err(TransactionError::EmptyPlan);
    }

    let workspace = TemporaryImage::copy_from(origin_path)?;
    for operation in &operations {
        apply_operation(workspace.path(), operation)?;
    }

    let (map, shadow, changed_sectors) = diff_effective_image(origin_path, workspace.path())?;
    Ok(CompiledTransaction {
        map,
        shadow,
        operation_count: operations.len(),
        changed_sectors,
    })
}

/// Parses a Stage 7 transaction plan.
///
/// The format is tab-separated, one operation per line. Blank lines and `#` comments are
/// ignored. File arguments are resolved relative to the plan file directory.
///
/// Supported forms:
/// `REPLACE <path> <file>`, `RESIZE <path> <file>`, `GROW <path> <file>`,
/// `CREATE <path> <file>`, `REMOVE <path>`, and `SELINUX <path> <file>`.
///
/// # Errors
/// Returns [`TransactionError`] when the plan cannot be read or a line is malformed.
pub fn parse_plan(plan_path: &Path) -> Result<Vec<Operation>, TransactionError> {
    let text = fs::read_to_string(plan_path).map_err(TransactionError::Io)?;
    let base = plan_path.parent().unwrap_or_else(|| Path::new("."));
    let mut operations = Vec::new();

    for (index, raw_line) in text.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = raw_line.split('\t').map(str::trim).collect();
        let command = fields
            .first()
            .ok_or_else(|| TransactionError::PlanLine {
                line: line_number,
                reason: "missing operation".to_string(),
            })?
            .to_ascii_uppercase();

        let operation = match command.as_str() {
            "REPLACE" => operation_with_file(&fields, line_number, base, |path, source| {
                Operation::Replace { path, source }
            })?,
            "RESIZE" => operation_with_file(&fields, line_number, base, |path, source| {
                Operation::Resize { path, source }
            })?,
            "GROW" => operation_with_file(&fields, line_number, base, |path, source| {
                Operation::Grow { path, source }
            })?,
            "CREATE" => operation_with_file(&fields, line_number, base, |path, source| {
                Operation::Create { path, source }
            })?,
            "SELINUX" => operation_with_file(&fields, line_number, base, |path, source| {
                Operation::Selinux { path, source }
            })?,
            "REMOVE" => {
                if fields.len() != 2 || fields[1].is_empty() {
                    return Err(TransactionError::PlanLine {
                        line: line_number,
                        reason: "REMOVE requires exactly one target path".to_string(),
                    });
                }
                Operation::Remove {
                    path: fields[1].to_string(),
                }
            }
            _ => {
                return Err(TransactionError::PlanLine {
                    line: line_number,
                    reason: format!("unknown operation {command:?}"),
                });
            }
        };
        operations.push(operation);
    }
    Ok(operations)
}

fn operation_with_file<F>(
    fields: &[&str],
    line: usize,
    base: &Path,
    build: F,
) -> Result<Operation, TransactionError>
where
    F: FnOnce(String, PathBuf) -> Operation,
{
    if fields.len() != 3 || fields[1].is_empty() || fields[2].is_empty() {
        return Err(TransactionError::PlanLine {
            line,
            reason: "operation requires target path and source file".to_string(),
        });
    }
    let source = Path::new(fields[2]);
    let source = if source.is_absolute() {
        source.to_path_buf()
    } else {
        base.join(source)
    };
    Ok(build(fields[1].to_string(), source))
}

fn apply_operation(working_image: &Path, operation: &Operation) -> Result<(), TransactionError> {
    match operation {
        Operation::Replace { path, source } => {
            let compiled = compile_same_size_replacement(working_image, path, source)?;
            apply_shadow_map(working_image, &compiled.map, &compiled.shadow)
        }
        Operation::Resize { path, source } => {
            let compiled = compile_resize_within_allocation(working_image, path, source)?;
            apply_shadow_map(working_image, &compiled.map, &compiled.shadow)
        }
        Operation::Grow { path, source } => {
            let compiled = compile_grow_with_block_allocation(working_image, path, source)?;
            apply_shadow_map(working_image, &compiled.map, &compiled.shadow)
        }
        Operation::Create { path, source } => {
            let compiled = compile_create_file(working_image, path, source)?;
            apply_shadow_map(working_image, &compiled.map, &compiled.shadow)
        }
        Operation::Remove { path } => {
            let compiled = compile_remove_file(working_image, path)?;
            apply_shadow_map(working_image, &compiled.map, &compiled.shadow)
        }
        Operation::Selinux { path, source } => {
            let compiled = compile_selinux_xattr(working_image, path, source)?;
            apply_shadow_map(working_image, &compiled.map, &compiled.shadow)
        }
    }
}

fn apply_shadow_map(
    working_image: &Path,
    map: &LoomMap,
    shadow: &[u8],
) -> Result<(), TransactionError> {
    let mut image = OpenOptions::new()
        .read(true)
        .write(true)
        .open(working_image)
        .map_err(TransactionError::Io)?;

    for extent in map.extents() {
        if extent.source != Source::Shadow {
            continue;
        }
        let byte_count = extent
            .sector_count
            .0
            .checked_mul(SECTOR_SIZE)
            .ok_or(TransactionError::ArithmeticOverflow)?;
        let shadow_start = extent
            .source_start
            .0
            .checked_mul(SECTOR_SIZE)
            .ok_or(TransactionError::ArithmeticOverflow)?;
        let shadow_end = shadow_start
            .checked_add(byte_count)
            .ok_or(TransactionError::ArithmeticOverflow)?;
        let logical_start = extent
            .logical_start
            .0
            .checked_mul(SECTOR_SIZE)
            .ok_or(TransactionError::ArithmeticOverflow)?;
        let shadow_start =
            usize::try_from(shadow_start).map_err(|_| TransactionError::ArithmeticOverflow)?;
        let shadow_end =
            usize::try_from(shadow_end).map_err(|_| TransactionError::ArithmeticOverflow)?;
        let bytes = shadow
            .get(shadow_start..shadow_end)
            .ok_or(TransactionError::ShadowOutOfBounds)?;
        image
            .seek(SeekFrom::Start(logical_start))
            .map_err(TransactionError::Io)?;
        image.write_all(bytes).map_err(TransactionError::Io)?;
    }
    image.flush().map_err(TransactionError::Io)?;
    Ok(())
}

fn diff_effective_image(
    origin_path: &Path,
    effective_path: &Path,
) -> Result<(LoomMap, Vec<u8>, usize), TransactionError> {
    let mut origin = File::open(origin_path).map_err(TransactionError::Io)?;
    let mut effective = File::open(effective_path).map_err(TransactionError::Io)?;
    let origin_len = origin.metadata().map_err(TransactionError::Io)?.len();
    let effective_len = effective.metadata().map_err(TransactionError::Io)?.len();
    if origin_len != effective_len || origin_len % SECTOR_SIZE != 0 {
        return Err(TransactionError::ImageSizeMismatch {
            origin: origin_len,
            effective: effective_len,
        });
    }

    let total_sectors = origin_len / SECTOR_SIZE;
    let mut origin_sector = [0_u8; SECTOR_SIZE as usize];
    let mut effective_sector = [0_u8; SECTOR_SIZE as usize];
    let mut shadow = Vec::new();
    let mut replacements = Vec::new();
    let mut changed = 0_usize;

    for sector in 0..total_sectors {
        origin
            .read_exact(&mut origin_sector)
            .map_err(TransactionError::Io)?;
        effective
            .read_exact(&mut effective_sector)
            .map_err(TransactionError::Io)?;
        if origin_sector == effective_sector {
            continue;
        }
        let shadow_start =
            u64::try_from(changed).map_err(|_| TransactionError::ArithmeticOverflow)?;
        replacements.push(ReplacementExtent {
            logical_start: Sector(sector),
            sector_count: SectorCount(1),
            shadow_start: Sector(shadow_start),
        });
        shadow.extend_from_slice(&effective_sector);
        changed = changed
            .checked_add(1)
            .ok_or(TransactionError::ArithmeticOverflow)?;
    }

    let map = LoomMap::from_replacements(SectorCount(total_sectors), &replacements)?;
    Ok((map, shadow, changed))
}

struct TemporaryImage {
    path: PathBuf,
}

impl TemporaryImage {
    fn copy_from(origin: &Path) -> Result<Self, TransactionError> {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| TransactionError::ClockBeforeEpoch)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "loom-txn-{}-{nanos}-{sequence}.img",
            std::process::id()
        ));
        fs::copy(origin, &path).map_err(TransactionError::Io)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryImage {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Debug)]
pub enum TransactionError {
    Io(io::Error),
    Ext4(Ext4Error),
    Map(MapError),
    EmptyPlan,
    PlanLine { line: usize, reason: String },
    ArithmeticOverflow,
    ShadowOutOfBounds,
    ImageSizeMismatch { origin: u64, effective: u64 },
    ClockBeforeEpoch,
}

impl From<Ext4Error> for TransactionError {
    fn from(value: Ext4Error) -> Self {
        Self::Ext4(value)
    }
}

impl From<MapError> for TransactionError {
    fn from(value: MapError) -> Self {
        Self::Map(value)
    }
}

impl fmt::Display for TransactionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "transaction I/O error: {error}"),
            Self::Ext4(error) => write!(f, "ext4 transaction operation failed: {error}"),
            Self::Map(error) => write!(f, "transaction map failed: {error}"),
            Self::EmptyPlan => write!(f, "transaction plan contains no operations"),
            Self::PlanLine { line, reason } => {
                write!(f, "invalid transaction plan line {line}: {reason}")
            }
            Self::ArithmeticOverflow => write!(f, "transaction address arithmetic overflow"),
            Self::ShadowOutOfBounds => write!(f, "compiled shadow extent lies outside shadow pack"),
            Self::ImageSizeMismatch { origin, effective } => write!(
                f,
                "transaction working image size mismatch: origin={origin}, effective={effective}"
            ),
            Self::ClockBeforeEpoch => write!(f, "system clock predates Unix epoch"),
        }
    }
}

impl std::error::Error for TransactionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_parser_resolves_relative_sources() {
        let base = std::env::temp_dir().join(format!(
            "loom-plan-test-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&base).unwrap();
        let plan = base.join("plan.tsv");
        fs::write(
            &plan,
            "CREATE\t/system/etc/a\tpayload.bin\nSELINUX\t/system/etc/a\tctx.bin\nREMOVE\t/system/etc/b\n",
        )
        .unwrap();
        let operations = parse_plan(&plan).unwrap();
        assert_eq!(operations.len(), 3);
        assert!(matches!(
            &operations[0],
            Operation::Create { source, .. } if source == &base.join("payload.bin")
        ));
        let _ = fs::remove_dir_all(base);
    }
}
