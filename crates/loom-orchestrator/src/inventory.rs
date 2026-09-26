use crate::providers::{kasumi_provider, nomount_provider, overlay_provider, ProviderDescriptor};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RuntimeSignals {
    pub overlayfs: bool,
    pub nomount: bool,
    pub kasumi: bool,
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
                overlay_provider(signals.overlayfs),
                nomount_provider(signals.nomount),
                kasumi_provider(signals.kasumi),
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
            overlayfs: true,
            nomount: true,
            kasumi: true,
        });
        let kasumi = inventory
            .providers()
            .iter()
            .find(|provider| provider.backend == BackendKind::Kasumi)
            .unwrap();

        assert!(kasumi.available);
        assert_eq!(kasumi.stability, ProviderStability::Experimental);
    }
}
