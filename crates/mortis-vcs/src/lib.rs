//! # mortis-vcs
//!
//! Version-control backends implementing [`mortis_core::VcsBackend`].
//!
//! - [`git::GixBackend`] — pure-Rust Git via `gitoxide` (M2).
//! - SVN via an embedded `svn` CLI — added in M7.
//!
//! Each backend is a stateless strategy; the application layer routes a
//! [`RepoContext`](mortis_core::vcs::RepoContext) to the backend matching the
//! repository's [`VcsKind`](mortis_core::VcsKind).

pub mod filter;
pub mod git;
mod publish;
pub mod svn;
mod util;

pub use git::GixBackend;
pub use svn::{SvnCliBackend, SvnTool};
