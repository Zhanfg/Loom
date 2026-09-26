use crate::capabilities::{BackendKind, CapabilitySet, ProviderStability};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDescriptor {
    pub name: &'static str,
    pub backend: BackendKind,
    pub stability: ProviderStability,
    pub available: bool,
    pub capabilities: CapabilitySet,
}

#[must_use]
pub const fn overlay_provider(available: bool) -> ProviderDescriptor {
    ProviderDescriptor {
        name: "overlayfs",
        backend: BackendKind::Overlay,
        stability: ProviderStability::Stable,
        available,
        capabilities: CapabilitySet {
            redirect_file: true,
            redirect_dir: true,
            whiteout: true,
            opaque_dir: true,
            symlink: true,
            uid_isolation: false,
            writable_forward: false,
            selinux_fidelity: true,
            xattr_fidelity: true,
            stat_identity: true,
            hot_reload: false,
            mountless: false,
        },
    }
}

#[must_use]
pub const fn nomount_provider(available: bool) -> ProviderDescriptor {
    ProviderDescriptor {
        name: "nomount",
        backend: BackendKind::NoMount,
        stability: ProviderStability::Preview,
        available,
        capabilities: CapabilitySet {
            redirect_file: true,
            redirect_dir: true,
            whiteout: true,
            opaque_dir: false,
            symlink: true,
            uid_isolation: true,
            writable_forward: false,
            selinux_fidelity: true,
            xattr_fidelity: true,
            stat_identity: true,
            hot_reload: true,
            mountless: true,
        },
    }
}

#[must_use]
pub const fn kasumi_provider(available: bool) -> ProviderDescriptor {
    ProviderDescriptor {
        name: "kasumi",
        backend: BackendKind::Kasumi,
        stability: ProviderStability::Experimental,
        available,
        capabilities: CapabilitySet {
            redirect_file: true,
            redirect_dir: true,
            whiteout: true,
            opaque_dir: true,
            symlink: true,
            uid_isolation: true,
            writable_forward: true,
            selinux_fidelity: true,
            xattr_fidelity: true,
            stat_identity: true,
            hot_reload: true,
            mountless: true,
        },
    }
}
