#![forbid(unsafe_code)]

pub mod capabilities;
pub mod inventory;
pub mod planner;
pub mod providers;
pub mod transaction;

pub use capabilities::{BackendKind, CapabilitySet, ProviderStability, Requirements};
pub use inventory::{ProviderInventory, RuntimeSignals};
pub use planner::{plan_requests, PlanEntry, PlanError, PlanPolicy, PlanRequest};
pub use providers::{kasumi_provider, nomount_provider, overlay_provider, ProviderDescriptor};
pub use transaction::{execute_atomic, ExecutionError, ExecutionReport, Executor};
