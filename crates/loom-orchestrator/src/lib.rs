#![forbid(unsafe_code)]

pub mod capabilities;
pub mod inventory;
pub mod planner;
pub mod providers;
pub mod transaction;

pub use capabilities::{BackendKind, CapabilitySet, ProviderStability, Requirements};
pub use inventory::{ProviderInventory, RuntimeSignals};
pub use planner::{PlanEntry, PlanError, PlanPolicy, PlanRequest, plan_requests};
pub use providers::{ProviderDescriptor, kasumi_provider, nomount_provider, overlay_provider};
pub use transaction::{ExecutionError, ExecutionReport, Executor, execute_atomic};
