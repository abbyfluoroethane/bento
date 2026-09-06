//! What a runner remembers so that an old controller cannot drive it
//! (MULTI-NODE 11.3).
//!
//! This defends against crashes, restarts, and bugs, not against an
//! attacker. Section 11.1 gives the trust model: the network decides who
//! may connect. What it cannot decide is *which* controller process is
//! the current one. Two controllers, or one controller that was replaced
//! while a request was in flight, would corrupt state by accident just as
//! thoroughly as on purpose.
//!
//! The memory must survive a process restart and a host reboot, so it is
//! SQLite on the runner's own disk. It is small on purpose: the highest
//! accepted epoch, and the outcome of each request that changed anything.

use std::time::Duration;

use time::OffsetDateTime;

use crate::protocol::{Envelope, PROTOCOL_VERSION, Refusal};

/// How far apart the two clocks may be before a runner stops judging
/// lease expiry. Beyond this it fails closed (MULTI-NODE 11.3).
pub const DEFAULT_MAX_SKEW: Duration = Duration::from_secs(60);

/// The durable half of the fence.
///
/// A fake is enough to test admission; the SQLite implementation is what
/// survives a reboot.
pub trait FenceStore: Send + Sync {
    /// The highest controller epoch this runner has accepted, or 0.
    fn accepted_epoch(&self) -> Result<i64, Error>;
    /// Records an accepted epoch. Never lowers the stored value: a
    /// controller cannot talk a runner backwards (MULTI-NODE 11.4).
    fn accept_epoch(&self, epoch: i64) -> Result<(), Error>;
    /// The recorded outcome of a request, if it already finished.
    fn outcome(&self, request_id: &str) -> Result<Option<String>, Error>;
    /// Records what a request did, so a retry answers the same way.
    fn record_outcome(&self, request_id: &str, outcome: &str) -> Result<(), Error>;
    /// The highest generation accepted for one object, and the digest it
    /// was accepted with.
    fn object_generation(&self, object_id: &str) -> Result<Option<(i64, String)>, Error>;
    /// Records a generation for one object. Never lowers it.
    fn accept_object(&self, object_id: &str, generation: i64, digest: &str) -> Result<(), Error>;
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("runner fence: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("runner fence: {0}")]
    Time(String),
}

/// Decides whether a request may act, and remembers what it decided.
pub struct Fence {
    machine_id: String,
    store: Box<dyn FenceStore>,
    max_skew: Duration,
    now: Box<dyn Fn() -> OffsetDateTime + Send + Sync>,
}

impl Fence {
    pub fn new(machine_id: impl Into<String>, store: Box<dyn FenceStore>) -> Self {
        Fence {
            machine_id: machine_id.into(),
            store,
            max_skew: DEFAULT_MAX_SKEW,
            now: Box::new(OffsetDateTime::now_utc),
        }
    }

    pub fn with_clock(mut self, now: impl Fn() -> OffsetDateTime + Send + Sync + 'static) -> Self {
        self.now = Box::new(now);
        self
    }

    pub fn with_max_skew(mut self, max_skew: Duration) -> Self {
        self.max_skew = max_skew;
        self
    }

    /// Decides whether `envelope` may act.
    ///
    /// The order of these checks is the design, not a detail. Each one
    /// answers a question the next one depends on:
    ///
    /// 1. Can this runner read the request at all?
    /// 2. Is the request even addressed to this machine?
    /// 3. Is the sender a controller that has already been replaced?
    /// 4. Does the sender still hold the lease it claims?
    ///
    /// A read-only operation stops here. It never records the epoch,
    /// which is what lets a controller with a restored database read
    /// fencing state before it is allowed to change anything
    /// (MULTI-NODE 11.4).
    pub fn admit(&self, envelope: &Envelope) -> Result<Admission, Error> {
        self.admit_as(envelope, envelope.op.mutates())
    }

