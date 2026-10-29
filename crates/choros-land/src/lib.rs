//! The landing queue engine: see `docs/landing-queue.md`.

pub mod admin;
pub mod agent;
pub mod auto_resolve;
pub mod branch;
pub mod context;
pub mod diffbase;
pub mod engine;
pub mod forge;
pub mod git;
pub mod graph;
pub mod instant;
pub mod land;
pub mod queue;
pub mod repos;
pub mod state;
pub mod status;
pub mod uuid;
pub mod workspace;

#[cfg(test)]
mod tests;
