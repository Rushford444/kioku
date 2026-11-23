//! Offline, opt-in D1 observation. This module never changes a plan.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;

use serde::Serialize;

use super::{D1Proposal, TestId};

/// Schema of the separate observation journal, not the selection journal.
pub const SCHEMA: &str = "kioku/d1-observe/v1";

/// Offline proposal source. Implementations must not use a model or network.
pub trait Observer {
    /// Produce a proposal; an error leaves the observed selection unchanged.
    ///
    /// # Errors
    /// Returns [`ObserverError`] when the local observer cannot supply its proposal.
    fn propose(&self) -> Result<D1Proposal, ObserverError>;
}

/// Proposal unavailable; no diagnostic data is persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObserverError;

/// Deterministic offline observer with a caller-supplied proposal.
#[derive(Debug, Clone, Default)]
pub struct Stub {
    /// Fixed proposal returned on every call.
    pub proposal: D1Proposal,
}

impl Observer for Stub {
    fn propose(&self) -> Result<D1Proposal, ObserverError> {
        Ok(self.proposal.clone())
    }
}

#[derive(Serialize)]
struct Event<'a> {
    schema: &'static str,
    timestamp: &'a str,
    action: &'static str,
    test: &'a TestId,
}

/// Append observations and return the hypothetical union, never a modified plan.
///
/// Only additions not already selected generate `would-add` events. Every distinct
/// proposed skip generates `skip-refused`, even when the test was not selected.
/// The timestamp is supplied by the caller. No journal is opened without events.
/// Absence or failure of the observer returns the original selection. I/O errors
/// are returned; callers must retain their original selection on error.
///
/// # Errors
/// Returns an error if the journal cannot be serialized, opened or appended. A
/// failed write may leave a partial final line; callers must treat it as incomplete.
pub fn observe(
    selected: &BTreeSet<TestId>,
    observer: Option<&dyn Observer>,
    timestamp: &str,
    journal: &Path,
) -> io::Result<BTreeSet<TestId>> {
    let Some(proposal) = observer.and_then(|source| source.propose().ok()) else {
        return Ok(selected.clone());
    };
    let additions: BTreeSet<_> = proposal.add.into_iter().collect();
    let skips: BTreeSet<_> = proposal.skip.into_iter().collect();
    let mut bytes = Vec::new();
    for (action, tests) in [
        (
            "would-add",
            additions.difference(selected).collect::<Vec<_>>(),
        ),
        ("skip-refused", skips.iter().collect()),
    ] {
        for test in tests {
            serde_json::to_writer(
                &mut bytes,
                &Event {
                    schema: SCHEMA,
                    timestamp,
                    action,
                    test,
                },
            )?;
            bytes.push(b'\n');
        }
    }
    if !bytes.is_empty() {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(journal)?
            .write_all(&bytes)?;
    }
    Ok(selected.union(&additions).cloned().collect())
}
