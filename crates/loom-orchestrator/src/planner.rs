use core::fmt;

use crate::capabilities::{BackendKind, ProviderStability, Requirements};
use crate::providers::ProviderDescriptor;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRequest {
    pub path: String,
    pub requirements: Requirements,
    pub preferred_backend: Option<BackendKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    pub path: String,
    pub provider: &'static str,
    pub backend: BackendKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlanPolicy {
    pub allow_experimental: bool,
    pub allow_reference_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    NoProvider { path: String },
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoProvider { path } => write!(f, "no provider can satisfy path {path}"),
        }
    }
}

impl std::error::Error for PlanError {}

fn allowed(provider: &ProviderDescriptor, policy: PlanPolicy) -> bool {
    match provider.stability {
        ProviderStability::Stable | ProviderStability::Preview => true,
        ProviderStability::Experimental => policy.allow_experimental,
        ProviderStability::ReferenceOnly => policy.allow_reference_only,
    }
}

const fn backend_rank(kind: BackendKind, req: Requirements) -> u8 {
    if req.mountless || req.uid_isolation || req.hot_reload {
        return match kind {
            BackendKind::NoMount => 0,
            BackendKind::Overlay => 1,
            BackendKind::Magic => 2,
            BackendKind::Kasumi => 3,
        };
    }

    match kind {
        BackendKind::Overlay => 0,
        BackendKind::NoMount => 1,
        BackendKind::Magic => 2,
        BackendKind::Kasumi => 3,
    }
}

fn choose_provider<'a>(
    providers: &'a [ProviderDescriptor],
    request: &PlanRequest,
    policy: PlanPolicy,
) -> Option<&'a ProviderDescriptor> {
    if let Some(provider) = request.preferred_backend.and_then(|preferred| {
        providers.iter().find(|provider| {
            provider.backend == preferred
                && provider.available
                && allowed(provider, policy)
                && provider.capabilities.satisfies(request.requirements)
        })
    }) {
        return Some(provider);
    }

    let mut candidates = providers
        .iter()
        .filter(|provider| {
            provider.available
                && allowed(provider, policy)
                && provider.capabilities.satisfies(request.requirements)
        })
        .collect::<Vec<_>>();

    candidates.sort_by_key(|provider| backend_rank(provider.backend, request.requirements));
    candidates.into_iter().next()
}

/// Builds a provider assignment for every requested path.
///
/// # Errors
/// Returns [`PlanError::NoProvider`] when no available provider allowed by the
/// current stability policy satisfies a path's capability requirements.
pub fn plan_requests(
    providers: &[ProviderDescriptor],
    requests: &[PlanRequest],
    policy: PlanPolicy,
) -> Result<Vec<PlanEntry>, PlanError> {
    requests
        .iter()
        .map(|request| {
            let provider = choose_provider(providers, request, policy).ok_or_else(|| {
                PlanError::NoProvider {
                    path: request.path.clone(),
                }
            })?;

            Ok(PlanEntry {
                path: request.path.clone(),
                provider: provider.name,
                backend: provider.backend,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{kasumi_provider, nomount_provider, overlay_provider};

    #[test]
    fn ordinary_path_prefers_overlay() {
        let providers = [
            overlay_provider(true),
            nomount_provider(true),
            kasumi_provider(true),
        ];
        let request = PlanRequest {
            path: "/system/etc/hosts".into(),
            requirements: Requirements {
                redirect_file: true,
                selinux_fidelity: true,
                ..Requirements::default()
            },
            preferred_backend: None,
        };

        let plan = plan_requests(&providers, &[request], PlanPolicy::default()).unwrap();
        assert_eq!(plan[0].backend, BackendKind::Overlay);
    }

    #[test]
    fn mountless_path_prefers_nomount() {
        let providers = [
            overlay_provider(true),
            nomount_provider(true),
            kasumi_provider(true),
        ];
        let request = PlanRequest {
            path: "/vendor/etc/audio_effects.xml".into(),
            requirements: Requirements {
                redirect_file: true,
                mountless: true,
                ..Requirements::default()
            },
            preferred_backend: None,
        };

        let plan = plan_requests(&providers, &[request], PlanPolicy::default()).unwrap();
        assert_eq!(plan[0].backend, BackendKind::NoMount);
    }

    #[test]
    fn kasumi_is_not_selected_by_default() {
        let providers = [
            overlay_provider(false),
            nomount_provider(false),
            kasumi_provider(true),
        ];
        let request = PlanRequest {
            path: "/system/bin/example".into(),
            requirements: Requirements {
                redirect_file: true,
                writable_forward: true,
                mountless: true,
                ..Requirements::default()
            },
            preferred_backend: None,
        };

        assert!(matches!(
            plan_requests(&providers, std::slice::from_ref(&request), PlanPolicy::default()),
            Err(PlanError::NoProvider { .. })
        ));

        let plan = plan_requests(
            &providers,
            &[request],
            PlanPolicy {
                allow_experimental: true,
                allow_reference_only: false,
            },
        )
        .unwrap();
        assert_eq!(plan[0].backend, BackendKind::Kasumi);
    }
}
