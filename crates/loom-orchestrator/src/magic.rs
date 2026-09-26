use core::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug)]
pub enum MagicError {
    Io(io::Error),
    InvalidSpec(String),
    Command(String),
    Verification(String),
}

impl fmt::Display for MagicError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "Magic Mount I/O error: {error}"),
            Self::InvalidSpec(detail) => write!(f, "invalid Magic Mount spec: {detail}"),
            Self::Command(detail) => write!(f, "Magic Mount command error: {detail}"),
            Self::Verification(detail) => write!(f, "Magic Mount verification error: {detail}"),
        }
    }
}

impl std::error::Error for MagicError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::InvalidSpec(_) | Self::Command(_) | Self::Verification(_) => None,
        }
    }
}

impl From<io::Error> for MagicError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MagicSpec {
    pub source: PathBuf,
    pub target: PathBuf,
    pub read_only: bool,
}

impl MagicSpec {
    /// Validates a Magic Mount bind specification.
    ///
    /// # Errors
    /// Returns `MagicError` when source/target are relative or identical.
    pub fn validate(&self) -> Result<(), MagicError> {
        if !self.source.is_absolute() {
            return Err(MagicError::InvalidSpec(format!(
                "source must be absolute: {}",
                self.source.display()
            )));
        }
        if !self.target.is_absolute() {
            return Err(MagicError::InvalidSpec(format!(
                "target must be absolute: {}",
                self.target.display()
            )));
        }
        if self.source == self.target {
            return Err(MagicError::InvalidSpec(
                "source and target must differ".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MagicToken {
    pub target: PathBuf,
    pub previous_mount_id: Option<u64>,
}

pub trait MagicRuntime {
    /// Creates one bind mount.
    ///
    /// # Errors
    /// Returns `MagicError` when the source/target is unavailable or the bind fails.
    fn bind_mount(&mut self, spec: &MagicSpec) -> Result<(), MagicError>;

    /// Returns the current mountinfo view.
    ///
    /// # Errors
    /// Returns `MagicError` when mountinfo cannot be read.
    fn mountinfo(&mut self) -> Result<String, MagicError>;

    /// Removes one bind mount.
    ///
    /// # Errors
    /// Returns `MagicError` when normal and lazy unmount both fail.
    fn unmount(&mut self, target: &Path) -> Result<(), MagicError>;
}

#[derive(Debug, Clone)]
pub struct CommandMagicRuntime {
    mount_program: PathBuf,
    umount_program: PathBuf,
}

impl Default for CommandMagicRuntime {
    fn default() -> Self {
        Self {
            mount_program: if Path::new("/system/bin/mount").exists() {
                PathBuf::from("/system/bin/mount")
            } else {
                PathBuf::from("mount")
            },
            umount_program: if Path::new("/system/bin/umount").exists() {
                PathBuf::from("/system/bin/umount")
            } else {
                PathBuf::from("umount")
            },
        }
    }
}

impl CommandMagicRuntime {
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

    fn run_umount(&self, target: &Path, lazy: bool) -> Result<(), MagicError> {
        let mut command = Command::new(&self.umount_program);
        if lazy {
            command.arg("-l");
        }
        let output = command.arg(target).output()?;
        if output.status.success() {
            return Ok(());
        }
        Err(MagicError::Command(format!(
            "{} {} failed with {}: {}",
            self.umount_program.display(),
            target.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

impl MagicRuntime for CommandMagicRuntime {
    fn bind_mount(&mut self, spec: &MagicSpec) -> Result<(), MagicError> {
        spec.validate()?;
        if !spec.source.exists() {
            return Err(MagicError::InvalidSpec(format!(
                "source does not exist: {}",
                spec.source.display()
            )));
        }
        if !spec.target.exists() {
            return Err(MagicError::InvalidSpec(format!(
                "target does not exist: {}; tmpfs skeleton fallback is required",
                spec.target.display()
            )));
        }

        let output = Command::new(&self.mount_program)
            .arg("--bind")
            .arg(&spec.source)
            .arg(&spec.target)
            .output()?;
        if !output.status.success() {
            return Err(MagicError::Command(format!(
                "{} --bind failed with {}: {}",
                self.mount_program.display(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }

        if spec.read_only {
            let remount = Command::new(&self.mount_program)
                .arg("-o")
                .arg("remount,bind,ro")
                .arg(&spec.target)
                .output()?;
            if !remount.status.success() {
                let remount_error = MagicError::Command(format!(
                    "read-only remount failed with {}: {}",
                    remount.status,
                    String::from_utf8_lossy(&remount.stderr).trim()
                ));
                let rollback = self.unmount(&spec.target);
                return match rollback {
                    Ok(()) => Err(remount_error),
                    Err(rollback_error) => Err(MagicError::Command(format!(
                        "{remount_error}; rollback also failed: {rollback_error}"
                    ))),
                };
            }
        }
        Ok(())
    }

    fn mountinfo(&mut self) -> Result<String, MagicError> {
        Ok(fs::read_to_string("/proc/self/mountinfo")?)
    }

    fn unmount(&mut self, target: &Path) -> Result<(), MagicError> {
        match self.run_umount(target, false) {
            Ok(()) => Ok(()),
            Err(first) => self.run_umount(target, true).map_err(|second| {
                MagicError::Command(format!(
                    "normal unmount failed: {first}; lazy unmount failed: {second}"
                ))
            }),
        }
    }
}

pub struct MagicAdapter<R> {
    runtime: R,
}

impl<R: MagicRuntime> MagicAdapter<R> {
    #[must_use]
    pub const fn new(runtime: R) -> Self {
        Self { runtime }
    }

    #[must_use]
    pub fn into_inner(self) -> R {
        self.runtime
    }

    /// Applies one Magic Mount bind and records the mount ID previously visible at target.
    ///
    /// # Errors
    /// Returns `MagicError` when validation, mountinfo read, or bind mount fails.
    pub fn apply(&mut self, spec: &MagicSpec) -> Result<MagicToken, MagicError> {
        spec.validate()?;
        let previous_mount_id = mountinfo_mount_id(&self.runtime.mountinfo()?, &spec.target);
        self.runtime.bind_mount(spec)?;
        Ok(MagicToken {
            target: spec.target.clone(),
            previous_mount_id,
        })
    }

    /// Verifies that apply created a new mount layer at the target.
    ///
    /// # Errors
    /// Returns `MagicError` when mountinfo cannot be read or the mount ID did not change.
    pub fn verify(&mut self, token: &MagicToken) -> Result<(), MagicError> {
        let current_mount_id = mountinfo_mount_id(&self.runtime.mountinfo()?, &token.target);
        if current_mount_id.is_some() && current_mount_id != token.previous_mount_id {
            return Ok(());
        }
        Err(MagicError::Verification(format!(
            "{} did not gain a new mount layer",
            token.target.display()
        )))
    }

    /// Rolls back one Magic Mount bind.
    ///
    /// # Errors
    /// Returns `MagicError` when the mount cannot be removed.
    pub fn rollback(&mut self, token: &MagicToken) -> Result<(), MagicError> {
        self.runtime.unmount(&token.target)
    }
}

fn decode_mountinfo_path(raw: &str) -> String {
    raw.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

#[must_use]
pub fn mountinfo_mount_id(mountinfo: &str, target: &Path) -> Option<u64> {
    let target = target.to_string_lossy();
    mountinfo
        .lines()
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.len() < 6 || decode_mountinfo_path(fields[4]) != target {
                return None;
            }
            fields[0].parse::<u64>().ok()
        })
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeRuntime {
        mountinfo_before: String,
        mountinfo_after: String,
        mounted: bool,
        unmounted: Vec<PathBuf>,
    }

    impl MagicRuntime for FakeRuntime {
        fn bind_mount(&mut self, _spec: &MagicSpec) -> Result<(), MagicError> {
            self.mounted = true;
            Ok(())
        }

        fn mountinfo(&mut self) -> Result<String, MagicError> {
            if self.mounted {
                Ok(self.mountinfo_after.clone())
            } else {
                Ok(self.mountinfo_before.clone())
            }
        }

        fn unmount(&mut self, target: &Path) -> Result<(), MagicError> {
            self.unmounted.push(target.to_path_buf());
            self.mounted = false;
            Ok(())
        }
    }

    fn spec() -> MagicSpec {
        MagicSpec {
            source: PathBuf::from("/data/adb/modules/demo/system/etc/hosts"),
            target: PathBuf::from("/system/etc/hosts"),
            read_only: true,
        }
    }

    #[test]
    fn mount_id_parser_returns_topmost_target_layer() {
        let info = concat!(
            "20 1 0:1 / /system/etc/hosts rw - ext4 /dev/a rw\n",
            "31 20 0:2 / /system/etc/hosts ro - ext4 /dev/b ro\n"
        );
        assert_eq!(
            mountinfo_mount_id(info, Path::new("/system/etc/hosts")),
            Some(31)
        );
    }

    #[test]
    fn adapter_requires_new_mount_id() {
        let runtime = FakeRuntime {
            mountinfo_before: "20 1 0:1 / /system/etc/hosts rw - ext4 /dev/a rw\n".to_owned(),
            mountinfo_after: concat!(
                "20 1 0:1 / /system/etc/hosts rw - ext4 /dev/a rw\n",
                "31 20 0:1 / /system/etc/hosts ro - ext4 /dev/a ro\n"
            )
            .to_owned(),
            ..FakeRuntime::default()
        };
        let mut adapter = MagicAdapter::new(runtime);
        let token = adapter.apply(&spec()).unwrap();
        adapter.verify(&token).unwrap();
        adapter.rollback(&token).unwrap();
        assert_eq!(
            adapter.into_inner().unmounted,
            vec![PathBuf::from("/system/etc/hosts")]
        );
    }
}
