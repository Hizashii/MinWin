//! Persistent state: what MinWin measured, what it changed, and how to undo
//! it.
//!
//! SQLite, under `%LOCALAPPDATA%\MinWin\state.db`. Never in the source tree.
//!
//! # What is stored
//!
//! Benchmark runs and their raw samples; apply sessions and, per change, the
//! state before, the state planned, the rollback data, the state after and the
//! verification result; rollback sessions and their outcomes. Each session also
//! records the MinWin version and the Windows build it ran against, because
//! state written on one build should not be replayed blindly onto another.
//!
//! # What is never stored
//!
//! No passwords, tokens or credential material. No browser data. No user file
//! paths or file contents. No process names or command lines. No environment
//! variables. MinWin stores what is needed to understand its own changes and
//! its own measurements, and nothing else.

pub mod db;
pub mod migrations;
pub mod models;

pub use db::{ApplySessionHeader, Database, PendingChange};
pub use models::{
    ApplySessionRecord, ChangeRecord, ChangeStatus, RollbackStatus, SessionCounts, SessionStatus,
};
