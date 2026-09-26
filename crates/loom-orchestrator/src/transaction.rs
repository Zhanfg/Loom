use core::fmt;

use crate::planner::PlanEntry;

pub trait Executor {
    type Token;

    fn apply(&mut self, entry: &PlanEntry) -> Result<Self::Token, String>;
    fn verify(&mut self, token: &Self::Token) -> Result<(), String>;
    fn rollback(&mut self, token: Self::Token) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionReport {
    pub applied: usize,
    pub verified: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionError {
    pub stage: &'static str,
    pub cause: String,
    pub rollback_failures: Vec<String>,
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed: {}", self.stage, self.cause)?;
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

impl std::error::Error for ExecutionError {}

fn rollback_all<E: Executor>(executor: &mut E, tokens: Vec<E::Token>) -> Vec<String> {
    let mut failures = Vec::new();
    for token in tokens.into_iter().rev() {
        if let Err(error) = executor.rollback(token) {
            failures.push(error);
        }
    }
    failures
}

pub fn execute_atomic<E: Executor>(
    executor: &mut E,
    plan: &[PlanEntry],
) -> Result<ExecutionReport, ExecutionError> {
    let mut tokens = Vec::with_capacity(plan.len());

    for entry in plan {
        match executor.apply(entry) {
            Ok(token) => tokens.push(token),
            Err(cause) => {
                let rollback_failures = rollback_all(executor, tokens);
                return Err(ExecutionError {
                    stage: "apply",
                    cause,
                    rollback_failures,
                });
            }
        }
    }

    for index in 0..tokens.len() {
        if let Err(cause) = executor.verify(&tokens[index]) {
            let rollback_failures = rollback_all(executor, tokens);
            return Err(ExecutionError {
                stage: "verify",
                cause,
                rollback_failures,
            });
        }
    }

    Ok(ExecutionReport {
        applied: tokens.len(),
        verified: tokens.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::BackendKind;

    #[derive(Default)]
    struct FakeExecutor {
        applied: Vec<String>,
        rolled_back: Vec<String>,
        fail_apply_on: Option<String>,
        fail_verify_on: Option<String>,
    }

    impl Executor for FakeExecutor {
        type Token = String;

        fn apply(&mut self, entry: &PlanEntry) -> Result<Self::Token, String> {
            if self.fail_apply_on.as_deref() == Some(entry.path.as_str()) {
                return Err(format!("apply {}", entry.path));
            }
            self.applied.push(entry.path.clone());
            Ok(entry.path.clone())
        }

        fn verify(&mut self, token: &Self::Token) -> Result<(), String> {
            if self.fail_verify_on.as_deref() == Some(token.as_str()) {
                return Err(format!("verify {token}"));
            }
            Ok(())
        }

        fn rollback(&mut self, token: Self::Token) -> Result<(), String> {
            self.rolled_back.push(token);
            Ok(())
        }
    }

    fn entry(path: &str) -> PlanEntry {
        PlanEntry {
            path: path.into(),
            provider: "overlayfs",
            backend: BackendKind::Overlay,
        }
    }

    #[test]
    fn apply_failure_rolls_back_the_whole_prefix() {
        let mut executor = FakeExecutor {
            fail_apply_on: Some("/b".into()),
            ..FakeExecutor::default()
        };
        let result = execute_atomic(&mut executor, &[entry("/a"), entry("/b"), entry("/c")]);

        assert!(result.is_err());
        assert_eq!(executor.applied, vec!["/a"]);
        assert_eq!(executor.rolled_back, vec!["/a"]);
    }

    #[test]
    fn verify_failure_rolls_back_the_entire_batch_in_reverse() {
        let mut executor = FakeExecutor {
            fail_verify_on: Some("/b".into()),
            ..FakeExecutor::default()
        };
        let result = execute_atomic(&mut executor, &[entry("/a"), entry("/b"), entry("/c")]);

        assert!(result.is_err());
        assert_eq!(executor.rolled_back, vec!["/c", "/b", "/a"]);
    }

    #[test]
    fn success_commits_without_partial_rollback() {
        let mut executor = FakeExecutor::default();
        let report = execute_atomic(&mut executor, &[entry("/a"), entry("/b")]).unwrap();

        assert_eq!(
            report,
            ExecutionReport {
                applied: 2,
                verified: 2,
            }
        );
        assert!(executor.rolled_back.is_empty());
    }
}
