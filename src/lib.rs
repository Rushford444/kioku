//! Kioku executes commands conservatively and optionally reuses successful exit status.
//!
//! Runtime execution is the default. Traced reuse requires explicit caller consent to
//! stable inputs and deterministic behavior. It never replays output or artifacts;
//! requested AST mode uses bytes. Unsupported observations do not authorize reuse.
//!
//! [`key::Snapshot`] and its capture options remain available as legacy fingerprint
//! utilities. They are not the runtime trace policy or a guarantee that every
//! input a command can read has been captured.
//!
//! # Example
//!
//! ```
//! use kioku::key::{CaptureOptions, Snapshot};
//! use kioku::Mode;
//!
//! let options = CaptureOptions {
//!     cwd: std::env::current_dir().unwrap_or_default(),
//!     argv: vec!["cargo".to_string(), "test".to_string()],
//!     mode: Mode::Bytes,
//!     inputs: Vec::new(),
//!     env: Vec::new(),
//! };
//! match Snapshot::capture(&options) {
//!     Ok(snapshot) => println!("key: {}", snapshot.key),
//!     Err(error) => println!("no snapshot: {error}"),
//! }
//! ```

#![forbid(unsafe_code)]

pub mod config;
pub mod fingerprint;
pub mod key;
pub mod policy;
pub mod runner;
pub mod store;
pub mod tools;
pub mod trace;

pub use fingerprint::Mode;
pub use key::{CaptureOptions, Snapshot};

use std::path::PathBuf;

/// Everything that can go wrong while kioku looks at a repository.
///
/// The variants are deliberately explicit: the caller decides what an
/// uncertain state means. A capture that fails is never a cache hit.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A filesystem operation failed.
    #[error("i/o error on {path}: {source}")]
    Io {
        /// Path of the failed operation.
        path: PathBuf,
        /// Underlying operating system error.
        #[source]
        source: std::io::Error,
    },
    /// `git` could not be run, or it failed.
    #[error("git {command} failed: {stderr}")]
    Git {
        /// Arguments of the `git` invocation that failed (without the `git` prefix).
        command: String,
        /// What `git` reported.
        stderr: String,
    },
    /// The `git` executable is not available.
    #[error("git is unavailable: {source}")]
    GitUnavailable {
        /// Underlying operating system error.
        #[source]
        source: std::io::Error,
    },
    /// The directory is not inside a git repository.
    #[error("not a git repository: {path}")]
    NotARepository {
        /// The directory that was inspected.
        path: PathBuf,
    },
    /// The working directory is outside the repository root.
    #[error("working directory {cwd} is outside the repository root {root}")]
    OutsideRepository {
        /// The working directory.
        cwd: PathBuf,
        /// The root of the repository.
        root: PathBuf,
    },
    /// A path cannot be represented without ambiguity.
    #[error("path is not valid UTF-8: {path:?}")]
    NonUtf8Path {
        /// The offending path.
        path: String,
    },
    /// An environment variable value cannot be represented without ambiguity.
    #[error("environment variable {name} is not valid UTF-8")]
    NonUtf8Env {
        /// The offending variable name.
        name: String,
    },
    /// An input could not be read.
    #[error("cannot read input {path}: {source}")]
    UnreadableInput {
        /// The input that could not be read.
        path: String,
        /// Underlying operating system error.
        #[source]
        source: std::io::Error,
    },
    /// An input changed while it was being read.
    #[error("input {path} changed while it was read")]
    RacyInput {
        /// The input that changed.
        path: String,
    },
    /// A symlink points at something that cannot be captured.
    #[error("symlink {path} points at a target that cannot be captured")]
    UnsupportedSymlink {
        /// The symlink.
        path: String,
    },
    /// The fingerprint mode is not supported by the key.
    #[error("unsupported fingerprint mode")]
    UnsupportedMode,
    /// The cache store lives in the work tree.
    ///
    /// The key leaves out the files of the store, so a tracked file that
    /// happens to live there would be left out of the key too, and a change of
    /// it would not change the key. The cache is off in that case.
    #[error("the cache store is inside the work tree: {path}")]
    StoreInsideWorkTree {
        /// Path of the store.
        path: String,
    },
    /// The version of a tool could not be established.
    ///
    /// A key that hides the version of a tool hides the changes of that tool,
    /// so the capture fails instead.
    #[error("cannot establish the version of {tool}: {reason}")]
    ToolProbe {
        /// The tool that could not be read.
        tool: String,
        /// Why the probe failed.
        reason: String,
    },
    /// An input glob is not a valid pattern.
    #[error("invalid input glob {pattern}: {message}")]
    InvalidGlob {
        /// The rejected pattern.
        pattern: String,
        /// Why the pattern was rejected.
        message: String,
    },
    /// The cache store failed.
    #[error(transparent)]
    Store(#[from] crate::store::Error),
}