    /// [`Fence::admit`] with the mutation decision supplied.
    ///
    /// The caller never chooses this; `admit` derives it from the
    /// operation. It is separate so the mutation rules can be tested
    /// before an operation that mutates exists, and so the rules stay one
    /// piece of code when one does.
    pub(crate) fn admit_as(&self, envelope: &Envelope, mutates: bool) -> Result<Admission, Error> {
        if envelope.protocol_version != PROTOCOL_VERSION {
            return Ok(Admission::Refused(Refusal::ProtocolVersion {
                yours: envelope.protocol_version,
                theirs: PROTOCOL_VERSION,
            }));
        }
        match &envelope.target_machine_id {
            Some(target) if *target != self.machine_id => {
                return Ok(Admission::Refused(Refusal::WrongMachine {
                    yours: target.clone(),
                    theirs: self.machine_id.clone(),
                }));
            }
            // Unaddressed. A read is answered, because that is how the
            // controller learns which machine this is. A change is
            // refused, so nothing is altered on a machine the controller
            // could not name (MULTI-NODE 11.2).
            None if mutates => {
                return Ok(Admission::Refused(Refusal::UnaddressedChange));
            }
            _ => {}
        }

        let accepted = self.store.accepted_epoch()?;
        if envelope.epoch < accepted {
            return Ok(Admission::Refused(Refusal::StaleEpoch {
                yours: envelope.epoch,
                theirs: accepted,
            }));
        }

        let now = (self.now)();
        // The lease is judged against this runner's clock, so the runner
        // first asks whether that clock can be trusted. It compares its
        // own time with the time the controller says it sent the request.
        // This catches a clock that is wrong in either direction: a
        // runner running fast would otherwise read every live lease as
        // expired and report the wrong reason (MULTI-NODE 11.3).
        let skew = (now - envelope.sent_at).whole_seconds();
        if skew.unsigned_abs() > self.max_skew.as_secs() {
            return Ok(Admission::Refused(Refusal::ClockUntrusted {
                skew_seconds: skew,
            }));
        }
        if envelope.lease_expires_at <= now {
            // RFC 3339, as every other time on the wire is. The default
            // `to_string` is for people, and this value is read by both.
            let at = envelope
                .lease_expires_at
                .format(&time::format_description::well_known::Rfc3339)
                .map_err(|error| Error::Time(error.to_string()))?;
            return Ok(Admission::Refused(Refusal::LeaseExpired { at }));
        }

        if !mutates {
            return Ok(Admission::Allowed);
        }

        // A change must say what it changes, so the runner can place it
        // in that object's history (MULTI-NODE 11.3).
        let Some(object) = &envelope.object else {
            return Ok(Admission::Refused(Refusal::UnfencedChange));
        };
        if let Some((accepted, digest)) = self.store.object_generation(&object.object_id)? {
            if object.generation < accepted {
                // A late order from a controller that has been overtaken.
                return Ok(Admission::Refused(Refusal::StaleGeneration {
                    object_id: object.object_id.clone(),
                    yours: object.generation,
                    theirs: accepted,
                }));
            }
            // The same generation must want the same thing. Two orders
            // that disagree at one generation mean one of them is wrong,
            // and the runner cannot tell which, so it takes neither.
            if object.generation == accepted && object.digest != digest {
                return Ok(Admission::Refused(Refusal::GenerationConflict {
                    object_id: object.object_id.clone(),
                    generation: object.generation,
                }));
            }
        }

        // A retry of a request that already finished answers with what it
        // answered before, rather than doing the work twice.
        if let Some(outcome) = self.store.outcome(&envelope.request_id)? {
            return Ok(Admission::AlreadyDone(outcome));
        }
        // Only a mutation moves the epoch. After this, every older
        // controller is refused, including one with a request in flight.
        self.store.accept_epoch(envelope.epoch)?;
        self.store
            .accept_object(&object.object_id, object.generation, &object.digest)?;
        Ok(Admission::Allowed)
    }

    /// Records what a mutation did, so its retry is answered from memory.
    pub fn finish(&self, request_id: &str, outcome: &str) -> Result<(), Error> {
        self.store.record_outcome(request_id, outcome)
    }

    pub fn accepted_epoch(&self) -> Result<i64, Error> {
        self.store.accepted_epoch()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    Allowed,
    /// This exact request already ran. The recorded answer is returned
    /// rather than the work being done a second time.
    AlreadyDone(String),
    Refused(Refusal),
}
