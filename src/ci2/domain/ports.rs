//! Portable interfaces. I14 is the unique durable writer implementation.
use super::{
    primitives::{BlobRef, Digest, Id},
    schema::{
        ClientIdentity, Lease, LocalAccepted, LocalRequest, NativeObservation, ProcessIdentity,
        QueueItem, RunState, StopToken,
    },
};

pub trait LocalStoragePort {
    type Error;
    /// Validates and atomically adopts principal-scoped idempotence.
    /// # Errors
    /// Digest divergences and storage failures are preserved.
    fn accept_request(
        &mut self,
        identity: &ClientIdentity,
        request: &LocalRequest,
        request_digest: Digest,
    ) -> Result<LocalAccepted, Self::Error>;
    /// Advances a durable run and yields its next event cursor.
    /// # Errors
    /// Conflicting terminal state or an unwritable journal is rejected.
    fn start_run(&mut self, run_id: &Id, phase: RunState) -> Result<u64, Self::Error>;
    /// Atomically reserves complete resources for an allowed queued phase.
    /// # Errors
    /// Resource, epoch or durable transaction errors never open partial leases.
    fn enqueue(&mut self, item: &QueueItem) -> Result<Lease, Self::Error>;
    /// Records the supervised process identity for an exact acquired lease.
    /// # Errors
    /// Wrong lease/fence or report failure leaves its previous state intact.
    fn mark_started(&mut self, lease: &Lease, identity: ProcessIdentity)
    -> Result<(), Self::Error>;
    /// Writes one natively observed sequence without interpreting it as a verdict.
    /// # Errors
    /// Missing execution, duplicate sequence, or write fault is preserved.
    fn append_observation(
        &mut self,
        execution: &Id,
        sequence: u64,
        observation: &NativeObservation,
    ) -> Result<(), Self::Error>;
    /// Finalizes only a terminal report reference already durably written.
    /// # Errors
    /// Invalid verdict, absent blob or transaction fault leaves recovery enabled.
    fn finalize_run(
        &mut self,
        run_id: &Id,
        state: RunState,
        report: BlobRef,
    ) -> Result<(), Self::Error>;
    /// Reconciles an incident with fresh process evidence; never asserts a text sentence.
    /// # Errors
    /// Unknown/foreign process, incorrect owner, or lease fence rejects the action.
    fn reconcile_stopped(
        &mut self,
        lease: &Lease,
        evidence: &ProcessIdentity,
        still_running: bool,
    ) -> Result<(), Self::Error>;
}

pub trait RuntimePort {
    type Error;
    /// # Errors
    /// Start/cancel/drain/process signals leave a native outcome observable, not a cached verdict.
    fn execute(
        &mut self,
        item: &QueueItem,
        lease: &Lease,
        stop: &StopToken,
    ) -> Result<NativeObservation, Self::Error>;
}
#[derive(Debug, Clone, PartialEq)]
pub enum NativeCommandState {
    Available,
    Unsupported,
    Interrupted,
}
