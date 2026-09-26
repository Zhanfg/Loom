use core::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug)]
pub enum OverlayError {
    Io(io::Error),
    InvalidSpec(String),
    Command(String),
    Verification(String),
}

impl fmt::Display for OverlayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "OverlayFS I/O error: {error}"),
            Self::InvalidSpec(detail) => write!(f, "invalid OverlayFS spec: {detail}"),
            Self::Command(detail) => write!(f, "OverlayFS command error: {detail}"),
            Self::Verification(detail) => write!(f, "OverlayFS verification error: {detail}"),
        }
    }
}

impl std::error::Error for OverlayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::InvalidSpec(_) | Self::Command(_) | Self::Verification(_) => None,
        }
    }
}

impl From<io::Error> for OverlayError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlaySpec {
    pub target: PathBuf,
    pub lowerdirs: Vec<PathBuf>,
    pub upperdir: Option<PathBuf>,
    pub workdir: Option<PathBuf>,
    pub read_only: bool,
}

impl OverlaySpec {
    /// Validates an OverlayFS mount specification.
    ///
    /// # Errors
    /// Returns OverlayError when required paths are absent, relative, unsafe for
    /// the option grammar, or upper/work are not supplied as a pair.
    pub fn validate(&self) -> Result<(), OverlayError> {
        validate_path(&self.target, "target")?;
        if self.lowerdirs.is_empty() {
            return Err(OverlayError::InvalidSpec(
                "at least one lowerdir is required".to_owned(),
            ));
        }
        for lower in &self.lowerdirs {
            validate_option_path(lower, "lowerdir")?;
        }

        match (&self.upperdir, &self.workdir) {
            (Some(upper), Some(work)) => {
                validate_option_path(upper, "upperdir")?;
                validate_option_path(work, "workdir")?;
                if self.read_only {
                    return Err(OverlayError::InvalidSpec(
                        "read-only overlay must not define upperdir/workdir".to_owned(),
                    ));
                }
            }
            (None, None) => {}
            _ => {
                return Err(OverlayError::InvalidSpec(
                    "upperdir and workdir must be supplied together".to_owned(),
                ));
            }
        }
        Ok(())
    }

