//! Remora conformance lab: headless endpoint platform, YAML scenario runner, trace
//! normalization/diffing, JSON-RPC driver protocol, and compatibility matrix reporting.

pub mod diff;
pub mod driver;
pub mod error;
pub mod headless;
pub mod matrix;
pub mod normalize;
pub mod probe;
pub mod record;
pub mod redact;
pub mod replay;
pub mod scenario;
pub mod target;
