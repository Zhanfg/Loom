use std::fs;
use std::path::Path;

use crate::inventory::{Availability, RuntimeSignals};
use crate::nomount::{NomountClient, SystemNomountTransport, NOMOUNT_VERSION};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeProbe {
    pub signals: RuntimeSignals,
    pub nomount_version: Option<String>,
    pub notes: Vec<String>,
}

impl RuntimeProbe {
    #[must_use]
    pub fn detect() -> Self {
        let mut notes = Vec::new();
        let overlayfs = match fs::read_to_string("/proc/filesystems") {
            Ok(filesystems) => {
                let present = filesystems_has_overlay(&filesystems);
                if !present {
                    notes.push("OverlayFS is absent from /proc/filesystems".to_owned());
                }
                present
            }
            Err(error) => {
                notes.push(format!("cannot read /proc/filesystems: {error}"));
                false
            }
        };

        let magic = ["/system/bin/mount", "/bin/mount", "/usr/bin/mount"]
            .iter()
            .any(|candidate| Path::new(candidate).is_file());
        if !magic {
            notes.push("Magic Mount helper was not found at a known absolute path".to_owned());
        }

        let (nomount, nomount_version) = match SystemNomountTransport::new() {
            Ok(transport) => {
                let mut client = NomountClient::new(transport);
                match client.version() {
                    Ok(version) if version == NOMOUNT_VERSION => (true, Some(version)),
                    Ok(version) => {
                        notes.push(format!(
                            "NoMount version {version} is present; Loom expects {NOMOUNT_VERSION}"
                        ));
                        (false, Some(version))
                    }
                    Err(error) => {
                        notes.push(format!("NoMount probe failed: {error}"));
                        (false, None)
                    }
                }
            }
            Err(error) => {
                notes.push(format!("NoMount transport setup failed: {error}"));
                (false, None)
            }
        };

        notes.push(
            "Kasumi auto-selection remains disabled; availability does not promote it beyond experimental"
                .to_owned(),
        );

        Self {
            signals: RuntimeSignals {
                overlayfs: Availability::from(overlayfs),
                nomount: Availability::from(nomount),
                magic: Availability::from(magic),
                kasumi: Availability::Unavailable,
            },
            nomount_version,
            notes,
        }
    }
}

#[must_use]
fn filesystems_has_overlay(filesystems: &str) -> bool {
    filesystems.lines().any(|line| {
        line.split_whitespace()
            .last()
            .is_some_and(|name| name == "overlay")
    })
}

#[cfg(test)]
mod tests {
    use super::filesystems_has_overlay;

    #[test]
    fn detects_overlay_with_or_without_nodev_prefix() {
        assert!(filesystems_has_overlay(
            "nodev\tsysfs\nnodev\toverlay\next4\n"
        ));
        assert!(filesystems_has_overlay("ext4\noverlay\n"));
        assert!(!filesystems_has_overlay("ext4\nerofs\n"));
    }
}
