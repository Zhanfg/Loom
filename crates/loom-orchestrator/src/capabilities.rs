#![allow(clippy::struct_excessive_bools)]

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BackendKind {
    Overlay,
    NoMount,
    Kasumi,
    Magic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProviderStability {
    Stable,
    Preview,
    Experimental,
    ReferenceOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CapabilitySet {
    pub redirect_file: bool,
    pub redirect_dir: bool,
    pub whiteout: bool,
    pub opaque_dir: bool,
    pub symlink: bool,
    pub uid_isolation: bool,
    pub writable_forward: bool,
    pub selinux_fidelity: bool,
    pub xattr_fidelity: bool,
    pub stat_identity: bool,
    pub hot_reload: bool,
    pub mountless: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Requirements {
    pub redirect_file: bool,
    pub redirect_dir: bool,
    pub whiteout: bool,
    pub opaque_dir: bool,
    pub symlink: bool,
    pub uid_isolation: bool,
    pub writable_forward: bool,
    pub selinux_fidelity: bool,
    pub xattr_fidelity: bool,
    pub stat_identity: bool,
    pub hot_reload: bool,
    pub mountless: bool,
}

impl CapabilitySet {
    #[must_use]
    pub const fn satisfies(self, req: Requirements) -> bool {
        (!req.redirect_file || self.redirect_file)
            && (!req.redirect_dir || self.redirect_dir)
            && (!req.whiteout || self.whiteout)
            && (!req.opaque_dir || self.opaque_dir)
            && (!req.symlink || self.symlink)
            && (!req.uid_isolation || self.uid_isolation)
            && (!req.writable_forward || self.writable_forward)
            && (!req.selinux_fidelity || self.selinux_fidelity)
            && (!req.xattr_fidelity || self.xattr_fidelity)
            && (!req.stat_identity || self.stat_identity)
            && (!req.hot_reload || self.hot_reload)
            && (!req.mountless || self.mountless)
    }
}
