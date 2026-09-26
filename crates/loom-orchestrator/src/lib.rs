#![forbid(unsafe_code)]

pub mod capabilities;
pub mod inventory;
pub mod magic;
pub mod nomount;
pub mod overlay;
pub mod planner;
pub mod probe;
pub mod providers;
pub mod runtime;
pub mod transaction;

pub use capabilities::{BackendKind, CapabilitySet, ProviderStability, Requirements};
pub use inventory::{ProviderInventory, RuntimeSignals};
pub use magic::{CommandMagicRuntime, MagicAdapter, MagicError, MagicSpec};
pub use nomount::{NomountClient, NomountError, NomountRule, SystemNomountTransport};
pub use overlay::{CommandOverlayRuntime, OverlayAdapter, OverlayError, OverlaySpec};
pub use planner::{plan_requests, PlanEntry, PlanError, PlanPolicy, PlanRequest};
pub use probe::RuntimeProbe;
pub use providers::{
    kasumi_provider, magic_provider, nomount_provider, overlay_provider, ProviderDescriptor,
};
pub use runtime::{RuntimeAction, RuntimeCoordinator, RuntimeError, RuntimeReport};
pub use transaction::{execute_atomic, ExecutionError, ExecutionReport, Executor};
