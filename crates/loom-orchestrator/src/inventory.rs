use crate::providers::{
    kasumi_provider, magic_provider, nomount_provider, overlay_provider, ProviderDescriptor,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Availability {
    #[default]
    Unavailable,
    Available,
}

impl Availability {
    #[must_use]
    pub const fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }
}

impl From<bool> for Availability {
    fn from(value: bool) -> Self {
        if value {
            Self::Available
        } else {
            Self::Unavailable
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RuntimeSignals {
    pub overlayfs: Availability,
    pub nomount: Availability,
    pub magic: Availability,
    pub kasumi: Availability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInventory {
    providers: Vec<ProviderDescriptor>,
}

impl ProviderInventory {
    #[must_use]
    pub fn from_signals(signals: RuntimeSignals) -> Self {
        Self {
            providers: vec![
                overlay_provider(signals.overlayfs.is_available()),
                nomount_provider(signals.nomount.is_available()),
                magic_provider(signals.magic.is_available()),
                kasumi_provider(signals.kasumi.is_available()),
            ],
        }
    }

    #[must_use]
    pub fn providers(&self) -> &[ProviderDescriptor] {
        &self.providers
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BackendKind, ProviderStability};

    #[test]
    fn availability_does_not_promote_kasumi_stability() {
        let inventory = ProviderInventory::from_signals(RuntimeSignals {
            overlayfs: Availability::Available,
            nomount: Availability::Available,
            magic: Availability::Available,
            kasumi: Availability::Available,
        });
        let kasumi = inventory
            .providers()
            .iter()
            .find(|provider| provider.backend == BackendKind::Kasumi)
            .unwrap();

        assert!(kasumi.available);
        assert_eq!(kasumi.stability, ProviderStability::Experimental);
    }

    #[test]
    fn bool_conversion_is_explicit_and_stable() {
        assert_eq!(Availability::from(true), Availability::Available);
        assert_eq!(Availability::from(false), Availability::Unavailable);
    }
}
