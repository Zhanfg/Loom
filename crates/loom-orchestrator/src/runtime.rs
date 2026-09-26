use core::fmt;

use crate::magic::{MagicAdapter, MagicRuntime, MagicSpec, MagicToken};
use crate::nomount::{AppliedRule, NomountClient, NomountRule, NomountTransport};
use crate::overlay::{OverlayAdapter, OverlayRuntime, OverlaySpec, OverlayToken};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeAction {
    Overlay(OverlaySpec),
    Magic(MagicSpec),
    NoMount(NomountRule),
}

impl RuntimeAction {
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::Overlay(spec) => spec.target.to_str().unwrap_or("<non-utf8-overlay-target>"),
            Self::Magic(spec) => spec.target.to_str().unwrap_or("<non-utf8-magic-target>"),
            Self::NoMount(rule) => &rule.virtual_path,
        }
    }
}

pub trait OverlayBackend {
    type Token;

    /// Applies one `OverlayFS` action.
    ///
    /// # Errors
    /// Returns a stringified backend error when apply fails.
    fn apply_overlay(&mut self, spec: &OverlaySpec) -> Result<Self::Token, String>;

    /// Verifies one applied `OverlayFS` action.
    ///
    /// # Errors
    /// Returns a stringified backend error when verification fails.
    fn verify_overlay(&mut self, token: &Self::Token) -> Result<(), String>;

    /// Rolls back one applied `OverlayFS` action.
    ///
    /// # Errors
    /// Returns a stringified backend error when rollback fails.
    fn rollback_overlay(&mut self, token: &Self::Token) -> Result<(), String>;
}

impl<R: OverlayRuntime> OverlayBackend for OverlayAdapter<R> {
    type Token = OverlayToken;

    fn apply_overlay(&mut self, spec: &OverlaySpec) -> Result<Self::Token, String> {
        self.apply(spec).map_err(|error| error.to_string())
    }

    fn verify_overlay(&mut self, token: &Self::Token) -> Result<(), String> {
        self.verify(token).map_err(|error| error.to_string())
    }

    fn rollback_overlay(&mut self, token: &Self::Token) -> Result<(), String> {
        self.rollback(token).map_err(|error| error.to_string())
    }
}

pub trait MagicBackend {
    type Token;

    /// Applies one Magic Mount action.
    ///
    /// # Errors
    /// Returns a stringified backend error when apply fails.
    fn apply_magic(&mut self, spec: &MagicSpec) -> Result<Self::Token, String>;

    /// Verifies one applied Magic Mount action.
    ///
    /// # Errors
    /// Returns a stringified backend error when verification fails.
    fn verify_magic(&mut self, token: &Self::Token) -> Result<(), String>;

    /// Rolls back one applied Magic Mount action.
    ///
    /// # Errors
    /// Returns a stringified backend error when rollback fails.
    fn rollback_magic(&mut self, token: &Self::Token) -> Result<(), String>;
}

impl<R: MagicRuntime> MagicBackend for MagicAdapter<R> {
    type Token = MagicToken;

    fn apply_magic(&mut self, spec: &MagicSpec) -> Result<Self::Token, String> {
        self.apply(spec).map_err(|error| error.to_string())
    }

    fn verify_magic(&mut self, token: &Self::Token) -> Result<(), String> {
        self.verify(token).map_err(|error| error.to_string())
    }

    fn rollback_magic(&mut self, token: &Self::Token) -> Result<(), String> {
        self.rollback(token).map_err(|error| error.to_string())
    }
}

pub trait NomountBackend {
    type Token;

    /// Applies one `NoMount` action.
    ///
    /// # Errors
    /// Returns a stringified backend error when apply fails.
    fn apply_nomount(&mut self, rule: &NomountRule) -> Result<Self::Token, String>;

    /// Verifies one applied `NoMount` action.
    ///
    /// # Errors
    /// Returns a stringified backend error when verification fails.
    fn verify_nomount(&mut self, token: &Self::Token) -> Result<(), String>;

