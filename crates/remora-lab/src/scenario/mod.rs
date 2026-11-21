//! YAML scenario definitions and deterministic execution engine.

pub mod model;
pub mod runner;

pub use model::{
    ActorKind, ActorSpec, Expect, FakeCloudSpec, HttpOverrideSpec, Invariant, ParallelBranch,
    Scenario, Step, StepAction,
};
pub use runner::{ObservedStep, ScenarioResult, run_scenario};
