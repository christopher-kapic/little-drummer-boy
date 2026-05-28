//! Concrete tool implementations.
//!
//! Every tool implements [`crate::engine::tool::Tool`] with
//! `Args = serde_json::Value` so the §12 repair layer can run between
//! rig's JSON-deserialized args and the typed dispatcher.
//!
//! Layout:
//!
//! - [`bash`] — process spawn, output capping, env scrub.
//! - [`read`] — snapshot read (no lock). Used by `orchestrator-build`
//!   for shallow inspection and by `coder` for non-mutating context
//!   reads.
//! - [`readlock`] — acquire-and-read (plan §4.1).
//! - [`writeunlock`] — write-and-release.
//! - [`unlock`] — release without write.
//! - [`editunlock`] — cascade-based search/replace (plan §13b).
//! - [`task`] — structural; the engine intercepts this name.

pub mod bash;
pub mod custom;
pub mod docs;
pub mod editunlock;
pub mod glob;
pub mod grep;
pub mod intel;
pub mod jobs;
pub mod question;
pub mod read;
pub mod readlock;
pub mod sandbox;
pub mod skill;
pub mod task;
pub mod unlock;
pub mod writeunlock;

pub mod common;