    /// Builds the Linux OverlayFS data option string.
    ///
    /// # Errors
    /// Returns OverlayError when the specification is invalid or a path is not
    /// representable in the conservative option grammar.
    pub fn options(&self) -> Result<String, OverlayError> {
        self.validate()?;
        let lowers = self
            .lowerdirs
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(":");
        let mut options = vec![format!("lowerdir={lowers}")];

        if let (Some(upper), Some(work)) = (&self.upperdir, &self.workdir) {
            options.push(format!("upperdir={}", upper.display()));
            options.push(format!("workdir={}", work.display()));
        }
        if self.read_only {
            options.push("ro".to_owned());
        }
        Ok(options.join(","))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayToken {
    pub target: PathBuf,
}

pub trait OverlayRuntime {
    /// Mounts one OverlayFS specification.
    ///
    /// # Errors
    /// Returns OverlayError when the mount cannot be created.
    fn mount(&mut self, spec: &OverlaySpec) -> Result<(), OverlayError>;

    /// Returns the current process mountinfo view.
    ///
    /// # Errors
    /// Returns OverlayError when mountinfo cannot be read.
    fn mountinfo(&mut self) -> Result<String, OverlayError>;

    /// Removes one mount target.
    ///
    /// # Errors
    /// Returns OverlayError when both normal and lazy unmount fail.
    fn unmount(&mut self, target: &Path) -> Result<(), OverlayError>;
}

#[derive(Debug, Clone)]
pub struct CommandOverlayRuntime {
    mount_program: PathBuf,
    umount_program: PathBuf,
}

impl Default for CommandOverlayRuntime {
    fn default() -> Self {
        let android_mount = PathBuf::from("/system/bin/mount");
        let android_umount = PathBuf::from("/system/bin/umount");
        Self {
            mount_program: if android_mount.exists() {
                android_mount
            } else {
                PathBuf::from("mount")
            },
            umount_program: if android_umount.exists() {
                android_umount
            } else {
                PathBuf::from("umount")
            },
        }
    }
}

impl CommandOverlayRuntime {
    #[must_use]
    pub fn with_programs(
        mount_program: impl Into<PathBuf>,
        umount_program: impl Into<PathBuf>,
    ) -> Self {
        Self {
            mount_program: mount_program.into(),
            umount_program: umount_program.into(),
        }
    }

    fn run_umount(&self, target: &Path, lazy: bool) -> Result<(), OverlayError> {
        let mut command = Command::new(&self.umount_program);
        if lazy {
            command.arg("-l");
        }
        let output = command.arg(target).output()?;
        if output.status.success() {
            return Ok(());
        }
        Err(OverlayError::Command(format!(
            "{} {} failed with {}: {}",
            self.umount_program.display(),
            target.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

impl OverlayRuntime for CommandOverlayRuntime {
    fn mount(&mut self, spec: &OverlaySpec) -> Result<(), OverlayError> {
        let options = spec.options()?;
        let output = Command::new(&self.mount_program)
            .arg("-t")
            .arg("overlay")
            .arg("overlay")
            .arg("-o")
            .arg(options)
            .arg(&spec.target)
            .output()?;
        if output.status.success() {
            return Ok(());
        }
        Err(OverlayError::Command(format!(
            "{} failed with {}: {}",
            self.mount_program.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }

    fn mountinfo(&mut self) -> Result<String, OverlayError> {
        Ok(fs::read_to_string("/proc/self/mountinfo")?)
    }

    fn unmount(&mut self, target: &Path) -> Result<(), OverlayError> {
        match self.run_umount(target, false) {
            Ok(()) => Ok(()),
            Err(first) => self.run_umount(target, true).map_err(|second| {
                OverlayError::Command(format!(
                    "normal unmount failed: {first}; lazy unmount failed: {second}"
                ))
            }),
        }
    }
}

pub struct OverlayAdapter<R> {
    runtime: R,
}

impl<R: OverlayRuntime> OverlayAdapter<R> {
    #[must_use]
    pub const fn new(runtime: R) -> Self {
        Self { runtime }
    }

    #[must_use]
    pub fn into_inner(self) -> R {
        self.runtime
    }

    /// Applies one OverlayFS mount and returns a rollback token.
    ///
    /// # Errors
    /// Returns OverlayError when the specification is invalid or mount fails.
    pub fn apply(&mut self, spec: &OverlaySpec) -> Result<OverlayToken, OverlayError> {
        spec.validate()?;
        self.runtime.mount(spec)?;
        Ok(OverlayToken {
            target: spec.target.clone(),
        })
    }

    /// Verifies the applied target is currently backed by OverlayFS.
    ///
    /// # Errors
    /// Returns OverlayError when mountinfo cannot be read or the target is not
    /// present as an overlay mount.
    pub fn verify(&mut self, token: &OverlayToken) -> Result<(), OverlayError> {
        let mountinfo = self.runtime.mountinfo()?;
        if mountinfo_has_overlay(&mountinfo, &token.target) {
            return Ok(());
        }
        Err(OverlayError::Verification(format!(
            "{} is not visible as OverlayFS in mountinfo",
            token.target.display()
        )))
    }

    /// Rolls back one applied OverlayFS mount.
    ///
    /// # Errors
    /// Returns OverlayError when the mount cannot be removed.
    pub fn rollback(&mut self, token: OverlayToken) -> Result<(), OverlayError> {
        self.runtime.unmount(&token.target)
    }
}

fn validate_path(path: &Path, label: &str) -> Result<(), OverlayError> {
    if !path.is_absolute() {
        return Err(OverlayError::InvalidSpec(format!(
            "{label} must be absolute: {}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_option_path(path: &Path, label: &str) -> Result<(), OverlayError> {
    validate_path(path, label)?;
    let text = path.to_string_lossy();
    if text
        .chars()
        .any(|character| matches!(character, ',' | ':' | '\n' | '\r'))
    {
        return Err(OverlayError::InvalidSpec(format!(
            "{label} contains an unsupported option separator: {text}"
        )));
    }
    Ok(())
}

fn decode_mountinfo_path(raw: &str) -> String {
    raw.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

#[must_use]
pub fn mountinfo_has_overlay(mountinfo: &str, target: &Path) -> bool {
    let target = target.to_string_lossy();
    mountinfo.lines().any(|line| {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 7 {
            return false;
        }
        let Some(separator) = fields.iter().position(|field| *field == "-") else {
            return false;
        };
        if separator + 1 >= fields.len() {
            return false;
        }
        decode_mountinfo_path(fields[4]) == target && fields[separator + 1] == "overlay"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeRuntime {
        mounted: Vec<PathBuf>,
        unmounted: Vec<PathBuf>,
        mountinfo: String,
    }

    impl OverlayRuntime for FakeRuntime {
        fn mount(&mut self, spec: &OverlaySpec) -> Result<(), OverlayError> {
            self.mounted.push(spec.target.clone());
            Ok(())
        }

        fn mountinfo(&mut self) -> Result<String, OverlayError> {
            Ok(self.mountinfo.clone())
        }

        fn unmount(&mut self, target: &Path) -> Result<(), OverlayError> {
            self.unmounted.push(target.to_path_buf());
            Ok(())
        }
    }

    fn spec() -> OverlaySpec {
        OverlaySpec {
            target: PathBuf::from("/system"),
            lowerdirs: vec![
                PathBuf::from("/data/adb/modules/a/system"),
                PathBuf::from("/system"),
            ],
            upperdir: None,
            workdir: None,
            read_only: true,
        }
    }

    #[test]
    fn lower_only_options_are_stable() {
        assert_eq!(
            spec().options().unwrap(),
            "lowerdir=/data/adb/modules/a/system:/system,ro"
        );
    }

    #[test]
    fn mountinfo_verifies_fstype_and_target() {
        let info = "36 25 0:32 / /system rw,relatime - overlay overlay rw,lowerdir=/x:/system\n";
        assert!(mountinfo_has_overlay(info, Path::new("/system")));
        assert!(!mountinfo_has_overlay(info, Path::new("/vendor")));
    }

    #[test]
    fn adapter_applies_verifies_and_rolls_back() {
        let runtime = FakeRuntime {
            mountinfo:
                "36 25 0:32 / /system rw,relatime - overlay overlay rw,lowerdir=/x:/system\n"
                    .to_owned(),
            ..FakeRuntime::default()
        };
        let mut adapter = OverlayAdapter::new(runtime);
        let token = adapter.apply(&spec()).unwrap();
        adapter.verify(&token).unwrap();
        adapter.rollback(token).unwrap();
        let runtime = adapter.into_inner();
        assert_eq!(runtime.mounted, vec![PathBuf::from("/system")]);
        assert_eq!(runtime.unmounted, vec![PathBuf::from("/system")]);
    }
}