    /// Rolls back one applied `NoMount` action.
    ///
    /// # Errors
    /// Returns a stringified backend error when rollback fails.
    fn rollback_nomount(&mut self, token: &Self::Token) -> Result<(), String>;
}

impl<T: NomountTransport> NomountBackend for NomountClient<T> {
    type Token = AppliedRule;

    fn apply_nomount(&mut self, rule: &NomountRule) -> Result<Self::Token, String> {
        self.apply(rule).map_err(|error| error.to_string())
    }

    fn verify_nomount(&mut self, token: &Self::Token) -> Result<(), String> {
        self.verify(token).map_err(|error| error.to_string())
    }

    fn rollback_nomount(&mut self, token: &Self::Token) -> Result<(), String> {
        self.rollback(token).map_err(|error| error.to_string())
    }
}

enum RuntimeToken<O, M, N> {
    Overlay(O),
    Magic(M),
    NoMount(N),
}

struct AppliedEntry<O, M, N> {
    path: String,
    token: RuntimeToken<O, M, N>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeReport {
    pub applied: usize,
    pub verified: usize,
    pub overlay_actions: usize,
    pub magic_actions: usize,
    pub nomount_actions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeError {
    pub stage: &'static str,
    pub path: String,
    pub cause: String,
    pub rollback_failures: Vec<String>,
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed for {}: {}", self.stage, self.path, self.cause)?;
        if !self.rollback_failures.is_empty() {
            write!(
                f,
                "; rollback failures: {}",
                self.rollback_failures.join("; ")
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for RuntimeError {}

pub struct RuntimeCoordinator<O, M, N> {
    overlay: O,
    magic: M,
    nomount: N,
}

impl<O, M, N> RuntimeCoordinator<O, M, N> {
    #[must_use]
    pub const fn new(overlay: O, magic: M, nomount: N) -> Self {
        Self {
            overlay,
            magic,
            nomount,
        }
    }

    #[must_use]
    pub fn into_inner(self) -> (O, M, N) {
        (self.overlay, self.magic, self.nomount)
    }
}

impl<O: OverlayBackend, M: MagicBackend, N: NomountBackend> RuntimeCoordinator<O, M, N> {
    /// Applies and verifies a mixed `OverlayFS`/Magic Mount/`NoMount` batch atomically.
    ///
    /// Every successful action yields a rollback token. Any later apply or verify
    /// failure rolls all previously applied actions back in reverse order.
    ///
    /// # Errors
    /// Returns `RuntimeError` on the first apply/verify failure, preserving any
    /// rollback failures in the same error.
    pub fn execute(&mut self, actions: &[RuntimeAction]) -> Result<RuntimeReport, RuntimeError> {
        let mut applied = Vec::with_capacity(actions.len());
        let mut overlay_actions = 0_usize;
        let mut magic_actions = 0_usize;
        let mut nomount_actions = 0_usize;

        for action in actions {
            let path = action.path().to_owned();
            let token = match action {
                RuntimeAction::Overlay(spec) => match self.overlay.apply_overlay(spec) {
                    Ok(token) => {
                        overlay_actions += 1;
                        RuntimeToken::Overlay(token)
                    }
                    Err(cause) => {
                        return Err(self.fail_with_rollback("apply", path, cause, &applied));
                    }
                },
                RuntimeAction::Magic(spec) => match self.magic.apply_magic(spec) {
                    Ok(token) => {
                        magic_actions += 1;
                        RuntimeToken::Magic(token)
                    }
                    Err(cause) => {
                        return Err(self.fail_with_rollback("apply", path, cause, &applied));
                    }
                },
                RuntimeAction::NoMount(rule) => match self.nomount.apply_nomount(rule) {
                    Ok(token) => {
                        nomount_actions += 1;
                        RuntimeToken::NoMount(token)
                    }
                    Err(cause) => {
                        return Err(self.fail_with_rollback("apply", path, cause, &applied));
                    }
                },
            };
            applied.push(AppliedEntry { path, token });
        }

        let mut verification_failure = None;
        for entry in &applied {
            let result = match &entry.token {
                RuntimeToken::Overlay(token) => self.overlay.verify_overlay(token),
                RuntimeToken::Magic(token) => self.magic.verify_magic(token),
                RuntimeToken::NoMount(token) => self.nomount.verify_nomount(token),
            };
            if let Err(cause) = result {
                verification_failure = Some((entry.path.clone(), cause));
                break;
            }
        }

        if let Some((path, cause)) = verification_failure {
            return Err(self.fail_with_rollback("verify", path, cause, &applied));
        }

        Ok(RuntimeReport {
            applied: applied.len(),
            verified: applied.len(),
            overlay_actions,
            magic_actions,
            nomount_actions,
        })
    }

    fn fail_with_rollback(
        &mut self,
        stage: &'static str,
        path: String,
        cause: String,
        applied: &[AppliedEntry<O::Token, M::Token, N::Token>],
    ) -> RuntimeError {
        let rollback_failures = self.rollback_all(applied);
        RuntimeError {
            stage,
            path,
            cause,
            rollback_failures,
        }
    }

    fn rollback_all(
        &mut self,
        applied: &[AppliedEntry<O::Token, M::Token, N::Token>],
    ) -> Vec<String> {
        let mut failures = Vec::new();
        for entry in applied.iter().rev() {
            let result = match &entry.token {
                RuntimeToken::Overlay(token) => self.overlay.rollback_overlay(token),
                RuntimeToken::Magic(token) => self.magic.rollback_magic(token),
                RuntimeToken::NoMount(token) => self.nomount.rollback_nomount(token),
            };
            if let Err(error) = result {
                failures.push(format!("{}: {error}", entry.path));
            }
        }
        failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[derive(Default)]
    struct FakeOverlay {
        applied: Vec<String>,
        rolled_back: Vec<String>,
        fail_verify: bool,
    }

    impl OverlayBackend for FakeOverlay {
        type Token = String;

        fn apply_overlay(&mut self, spec: &OverlaySpec) -> Result<Self::Token, String> {
            let path = spec.target.display().to_string();
            self.applied.push(path.clone());
            Ok(path)
        }

        fn verify_overlay(&mut self, _token: &Self::Token) -> Result<(), String> {
            if self.fail_verify {
                return Err("overlay verify failed".to_owned());
            }
            Ok(())
        }

        fn rollback_overlay(&mut self, token: &Self::Token) -> Result<(), String> {
            self.rolled_back.push(token.clone());
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeMagic {
        applied: Vec<String>,
        rolled_back: Vec<String>,
        fail_apply: bool,
    }

    impl MagicBackend for FakeMagic {
        type Token = String;

        fn apply_magic(&mut self, spec: &MagicSpec) -> Result<Self::Token, String> {
            if self.fail_apply {
                return Err("magic apply failed".to_owned());
            }
            let path = spec.target.display().to_string();
            self.applied.push(path.clone());
            Ok(path)
        }

        fn verify_magic(&mut self, _token: &Self::Token) -> Result<(), String> {
            Ok(())
        }

        fn rollback_magic(&mut self, token: &Self::Token) -> Result<(), String> {
            self.rolled_back.push(token.clone());
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeNoMount {
        applied: Vec<String>,
        rolled_back: Vec<String>,
        fail_apply: bool,
    }

    impl NomountBackend for FakeNoMount {
        type Token = String;

        fn apply_nomount(&mut self, rule: &NomountRule) -> Result<Self::Token, String> {
            if self.fail_apply {
                return Err("nomount apply failed".to_owned());
            }
            self.applied.push(rule.virtual_path.clone());
            Ok(rule.virtual_path.clone())
        }

        fn verify_nomount(&mut self, _token: &Self::Token) -> Result<(), String> {
            Ok(())
        }

        fn rollback_nomount(&mut self, token: &Self::Token) -> Result<(), String> {
            self.rolled_back.push(token.clone());
            Ok(())
        }
    }

    fn overlay_action(target: &str) -> RuntimeAction {
        RuntimeAction::Overlay(OverlaySpec {
            target: PathBuf::from(target),
            lowerdirs: vec![
                PathBuf::from("/data/adb/modules/demo/system"),
                PathBuf::from(target),
            ],
            upperdir: None,
            workdir: None,
            read_only: true,
        })
    }

    fn magic_action(target: &str) -> RuntimeAction {
        RuntimeAction::Magic(MagicSpec {
            source: PathBuf::from("/data/adb/modules/demo/system/etc/hosts"),
            target: PathBuf::from(target),
            read_only: true,
        })
    }

    #[test]
    fn later_nomount_apply_failure_rolls_back_overlay_prefix() {
        let nomount = FakeNoMount {
            fail_apply: true,
            ..FakeNoMount::default()
        };
        let mut runtime =
            RuntimeCoordinator::new(FakeOverlay::default(), FakeMagic::default(), nomount);
        let actions = [
            overlay_action("/system"),
            RuntimeAction::NoMount(
                NomountRule::redirect("/vendor/etc/a", "/data/adb/a", 0, false).unwrap(),
            ),
        ];

        assert!(runtime.execute(&actions).is_err());
        let (overlay, _, _) = runtime.into_inner();
        assert_eq!(overlay.rolled_back, vec!["/system"]);
    }

    #[test]
    fn magic_apply_failure_rolls_back_overlay_prefix() {
        let magic = FakeMagic {
            fail_apply: true,
            ..FakeMagic::default()
        };
        let mut runtime =
            RuntimeCoordinator::new(FakeOverlay::default(), magic, FakeNoMount::default());
        let actions = [overlay_action("/system"), magic_action("/vendor/etc/hosts")];

        assert!(runtime.execute(&actions).is_err());
        let (overlay, _, _) = runtime.into_inner();
        assert_eq!(overlay.rolled_back, vec!["/system"]);
    }

    #[test]
    fn overlay_verify_failure_rolls_back_all_backends_reverse_order() {
        let overlay = FakeOverlay {
            fail_verify: true,
            ..FakeOverlay::default()
        };
        let mut runtime =
            RuntimeCoordinator::new(overlay, FakeMagic::default(), FakeNoMount::default());
        let actions = [
            RuntimeAction::NoMount(
                NomountRule::redirect("/vendor/etc/a", "/data/adb/a", 0, false).unwrap(),
            ),
            magic_action("/product/etc/hosts"),
            overlay_action("/system"),
        ];

        assert!(runtime.execute(&actions).is_err());
        let (overlay, magic, nomount) = runtime.into_inner();
        assert_eq!(overlay.rolled_back, vec!["/system"]);
        assert_eq!(magic.rolled_back, vec!["/product/etc/hosts"]);
        assert_eq!(nomount.rolled_back, vec!["/vendor/etc/a"]);
    }

    #[test]
    fn successful_mixed_batch_reports_backend_counts() {
        let mut runtime = RuntimeCoordinator::new(
            FakeOverlay::default(),
            FakeMagic::default(),
            FakeNoMount::default(),
        );
        let actions = [
            overlay_action("/system"),
            magic_action("/product/etc/hosts"),
            RuntimeAction::NoMount(
                NomountRule::redirect("/vendor/etc/a", "/data/adb/a", 0, false).unwrap(),
            ),
        ];

        assert_eq!(
            runtime.execute(&actions).unwrap(),
            RuntimeReport {
                applied: 3,
                verified: 3,
                overlay_actions: 1,
                magic_actions: 1,
                nomount_actions: 1,
            }
        );
    }
}
