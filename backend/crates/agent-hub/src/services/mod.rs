//! services/: one Rust file per Python module (PORTING.md module map names the owner of each). A function a workstream
//! has not ported yet returns `Err(Problem::not_ported("services.<module>"))` (HTTP 501); its PUBLIC SIGNATURE is the
//! locked contract.

// CORE
pub mod audit;
pub mod auth;
pub mod hub;
pub mod store;

// WS-B1 (content store, organize, import)
pub mod attest;
pub mod content;
pub mod importer;
pub mod inbox;
pub mod organize;
pub mod resolver;
pub mod suggest;

// WS-B2 (delivery: git, plan, merge, apply, rollback) and WS-F (policy floor, targets, content guard, doctor)
pub mod applier;
pub mod applylog;
pub mod contentguard;
pub mod difflib;
pub mod doctor;
pub mod floor;
pub mod fs;
pub mod gitrepo;
pub mod lock;
pub mod merge;
pub mod planner;
pub mod rollback;
pub mod safefs;
pub mod targets;

// WS-C (evaluation, observation, knowledge proxy)
pub mod knowledge;
pub mod observe;
pub mod skilleval;
