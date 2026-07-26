//! Owns all generation-local transaction reservations and terminal correlation
//! memory. The registry is deliberately single-threaded and Sans-I/O: callers
//! serialize decisions, execute timers and completions elsewhere, and feed
//! exact tokens or write outcomes back into this state owner.

use std::collections::{HashMap, VecDeque};

use crate::hsms::{
    model::{
        ids::{ConnectionGeneration, Function, OperationId, SessionId, Stream, SystemBytes},
        runtime::TimerToken,
    },
    protocol::message::DataMessage,
    TimeoutKind,
};

use super::{
    allocator::{AllocationError, SystemBytesAllocator},
    matcher::{MatcherBuildError, MatcherDecision, MismatchField, ResponseMatcher},
};

/// Invalid bounded capacities supplied while constructing a registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegistryBuildError {
    /// W=1 request capacity must contain at least one entry.
    ZeroRequestCapacity,
    /// Terminal tombstone capacity must contain at least one entry.
    ZeroTombstoneCapacity,
}

/// Stable reason an all-or-nothing reservation was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReserveError {
    /// Generation shutdown has permanently closed this registry's admission.
    Closing,
    /// The supplied operation identity is already live in another reservation.
    DuplicateOperation {
        /// Operation identity that was already registered.
        operation_id: OperationId,
    },
    /// The bounded W=1 request registry is full.
    RequestCapacityExhausted {
        /// Configured maximum number of live W=1 requests.
        capacity: usize,
    },
    /// A transactional control request already owns the independent control slot.
    ControlSlotOccupied {
        /// Kind of control transaction currently occupying the slot.
        pending: ControlKind,
    },
    /// Request function was not an odd primary in the inclusive 1..=253 range.
    InvalidPrimaryFunction {
        /// Function value rejected before reservation commit.
        function: Function,
    },
    /// Every non-zero System Bytes value is live or retained as a tombstone.
    SystemBytesExhausted,
    /// Internal occupancy indexes disagreed while finding a free candidate.
    OccupancyInvariantViolation,
}

/// Unacknowledged outbound operation that needs a System Bytes lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OneWayKind {
    /// W=0 Data primary whose lease ends at its terminal write outcome.
    Data,
    /// `Separate.req`, which has no response and closes after its terminal write.
    Separate,
}

/// Transactional control requests supported by the independent control slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ControlKind {
    /// `Select.req` awaiting `Select.rsp`.
    Select,
    /// `Deselect.req` awaiting `Deselect.rsp`.
    Deselect,
    /// `Linktest.req` awaiting `Linktest.rsp`.
    Linktest,
}

/// Registry ownership class of one live or closing operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationClass {
    /// W=1 Data request with a compiled Secondary matcher.
    Request,
    /// Unacknowledged frame whose lease ends with its write.
    OneWay(OneWayKind),
    /// Transactional control request using the independent control slot.
    Control(ControlKind),
}

/// Strongest evidence currently held about peer visibility of an outbound frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationVisibility {
    /// No event has indicated that any frame byte could be peer-visible.
    NotVisible,
    /// At least one byte may be visible, but complete delivery is not proven.
    MayBeVisible,
    /// The complete frame reached the local ordered writer commit point.
    Committed,
}

impl OperationVisibility {
    /// Returns whether a close or failure must retain correlation memory.
    pub(crate) const fn needs_tombstone(self) -> bool {
        !matches!(self, Self::NotVisible)
    }
}

/// Terminal reason retained with an occupied System Bytes tombstone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TombstoneCategory {
    /// The expected F+1 Secondary completed the request successfully.
    ResponseMatched,
    /// An exact same-transaction SxF0 aborted the request.
    AbortReceived,
    /// The exact committed request exceeded T3.
    T3Expired,
    /// A supposedly not-written operation had previously become possibly visible.
    NotWrittenAfterVisibility,
    /// A terminal write could have delivered a partial or complete frame.
    DeliveryIndeterminate,
    /// Generation close removed an operation already committed or possibly visible.
    ClosedAfterVisibility,
    /// A W=0 Data or `Separate.req` frame committed locally.
    OneWayCommitted,
    /// The exact kind and System Bytes control response completed its request.
    ControlResponseMatched,
    /// The exact committed control request exceeded T6.
    ControlExpired,
}

/// Whether a frame matching terminal correlation memory is repeated or late.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TombstoneArrival {
    /// The same terminal response form was observed more than once.
    Duplicate,
    /// A valid correlated response arrived after another terminal outcome.
    Late,
}

/// Successful atomic reservation of one W=1 Data request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReservedRequest {
    /// Core operation that owns the request through its sole terminal decision.
    operation_id: OperationId,
    /// Non-zero locally allocated transaction correlation value.
    system_bytes: SystemBytes,
}

impl ReservedRequest {
    /// Returns the operation identity associated with this request.
    pub(crate) const fn operation_id(self) -> OperationId {
        self.operation_id
    }

    /// Returns the locally allocated System Bytes used to build its header.
    pub(crate) const fn system_bytes(self) -> SystemBytes {
        self.system_bytes
    }
}

/// Successful atomic reservation of one unacknowledged outbound frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReservedOneWay {
    /// Core operation that owns the lease through its terminal write outcome.
    operation_id: OperationId,
    /// Non-zero locally allocated System Bytes retained until that outcome.
    system_bytes: SystemBytes,
    /// Data or `Separate.req` semantics carried by the lease.
    kind: OneWayKind,
}

impl ReservedOneWay {
    /// Returns the operation identity associated with this lease.
    pub(crate) const fn operation_id(self) -> OperationId {
        self.operation_id
    }

    /// Returns the locally allocated System Bytes used to build its header.
    pub(crate) const fn system_bytes(self) -> SystemBytes {
        self.system_bytes
    }

    /// Returns whether the lease belongs to W=0 Data or `Separate.req`.
    pub(crate) const fn kind(self) -> OneWayKind {
        self.kind
    }
}

/// Successful atomic reservation of the independent control transaction slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReservedControl {
    /// Core operation that owns the control slot.
    operation_id: OperationId,
    /// Non-zero locally allocated System Bytes used by the control request.
    system_bytes: SystemBytes,
    /// Typed control request that the eventual response must match.
    kind: ControlKind,
}

impl ReservedControl {
    /// Returns the operation identity associated with this control request.
    pub(crate) const fn operation_id(self) -> OperationId {
        self.operation_id
    }

    /// Returns the locally allocated System Bytes used to build its header.
    pub(crate) const fn system_bytes(self) -> SystemBytes {
        self.system_bytes
    }

    /// Returns the response kind required to consume the control slot.
    pub(crate) const fn kind(self) -> ControlKind {
        self.kind
    }
}

/// Result of recording that an operation's write may be peer-visible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MarkVisibleDecision {
    /// Visibility advanced from definitely invisible to possibly visible.
    Marked {
        /// Class of operation whose visibility advanced.
        class: OperationClass,
    },
    /// Existing visibility was already at least as strong.
    Unchanged {
        /// Class of operation that received the duplicate notification.
        class: OperationClass,
        /// Previously retained visibility evidence.
        visibility: OperationVisibility,
    },
    /// The operation already reached a tombstoned terminal state.
    AlreadyTerminal {
        /// Retained terminal category that prevented further mutation.
        category: TombstoneCategory,
    },
    /// No live or retained operation uses the supplied identity.
    UnknownOperation,
}

/// Result of explicitly committing a request/control write and associating its timer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitDecision {
    /// The exact timer was attached and must now be armed by Core.
    ArmTimer {
        /// Live operation whose committed write starts the timer.
        operation_id: OperationId,
        /// System Bytes retained by the pending transaction.
        system_bytes: SystemBytes,
        /// Request or control class that selects T3 or T6.
        class: OperationClass,
        /// Exact token Core must pass to TimerDriver and later expiry handling.
        token: TimerToken,
    },
    /// The write was already committed and has an associated timer.
    AlreadyCommitted {
        /// Exact timer already owned by the operation.
        token: TimerToken,
    },
    /// The operation already reached a tombstoned terminal state.
    AlreadyTerminal {
        /// Retained terminal category that prevented timer arming.
        category: TombstoneCategory,
    },
    /// The operation class has no reply timer and must use its finish method.
    WrongOperationKind {
        /// Actual live class encountered for the operation.
        actual: OperationClass,
    },
    /// The supplied token used a timeout kind inappropriate for the operation.
    WrongTimerKind {
        /// Timer kind required by the live operation class.
        expected: TimeoutKind,
        /// Timer kind carried by the rejected token.
        actual: TimeoutKind,
    },
    /// The exact token is already associated with another live operation.
    TimerTokenInUse {
        /// Conflicting token rejected without mutating either operation.
        token: TimerToken,
    },
    /// No live or retained operation uses the supplied identity.
    UnknownOperation,
}

/// Result of a writer terminal outcome applied to registry ownership.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FinishDecision {
    /// The live operation was removed exactly once.
    Finished {
        /// Removed operation identity.
        operation_id: OperationId,
        /// Released System Bytes value.
        system_bytes: SystemBytes,
        /// Removed operation's registry class.
        class: OperationClass,
        /// Strongest visibility evidence at removal.
        visibility: OperationVisibility,
        /// Exact pending timer Core must cancel, if one existed.
        cancel_timer: Option<TimerToken>,
        /// Terminal memory created for this outcome, if visibility required it.
        tombstone: Option<TombstoneCategory>,
    },
    /// The operation already reached a tombstoned terminal state.
    AlreadyTerminal {
        /// Retained terminal category that prevented duplicate completion.
        category: TombstoneCategory,
    },
    /// A one-way commit was applied to a different live operation class.
    WrongOperationKind {
        /// Actual class that rejected the one-way-only operation.
        actual: OperationClass,
    },
    /// No live or retained operation uses the supplied identity.
    UnknownOperation,
}

/// Result of exact T3 or T6 expiry processing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExpiryDecision {
    /// The token exactly removed its still-live pending transaction.
    Expired {
        /// Operation completed by the timeout.
        operation_id: OperationId,
        /// System Bytes retained as late-response correlation memory.
        system_bytes: SystemBytes,
        /// Request or control class completed by the timeout.
        class: OperationClass,
        /// Visibility evidence retained at expiry.
        visibility: OperationVisibility,
    },
    /// Token used the wrong timeout kind for the requested expiry method.
    WrongTimerKind {
        /// Timer kind required by the expiry method.
        expected: TimeoutKind,
        /// Timer kind carried by the rejected token.
        actual: TimeoutKind,
    },
    /// Token was stale, duplicated, never armed, or belonged to another class.
    Stale,
}

/// Live or terminal owner whose same System Bytes rejected an even Data frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CollisionSource {
    /// Candidate failed a field of a still-live request matcher.
    LiveRequest {
        /// First matcher field that rejected the candidate.
        field: MismatchField,
    },
    /// Candidate collided with an unacknowledged live frame lease.
    LiveOneWay {
        /// One-way operation retaining the colliding System Bytes.
        kind: OneWayKind,
    },
    /// Candidate collided with a live transactional control request.
    LiveControl {
        /// Control operation retaining the colliding System Bytes.
        kind: ControlKind,
    },
    /// Candidate failed a field of a retained request matcher.
    RequestTombstone {
        /// Terminal category retained by the colliding request tombstone.
        category: TombstoneCategory,
        /// First matcher field that rejected the candidate.
        field: MismatchField,
    },
    /// Candidate used System Bytes retained by a non-request tombstone.
    Tombstone {
        /// Terminal category retained by the colliding tombstone.
        category: TombstoneCategory,
    },
}

/// Complete classification of one inbound semantic Data message.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum InboundDataDecision {
    /// Odd-function Data is a Primary candidate regardless of System Bytes collisions.
    PrimaryCandidate {
        /// Original semantic message passed on for Core state validation.
        message: DataMessage,
    },
    /// Exact F+1/W=false response completed a live W=1 request.
    MatchedSecondary {
        /// Operation completed by the matched response.
        operation_id: OperationId,
        /// Validated semantic response retained for command completion.
        message: DataMessage,
        /// Exact T3 registration Core must cancel, if it had been armed.
        cancel_t3: Option<TimerToken>,
        /// Request visibility when the response won the serialized race.
        visibility: OperationVisibility,
    },
    /// Exact W=false, header-only F0 for the same transaction aborted a request.
    Aborted {
        /// Operation completed by the abort.
        operation_id: OperationId,
        /// Original F0 semantic message retained for protocol diagnostics.
        message: DataMessage,
        /// Exact T3 registration Core must cancel, if it had been armed.
        cancel_t3: Option<TimerToken>,
        /// Request visibility when the abort won the serialized race.
        visibility: OperationVisibility,
    },
    /// Exact response or abort matched retained terminal correlation memory.
    Tombstoned {
        /// Original operation associated with the tombstone.
        operation_id: OperationId,
        /// Original semantic message classified as duplicate or late.
        message: DataMessage,
        /// Terminal category that retained this correlation identity.
        category: TombstoneCategory,
        /// Whether this repeats the same terminal form or arrived after another end.
        arrival: TombstoneArrival,
    },
    /// Even-function candidate collided with a live or terminal non-match.
    Mismatch {
        /// Original semantic message left unconsumed for a Core diagnostic.
        message: DataMessage,
        /// Registry owner that rejected the candidate without mutation.
        collision: CollisionSource,
    },
    /// Even-function/F0 Data had no matching live request or exact tombstone.
    OrphanSecondary {
        /// Original orphaned message retained for a Core diagnostic.
        message: DataMessage,
    },
}

/// Owner that rejected an inbound control response without being consumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ControlCollision {
    /// Another live control transaction owns the single slot.
    Live {
        /// Live control kind that did not match the response.
        kind: ControlKind,
        /// System Bytes expected by the live transaction.
        system_bytes: SystemBytes,
    },
    /// A tombstone with the same System Bytes retained a different control kind.
    Tombstone {
        /// Control kind retained by the tombstone.
        kind: ControlKind,
        /// Terminal category retained by the tombstone.
        category: TombstoneCategory,
    },
    /// A Data or one-way tombstone retained the response's System Bytes.
    OtherTombstone {
        /// Terminal category retained by the non-control tombstone.
        category: TombstoneCategory,
    },
}

/// Result of matching and taking a typed control response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ControlTakeDecision {
    /// Kind and System Bytes exactly consumed the live control slot.
    Matched {
        /// Operation completed by the response.
        operation_id: OperationId,
        /// System Bytes released into terminal correlation memory.
        system_bytes: SystemBytes,
        /// Exact control kind that matched.
        kind: ControlKind,
        /// Exact T6 registration Core must cancel, if it had been armed.
        cancel_t6: Option<TimerToken>,
        /// Visibility state when the response won the serialized race.
        visibility: OperationVisibility,
    },
    /// Exact kind and System Bytes matched a terminal control tombstone.
    Tombstoned {
        /// Original operation associated with the tombstone.
        operation_id: OperationId,
        /// Terminal category that retained this correlation identity.
        category: TombstoneCategory,
        /// Whether the response duplicates success or arrived after another end.
        arrival: TombstoneArrival,
    },
    /// A live or terminal owner rejected the response without mutation.
    Mismatch {
        /// Owner that prevented an exact control match.
        collision: ControlCollision,
    },
    /// No live control or same-System-Bytes tombstone exists.
    NoPending,
}

/// One operation drained by the first generation-close transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CloseOperation {
    /// Live operation removed by close.
    operation_id: OperationId,
    /// System Bytes released or retained as a tombstone.
    system_bytes: SystemBytes,
    /// Registry class of the removed operation.
    class: OperationClass,
    /// Exact timer Core must cancel, if one was armed.
    cancel_timer: Option<TimerToken>,
    /// Strongest peer-visibility evidence held at close.
    visibility: OperationVisibility,
    /// Terminal memory created because the frame was visible or committed.
    tombstone: Option<TombstoneCategory>,
}

impl CloseOperation {
    /// Returns the operation identity removed by close.
    pub(crate) const fn operation_id(self) -> OperationId {
        self.operation_id
    }

    /// Returns the operation's System Bytes.
    pub(crate) const fn system_bytes(self) -> SystemBytes {
        self.system_bytes
    }

    /// Returns the removed request, one-way, or control class.
    pub(crate) const fn class(self) -> OperationClass {
        self.class
    }

    /// Returns the exact timer that Core must cancel, if present.
    pub(crate) const fn cancel_timer(self) -> Option<TimerToken> {
        self.cancel_timer
    }

    /// Returns the strongest visibility evidence held at close.
    pub(crate) const fn visibility(self) -> OperationVisibility {
        self.visibility
    }

    /// Returns the close tombstone category, or `None` if never visible.
    pub(crate) const fn tombstone(self) -> Option<TombstoneCategory> {
        self.tombstone
    }
}

/// Idempotent result of beginning registry close.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CloseDecision {
    /// Whether this call performed the one-time open-to-closing transition.
    began_close: bool,
    /// Every live request, one-way lease, and control slot removed on first close.
    operations: Vec<CloseOperation>,
}

impl CloseDecision {
    /// Returns whether this invocation performed the close transition.
    pub(crate) const fn began_close(&self) -> bool {
        self.began_close
    }

    /// Borrows the deterministic operation-id-ordered close dispositions.
    pub(crate) fn operations(&self) -> &[CloseOperation] {
        &self.operations
    }

    /// Consumes the decision and returns its close dispositions.
    pub(crate) fn into_operations(self) -> Vec<CloseOperation> {
        self.operations
    }
}

/// Live W=1 request state indexed by System Bytes.
#[derive(Clone, Copy, Debug)]
struct RequestEntry {
    /// Operation owning this request.
    operation_id: OperationId,
    /// Complete response contract compiled before scheduling.
    matcher: ResponseMatcher,
    /// Strongest writer visibility evidence.
    visibility: OperationVisibility,
    /// Exact T3 token attached only after local commit.
    timer: Option<TimerToken>,
}

/// Live W=0 Data or `Separate.req` lease indexed by System Bytes.
#[derive(Clone, Copy, Debug)]
struct OneWayEntry {
    /// Operation owning this lease.
    operation_id: OperationId,
    /// Locally allocated correlation value.
    system_bytes: SystemBytes,
    /// Data or `Separate.req` lease semantics.
    kind: OneWayKind,
    /// Strongest writer visibility evidence.
    visibility: OperationVisibility,
}

/// Live transactional control state held in the independent single slot.
#[derive(Clone, Copy, Debug)]
struct ControlEntry {
    /// Operation owning this slot.
    operation_id: OperationId,
    /// Locally allocated correlation value.
    system_bytes: SystemBytes,
    /// Exact response kind required to consume the slot.
    kind: ControlKind,
    /// Strongest writer visibility evidence.
    visibility: OperationVisibility,
    /// Exact T6 token attached only after local commit.
    timer: Option<TimerToken>,
}

/// Location of one operation in the registry's disjoint live stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OperationLocator {
    /// W=1 request indexed by these System Bytes.
    Request(SystemBytes),
    /// One-way lease indexed by these System Bytes and kind.
    OneWay(SystemBytes, OneWayKind),
    /// The independent control slot of this kind and System Bytes.
    Control(SystemBytes, ControlKind),
}

impl OperationLocator {
    /// Returns the public registry class represented by this locator.
    const fn class(self) -> OperationClass {
        match self {
            Self::Request(_) => OperationClass::Request,
            Self::OneWay(_, kind) => OperationClass::OneWay(kind),
            Self::Control(_, kind) => OperationClass::Control(kind),
        }
    }
}

/// Live owner of one exact timer token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TimerOwner {
    /// T3 registration owned by a W=1 request.
    Request {
        /// Request operation identity.
        operation_id: OperationId,
        /// Request System Bytes.
        system_bytes: SystemBytes,
    },
    /// T6 registration owned by the control slot.
    Control {
        /// Control operation identity.
        operation_id: OperationId,
        /// Control System Bytes.
        system_bytes: SystemBytes,
    },
}

/// Match semantics retained by a terminal System Bytes tombstone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TombstoneSubject {
    /// Response/abort matcher retained for a completed W=1 request.
    Request(ResponseMatcher),
    /// Allocation-only memory for an unacknowledged outbound frame.
    OneWay(OneWayKind),
    /// Exact typed control response retained after control termination.
    Control(ControlKind),
}

/// Bounded terminal correlation record indexed by System Bytes.
#[derive(Clone, Copy, Debug)]
struct Tombstone {
    /// Original operation that reached terminal state.
    operation_id: OperationId,
    /// Retained System Bytes excluded from local reuse.
    system_bytes: SystemBytes,
    /// Request, one-way, or control matching semantics.
    subject: TombstoneSubject,
    /// Stable terminal category used to classify later arrivals.
    category: TombstoneCategory,
}

/// Fully removed live operation used to construct pure terminal decisions.
#[derive(Clone, Copy, Debug)]
struct RemovedOperation {
    /// Removed operation identity.
    operation_id: OperationId,
    /// Released System Bytes.
    system_bytes: SystemBytes,
    /// Removed operation class.
    class: OperationClass,
    /// Strongest visibility evidence at removal.
    visibility: OperationVisibility,
    /// Exact timer removed from the timer index, if present.
    timer: Option<TimerToken>,
    /// Request matcher retained only for W=1 operations.
    matcher: Option<ResponseMatcher>,
}

impl RemovedOperation {
    /// Converts this removed operation into its tombstone matching subject.
    fn tombstone_subject(self) -> TombstoneSubject {
        match self.class {
            OperationClass::Request => TombstoneSubject::Request(
                self.matcher
                    .expect("removed request must retain its response matcher"),
            ),
            OperationClass::OneWay(kind) => TombstoneSubject::OneWay(kind),
            OperationClass::Control(kind) => TombstoneSubject::Control(kind),
        }
    }
}

/// Generation-local owner of System Bytes, pending operations, timers, and tombstones.
#[derive(Debug)]
pub(crate) struct TransactionRegistry {
    /// TCP incarnation whose operations this registry owns.
    generation: ConnectionGeneration,
    /// Cursor used only through non-mutating discovery plus explicit commit.
    allocator: SystemBytesAllocator,
    /// Maximum number of simultaneously live W=1 requests.
    request_capacity: usize,
    /// Maximum number of independently retained terminal identities.
    tombstone_capacity: usize,
    /// Permanent admission fence set by the first close transition.
    closing: bool,
    /// Live W=1 requests keyed by System Bytes.
    requests: HashMap<SystemBytes, RequestEntry>,
    /// Live W=0/Separate leases keyed by System Bytes.
    one_way: HashMap<SystemBytes, OneWayEntry>,
    /// Independent single transactional control slot.
    control: Option<ControlEntry>,
    /// Every live operation's disjoint storage location.
    operations: HashMap<OperationId, OperationLocator>,
    /// Exact T3/T6 token ownership used to reject stale or duplicate expiry.
    timers: HashMap<TimerToken, TimerOwner>,
    /// FIFO order of retained terminal System Bytes.
    tombstone_order: VecDeque<SystemBytes>,
    /// Terminal correlation memory keyed by occupied System Bytes.
    tombstones: HashMap<SystemBytes, Tombstone>,
}

impl TransactionRegistry {
    /// Creates one generation-scoped registry with independent request and tombstone bounds.
    ///
    /// `request_capacity` limits only W=1 Data requests. One-way leases use no
    /// request slot, transactional control has one independent slot, and
    /// `tombstone_capacity` bounds a separate FIFO. Zero capacities return a
    /// structured construction error.
    pub(crate) fn new(
        generation: ConnectionGeneration,
        request_capacity: usize,
        tombstone_capacity: usize,
    ) -> Result<Self, RegistryBuildError> {
        if request_capacity == 0 {
            return Err(RegistryBuildError::ZeroRequestCapacity);
        }
        if tombstone_capacity == 0 {
            return Err(RegistryBuildError::ZeroTombstoneCapacity);
        }

        Ok(Self {
            generation,
            allocator: SystemBytesAllocator::new(),
            request_capacity,
            tombstone_capacity,
            closing: false,
            requests: HashMap::with_capacity(request_capacity),
            one_way: HashMap::new(),
            control: None,
            operations: HashMap::new(),
            timers: HashMap::new(),
            tombstone_order: VecDeque::with_capacity(tombstone_capacity),
            tombstones: HashMap::with_capacity(tombstone_capacity),
        })
    }

    /// Returns the TCP generation whose protocol state this registry owns.
    pub(crate) const fn generation(&self) -> ConnectionGeneration {
        self.generation
    }

    /// Returns whether the first close transition has fenced new reservations.
    pub(crate) const fn is_closing(&self) -> bool {
        self.closing
    }

    /// Returns the number of live W=1 request slots currently occupied.
    pub(crate) fn request_len(&self) -> usize {
        self.requests.len()
    }

    /// Returns the number of live one-way leases currently held.
    pub(crate) fn one_way_len(&self) -> usize {
        self.one_way.len()
    }

    /// Returns whether the independent transactional control slot is occupied.
    pub(crate) const fn has_control(&self) -> bool {
        self.control.is_some()
    }

    /// Returns the number of terminal System Bytes retained in the FIFO.
    pub(crate) fn tombstone_len(&self) -> usize {
        self.tombstones.len()
    }

    /// Atomically validates, allocates, compiles, and registers a W=1 request.
    ///
    /// `function` must be an odd primary within 1..=253. Failure leaves every
    /// live index, tombstone, and allocator cursor unchanged.
    pub(crate) fn reserve_request(
        &mut self,
        operation_id: OperationId,
        session_id: SessionId,
        stream: Stream,
        function: Function,
    ) -> Result<ReservedRequest, ReserveError> {
        self.ensure_reservation_allowed(operation_id)?;
        if self.requests.len() >= self.request_capacity {
            return Err(ReserveError::RequestCapacityExhausted {
                capacity: self.request_capacity,
            });
        }

        let system_bytes = self.find_available_system_bytes()?;
        let matcher = ResponseMatcher::compile(system_bytes, session_id, stream, function)
            .map_err(Self::map_matcher_error)?;
        let entry = RequestEntry {
            operation_id,
            matcher,
            visibility: OperationVisibility::NotVisible,
            timer: None,
        };

        debug_assert!(!self.requests.contains_key(&system_bytes));
        debug_assert!(!self.operations.contains_key(&operation_id));
        self.requests.insert(system_bytes, entry);
        self.operations
            .insert(operation_id, OperationLocator::Request(system_bytes));
        self.allocator.commit(system_bytes);

        Ok(ReservedRequest {
            operation_id,
            system_bytes,
        })
    }

    /// Atomically allocates and registers a W=0 Data or `Separate.req` lease.
    ///
    /// One-way reservations do not consume W=1 request capacity. Their System
    /// Bytes remain occupied until a terminal write method removes the lease.
    pub(crate) fn reserve_one_way(
        &mut self,
        operation_id: OperationId,
        kind: OneWayKind,
    ) -> Result<ReservedOneWay, ReserveError> {
        self.ensure_reservation_allowed(operation_id)?;
        let system_bytes = self.find_available_system_bytes()?;
        let entry = OneWayEntry {
            operation_id,
            system_bytes,
            kind,
            visibility: OperationVisibility::NotVisible,
        };

        debug_assert!(!self.one_way.contains_key(&system_bytes));
        debug_assert!(!self.operations.contains_key(&operation_id));
        self.one_way.insert(system_bytes, entry);
        self.operations
            .insert(operation_id, OperationLocator::OneWay(system_bytes, kind));
        self.allocator.commit(system_bytes);

        Ok(ReservedOneWay {
            operation_id,
            system_bytes,
            kind,
        })
    }

    /// Atomically reserves the independent transactional control slot.
    ///
    /// A full W=1 Data registry does not block this slot. A second concurrent
    /// control request is rejected without advancing System Bytes.
    pub(crate) fn reserve_control(
        &mut self,
        operation_id: OperationId,
        kind: ControlKind,
    ) -> Result<ReservedControl, ReserveError> {
        self.ensure_reservation_allowed(operation_id)?;
        if let Some(pending) = self.control {
            return Err(ReserveError::ControlSlotOccupied {
                pending: pending.kind,
            });
        }

        let system_bytes = self.find_available_system_bytes()?;
        let entry = ControlEntry {
            operation_id,
            system_bytes,
            kind,
            visibility: OperationVisibility::NotVisible,
            timer: None,
        };

        debug_assert!(!self.operations.contains_key(&operation_id));
        self.control = Some(entry);
        self.operations
            .insert(operation_id, OperationLocator::Control(system_bytes, kind));
        self.allocator.commit(system_bytes);

        Ok(ReservedControl {
            operation_id,
            system_bytes,
            kind,
        })
    }

    /// Records that at least one byte of a live operation may be peer-visible.
    ///
    /// Terminal, duplicate, and unknown notifications never recreate state.
    pub(crate) fn mark_visible(&mut self, operation_id: OperationId) -> MarkVisibleDecision {
        let Some(locator) = self.operations.get(&operation_id).copied() else {
            return self
                .terminal_category_for_operation(operation_id)
                .map_or(MarkVisibleDecision::UnknownOperation, |category| {
                    MarkVisibleDecision::AlreadyTerminal { category }
                });
        };

        let visibility = self.visibility(locator);
        if visibility != OperationVisibility::NotVisible {
            return MarkVisibleDecision::Unchanged {
                class: locator.class(),
                visibility,
            };
        }

        self.set_visibility(locator, OperationVisibility::MayBeVisible);
        MarkVisibleDecision::Marked {
            class: locator.class(),
        }
    }

    /// Marks a W=1 request or transactional control write committed and attaches its timer.
    ///
    /// Request operations require a T3 token and control operations require a
    /// T6 token. The association is atomic: wrong kinds, token collisions,
    /// fast-response terminal state, and duplicate commit notifications do not
    /// arm or replace a timer.
    pub(crate) fn mark_committed(
        &mut self,
        operation_id: OperationId,
        token: TimerToken,
    ) -> CommitDecision {
        let Some(locator) = self.operations.get(&operation_id).copied() else {
            return self
                .terminal_category_for_operation(operation_id)
                .map_or(CommitDecision::UnknownOperation, |category| {
                    CommitDecision::AlreadyTerminal { category }
                });
        };

        let expected = match locator {
            OperationLocator::Request(_) => TimeoutKind::T3,
            OperationLocator::Control(_, _) => TimeoutKind::T6,
            OperationLocator::OneWay(_, _) => {
                return CommitDecision::WrongOperationKind {
                    actual: locator.class(),
                };
            }
        };
        if let Some(existing) = self.timer(locator) {
            return CommitDecision::AlreadyCommitted { token: existing };
        }
        if token.kind() != expected {
            return CommitDecision::WrongTimerKind {
                expected,
                actual: token.kind(),
            };
        }
        if self.timers.contains_key(&token) {
            return CommitDecision::TimerTokenInUse { token };
        }

        self.set_visibility(locator, OperationVisibility::Committed);
        self.set_timer(locator, Some(token));
        let system_bytes = Self::locator_system_bytes(locator);
        let owner = match locator {
            OperationLocator::Request(_) => TimerOwner::Request {
                operation_id,
                system_bytes,
            },
            OperationLocator::Control(_, _) => TimerOwner::Control {
                operation_id,
                system_bytes,
            },
            OperationLocator::OneWay(_, _) => unreachable!("one-way rejected above"),
        };
        self.timers.insert(token, owner);

        CommitDecision::ArmTimer {
            operation_id,
            system_bytes,
            class: locator.class(),
            token,
        }
    }

    /// Applies a definitely-not-written terminal outcome to any live operation.
    ///
    /// An operation that never became visible is removed without a tombstone.
    /// Defensive handling of a previously visible/committed operation retains
    /// terminal correlation memory and returns its exact timer for cancellation.
    pub(crate) fn finish_not_written(&mut self, operation_id: OperationId) -> FinishDecision {
        let Some(removed) = self.remove_live_operation(operation_id) else {
            return self.finished_absent_decision(operation_id);
        };
        let category = if removed.visibility.needs_tombstone() {
            Some(TombstoneCategory::NotWrittenAfterVisibility)
        } else {
            None
        };
        if let Some(category) = category {
            self.insert_tombstone(removed, category);
        }
        Self::finish_decision(removed, category)
    }

    /// Applies an indeterminate terminal write outcome to any live operation.
    ///
    /// Indeterminate delivery always advances visibility to at least
    /// `MayBeVisible`, removes an exact timer if present, and creates a
    /// tombstone for late-response correlation.
    pub(crate) fn finish_indeterminate(&mut self, operation_id: OperationId) -> FinishDecision {
        let Some(locator) = self.operations.get(&operation_id).copied() else {
            return self.finished_absent_decision(operation_id);
        };
        if self.visibility(locator) == OperationVisibility::NotVisible {
            self.set_visibility(locator, OperationVisibility::MayBeVisible);
        }
        let removed = self
            .remove_live_operation(operation_id)
            .expect("operation located immediately before removal");
        let category = TombstoneCategory::DeliveryIndeterminate;
        self.insert_tombstone(removed, category);
        Self::finish_decision(removed, Some(category))
    }

    /// Completes a one-way lease after its full frame commits locally.
    ///
    /// Only W=0 Data and `Separate.req` operations use this method. The
    /// committed System Bytes becomes a tombstone and cannot be reused until
    /// FIFO eviction.
    pub(crate) fn finish_one_way(&mut self, operation_id: OperationId) -> FinishDecision {
        let Some(locator) = self.operations.get(&operation_id).copied() else {
            return self.finished_absent_decision(operation_id);
        };
        if !matches!(locator, OperationLocator::OneWay(_, _)) {
            return FinishDecision::WrongOperationKind {
                actual: locator.class(),
            };
        }

        self.set_visibility(locator, OperationVisibility::Committed);
        let removed = self
            .remove_live_operation(operation_id)
            .expect("one-way operation located immediately before removal");
        let category = TombstoneCategory::OneWayCommitted;
        self.insert_tombstone(removed, category);
        Self::finish_decision(removed, Some(category))
    }

    /// Classifies one inbound Data message and atomically consumes exact live matches.
    ///
    /// Odd functions are always Primary candidates. Even functions and F0 can
    /// only become a matched Secondary, exact abort, tombstone hit, mismatch,
    /// or orphan; they are never promoted to Primary. Same-System-Bytes
    /// mismatches preserve every live and terminal index unchanged.
    pub(crate) fn classify_inbound(&mut self, message: DataMessage) -> InboundDataDecision {
        let header = message.header();
        let has_message_text = message.body().is_some();
        if header.function().get() % 2 == 1 {
            return InboundDataDecision::PrimaryCandidate { message };
        }

        let system_bytes = header.system_bytes();
        if let Some(request) = self.requests.get(&system_bytes).copied() {
            return match request.matcher.classify(header, has_message_text) {
                MatcherDecision::Secondary => {
                    let removed = self
                        .remove_live_operation(request.operation_id)
                        .expect("request found by System Bytes must be removable");
                    let category = TombstoneCategory::ResponseMatched;
                    self.insert_tombstone(removed, category);
                    InboundDataDecision::MatchedSecondary {
                        operation_id: removed.operation_id,
                        message,
                        cancel_t3: removed.timer,
                        visibility: removed.visibility,
                    }
                }
                MatcherDecision::Abort => {
                    let removed = self
                        .remove_live_operation(request.operation_id)
                        .expect("request found by System Bytes must be removable");
                    let category = TombstoneCategory::AbortReceived;
                    self.insert_tombstone(removed, category);
                    InboundDataDecision::Aborted {
                        operation_id: removed.operation_id,
                        message,
                        cancel_t3: removed.timer,
                        visibility: removed.visibility,
                    }
                }
                MatcherDecision::Mismatch { field } => InboundDataDecision::Mismatch {
                    message,
                    collision: CollisionSource::LiveRequest { field },
                },
            };
        }

        if let Some(tombstone) = self.tombstones.get(&system_bytes).copied() {
            if let TombstoneSubject::Request(matcher) = tombstone.subject {
                let matched = matcher.classify(header, has_message_text);
                return match matched {
                    MatcherDecision::Secondary | MatcherDecision::Abort => {
                        InboundDataDecision::Tombstoned {
                            operation_id: tombstone.operation_id,
                            message,
                            category: tombstone.category,
                            arrival: Self::data_tombstone_arrival(tombstone.category, matched),
                        }
                    }
                    MatcherDecision::Mismatch { field } => InboundDataDecision::Mismatch {
                        message,
                        collision: CollisionSource::RequestTombstone {
                            category: tombstone.category,
                            field,
                        },
                    },
                };
            }
            return InboundDataDecision::Mismatch {
                message,
                collision: CollisionSource::Tombstone {
                    category: tombstone.category,
                },
            };
        }

        if let Some(one_way) = self.one_way.get(&system_bytes) {
            return InboundDataDecision::Mismatch {
                message,
                collision: CollisionSource::LiveOneWay { kind: one_way.kind },
            };
        }
        if let Some(control) = self
            .control
            .filter(|entry| entry.system_bytes == system_bytes)
        {
            return InboundDataDecision::Mismatch {
                message,
                collision: CollisionSource::LiveControl { kind: control.kind },
            };
        }

        InboundDataDecision::OrphanSecondary { message }
    }

    /// Matches an inbound typed control response by both kind and System Bytes.
    ///
    /// An exact match consumes the slot and creates a tombstone. Exact old
    /// tombstones are classified before reporting a mismatch against a newer
    /// live slot, so a late response cannot disturb the current transaction.
    pub(crate) fn take_control(
        &mut self,
        kind: ControlKind,
        system_bytes: SystemBytes,
    ) -> ControlTakeDecision {
        if self
            .control
            .is_some_and(|entry| entry.kind == kind && entry.system_bytes == system_bytes)
        {
            let operation_id = self
                .control
                .expect("exact control checked above")
                .operation_id;
            let removed = self
                .remove_live_operation(operation_id)
                .expect("live control slot must be removable");
            let category = TombstoneCategory::ControlResponseMatched;
            self.insert_tombstone(removed, category);
            return ControlTakeDecision::Matched {
                operation_id,
                system_bytes,
                kind,
                cancel_t6: removed.timer,
                visibility: removed.visibility,
            };
        }

        if let Some(tombstone) = self.tombstones.get(&system_bytes) {
            return match tombstone.subject {
                TombstoneSubject::Control(retained_kind) if retained_kind == kind => {
                    ControlTakeDecision::Tombstoned {
                        operation_id: tombstone.operation_id,
                        category: tombstone.category,
                        arrival: if tombstone.category == TombstoneCategory::ControlResponseMatched
                        {
                            TombstoneArrival::Duplicate
                        } else {
                            TombstoneArrival::Late
                        },
                    }
                }
                TombstoneSubject::Control(retained_kind) => ControlTakeDecision::Mismatch {
                    collision: ControlCollision::Tombstone {
                        kind: retained_kind,
                        category: tombstone.category,
                    },
                },
                TombstoneSubject::Request(_) | TombstoneSubject::OneWay(_) => {
                    ControlTakeDecision::Mismatch {
                        collision: ControlCollision::OtherTombstone {
                            category: tombstone.category,
                        },
                    }
                }
            };
        }

        if let Some(control) = self.control {
            return ControlTakeDecision::Mismatch {
                collision: ControlCollision::Live {
                    kind: control.kind,
                    system_bytes: control.system_bytes,
                },
            };
        }
        ControlTakeDecision::NoPending
    }

    /// Expires only the exact still-associated T3 token.
    ///
    /// Stale, duplicate, never-armed, response-won, and control-owned tokens
    /// return without mutating any index.
    pub(crate) fn expire_t3(&mut self, token: TimerToken) -> ExpiryDecision {
        if token.kind() != TimeoutKind::T3 {
            return ExpiryDecision::WrongTimerKind {
                expected: TimeoutKind::T3,
                actual: token.kind(),
            };
        }
        let Some(TimerOwner::Request {
            operation_id,
            system_bytes,
        }) = self.timers.get(&token).copied()
        else {
            return ExpiryDecision::Stale;
        };
        if !self
            .requests
            .get(&system_bytes)
            .is_some_and(|entry| entry.operation_id == operation_id && entry.timer == Some(token))
        {
            return ExpiryDecision::Stale;
        }

        let removed = self
            .remove_live_operation(operation_id)
            .expect("exact T3 owner must be removable");
        let category = TombstoneCategory::T3Expired;
        self.insert_tombstone(removed, category);
        ExpiryDecision::Expired {
            operation_id,
            system_bytes,
            class: OperationClass::Request,
            visibility: removed.visibility,
        }
    }

    /// Expires only the exact still-associated control T6 token.
    ///
    /// Stale, duplicate, never-armed, response-won, and request-owned tokens
    /// return without mutating any index.
    pub(crate) fn expire_control(&mut self, token: TimerToken) -> ExpiryDecision {
        if token.kind() != TimeoutKind::T6 {
            return ExpiryDecision::WrongTimerKind {
                expected: TimeoutKind::T6,
                actual: token.kind(),
            };
        }
        let Some(TimerOwner::Control {
            operation_id,
            system_bytes,
        }) = self.timers.get(&token).copied()
        else {
            return ExpiryDecision::Stale;
        };
        if !self.control.is_some_and(|entry| {
            entry.operation_id == operation_id
                && entry.system_bytes == system_bytes
                && entry.timer == Some(token)
        }) {
            return ExpiryDecision::Stale;
        }

        let removed = self
            .remove_live_operation(operation_id)
            .expect("exact T6 owner must be removable");
        let class = removed.class;
        let category = TombstoneCategory::ControlExpired;
        self.insert_tombstone(removed, category);
        ExpiryDecision::Expired {
            operation_id,
            system_bytes,
            class,
            visibility: removed.visibility,
        }
    }

    /// Permanently closes reservation admission and drains every live operation.
    ///
    /// The first call returns all request, one-way, and control operation/timer/
    /// visibility dispositions in ascending operation-id order. Visible or
    /// committed entries create tombstones; never-visible entries do not. Later
    /// calls are idempotent and return no operations.
    pub(crate) fn begin_close(&mut self) -> CloseDecision {
        if self.closing {
            return CloseDecision {
                began_close: false,
                operations: Vec::new(),
            };
        }
        self.closing = true;

        let mut operation_ids: Vec<_> = self.operations.keys().copied().collect();
        operation_ids.sort_unstable();
        let mut operations = Vec::with_capacity(operation_ids.len());
        for operation_id in operation_ids {
            let removed = self
                .remove_live_operation(operation_id)
                .expect("operation index snapshot must remain removable");
            let category = if removed.visibility.needs_tombstone() {
                Some(TombstoneCategory::ClosedAfterVisibility)
            } else {
                None
            };
            if let Some(category) = category {
                self.insert_tombstone(removed, category);
            }
            operations.push(CloseOperation {
                operation_id: removed.operation_id,
                system_bytes: removed.system_bytes,
                class: removed.class,
                cancel_timer: removed.timer,
                visibility: removed.visibility,
                tombstone: category,
            });
        }

        CloseDecision {
            began_close: true,
            operations,
        }
    }

    /// Rejects close or duplicate-operation reservations without changing state.
    fn ensure_reservation_allowed(&self, operation_id: OperationId) -> Result<(), ReserveError> {
        if self.closing {
            return Err(ReserveError::Closing);
        }
        if self.operations.contains_key(&operation_id) {
            return Err(ReserveError::DuplicateOperation { operation_id });
        }
        Ok(())
    }

    /// Finds a free value across every live class and the independent tombstone FIFO.
    fn find_available_system_bytes(&self) -> Result<SystemBytes, ReserveError> {
        let occupied_count = self
            .requests
            .len()
            .saturating_add(self.one_way.len())
            .saturating_add(usize::from(self.control.is_some()))
            .saturating_add(self.tombstones.len());
        let occupied_count =
            u64::try_from(occupied_count).map_err(|_| ReserveError::SystemBytesExhausted)?;

        self.allocator
            .find_available(occupied_count, |candidate| {
                self.is_system_bytes_occupied(candidate)
            })
            .map_err(|error| match error {
                AllocationError::Exhausted => ReserveError::SystemBytesExhausted,
                AllocationError::InconsistentOccupancy => ReserveError::OccupancyInvariantViolation,
            })
    }

    /// Returns whether any live class or tombstone owns `system_bytes`.
    fn is_system_bytes_occupied(&self, system_bytes: SystemBytes) -> bool {
        self.requests.contains_key(&system_bytes)
            || self.one_way.contains_key(&system_bytes)
            || self
                .control
                .is_some_and(|entry| entry.system_bytes == system_bytes)
            || self.tombstones.contains_key(&system_bytes)
    }

    /// Maps matcher construction failure into the registry's atomic reserve error.
    fn map_matcher_error(error: MatcherBuildError) -> ReserveError {
        match error {
            MatcherBuildError::InvalidPrimaryFunction { function } => {
                ReserveError::InvalidPrimaryFunction { function }
            }
        }
    }

    /// Returns a locator's System Bytes without reading its backing entry.
    const fn locator_system_bytes(locator: OperationLocator) -> SystemBytes {
        match locator {
            OperationLocator::Request(system_bytes)
            | OperationLocator::OneWay(system_bytes, _)
            | OperationLocator::Control(system_bytes, _) => system_bytes,
        }
    }

    /// Reads the visibility stored at a valid operation locator.
    fn visibility(&self, locator: OperationLocator) -> OperationVisibility {
        match locator {
            OperationLocator::Request(system_bytes) => {
                self.requests
                    .get(&system_bytes)
                    .expect("operation index must locate request")
                    .visibility
            }
            OperationLocator::OneWay(system_bytes, _) => {
                self.one_way
                    .get(&system_bytes)
                    .expect("operation index must locate one-way lease")
                    .visibility
            }
            OperationLocator::Control(system_bytes, _) => {
                let entry = self.control.expect("operation index must locate control");
                debug_assert_eq!(entry.system_bytes, system_bytes);
                entry.visibility
            }
        }
    }

    /// Replaces visibility at a valid operation locator.
    fn set_visibility(&mut self, locator: OperationLocator, visibility: OperationVisibility) {
        match locator {
            OperationLocator::Request(system_bytes) => {
                self.requests
                    .get_mut(&system_bytes)
                    .expect("operation index must locate request")
                    .visibility = visibility;
            }
            OperationLocator::OneWay(system_bytes, _) => {
                self.one_way
                    .get_mut(&system_bytes)
                    .expect("operation index must locate one-way lease")
                    .visibility = visibility;
            }
            OperationLocator::Control(system_bytes, _) => {
                let entry = self
                    .control
                    .as_mut()
                    .expect("operation index must locate control");
                debug_assert_eq!(entry.system_bytes, system_bytes);
                entry.visibility = visibility;
            }
        }
    }

    /// Reads the optional request/control timer stored at a valid locator.
    fn timer(&self, locator: OperationLocator) -> Option<TimerToken> {
        match locator {
            OperationLocator::Request(system_bytes) => {
                self.requests
                    .get(&system_bytes)
                    .expect("operation index must locate request")
                    .timer
            }
            OperationLocator::Control(system_bytes, _) => {
                let entry = self.control.expect("operation index must locate control");
                debug_assert_eq!(entry.system_bytes, system_bytes);
                entry.timer
            }
            OperationLocator::OneWay(_, _) => None,
        }
    }

    /// Replaces the optional timer at a valid request/control locator.
    fn set_timer(&mut self, locator: OperationLocator, timer: Option<TimerToken>) {
        match locator {
            OperationLocator::Request(system_bytes) => {
                self.requests
                    .get_mut(&system_bytes)
                    .expect("operation index must locate request")
                    .timer = timer;
            }
            OperationLocator::Control(system_bytes, _) => {
                let entry = self
                    .control
                    .as_mut()
                    .expect("operation index must locate control");
                debug_assert_eq!(entry.system_bytes, system_bytes);
                entry.timer = timer;
            }
            OperationLocator::OneWay(_, _) => {
                debug_assert!(timer.is_none(), "one-way leases cannot own timers");
            }
        }
    }

    /// Removes one exact live operation and all of its secondary indexes.
    fn remove_live_operation(&mut self, operation_id: OperationId) -> Option<RemovedOperation> {
        let locator = self.operations.remove(&operation_id)?;
        let removed = match locator {
            OperationLocator::Request(system_bytes) => {
                let entry = self
                    .requests
                    .remove(&system_bytes)
                    .expect("operation index must locate request");
                RemovedOperation {
                    operation_id: entry.operation_id,
                    system_bytes,
                    class: OperationClass::Request,
                    visibility: entry.visibility,
                    timer: entry.timer,
                    matcher: Some(entry.matcher),
                }
            }
            OperationLocator::OneWay(system_bytes, kind) => {
                let entry = self
                    .one_way
                    .remove(&system_bytes)
                    .expect("operation index must locate one-way lease");
                debug_assert_eq!(entry.kind, kind);
                RemovedOperation {
                    operation_id: entry.operation_id,
                    system_bytes: entry.system_bytes,
                    class: OperationClass::OneWay(entry.kind),
                    visibility: entry.visibility,
                    timer: None,
                    matcher: None,
                }
            }
            OperationLocator::Control(system_bytes, kind) => {
                let entry = self
                    .control
                    .take()
                    .expect("operation index must locate control");
                debug_assert_eq!(entry.system_bytes, system_bytes);
                debug_assert_eq!(entry.kind, kind);
                RemovedOperation {
                    operation_id: entry.operation_id,
                    system_bytes: entry.system_bytes,
                    class: OperationClass::Control(entry.kind),
                    visibility: entry.visibility,
                    timer: entry.timer,
                    matcher: None,
                }
            }
        };
        debug_assert_eq!(removed.operation_id, operation_id);
        if let Some(token) = removed.timer {
            let owner = self
                .timers
                .remove(&token)
                .expect("entry timer must have an exact timer index");
            debug_assert!(match owner {
                TimerOwner::Request {
                    operation_id: owner_operation,
                    system_bytes: owner_system_bytes,
                }
                | TimerOwner::Control {
                    operation_id: owner_operation,
                    system_bytes: owner_system_bytes,
                } => {
                    owner_operation == operation_id && owner_system_bytes == removed.system_bytes
                }
            });
        }
        Some(removed)
    }

    /// Pushes one terminal identity and evicts the oldest tombstone at capacity.
    fn insert_tombstone(&mut self, removed: RemovedOperation, category: TombstoneCategory) {
        debug_assert!(!self.tombstones.contains_key(&removed.system_bytes));
        if self.tombstone_order.len() == self.tombstone_capacity {
            let evicted_system_bytes = self
                .tombstone_order
                .pop_front()
                .expect("full tombstone FIFO must have an oldest entry");
            self.tombstones
                .remove(&evicted_system_bytes)
                .expect("FIFO and tombstone index must agree");
        }

        let tombstone = Tombstone {
            operation_id: removed.operation_id,
            system_bytes: removed.system_bytes,
            subject: removed.tombstone_subject(),
            category,
        };
        self.tombstone_order.push_back(removed.system_bytes);
        self.tombstones.insert(removed.system_bytes, tombstone);
    }

    /// Finds a retained terminal category for an operation identity.
    fn terminal_category_for_operation(
        &self,
        operation_id: OperationId,
    ) -> Option<TombstoneCategory> {
        self.tombstones
            .values()
            .find(|tombstone| tombstone.operation_id == operation_id)
            .map(|tombstone| tombstone.category)
    }

    /// Builds a duplicate-safe result when no live operation could be removed.
    fn finished_absent_decision(&self, operation_id: OperationId) -> FinishDecision {
        self.terminal_category_for_operation(operation_id)
            .map_or(FinishDecision::UnknownOperation, |category| {
                FinishDecision::AlreadyTerminal { category }
            })
    }

    /// Converts a removed operation and optional tombstone into its finish result.
    const fn finish_decision(
        removed: RemovedOperation,
        tombstone: Option<TombstoneCategory>,
    ) -> FinishDecision {
        FinishDecision::Finished {
            operation_id: removed.operation_id,
            system_bytes: removed.system_bytes,
            class: removed.class,
            visibility: removed.visibility,
            cancel_timer: removed.timer,
            tombstone,
        }
    }

    /// Classifies an exact request-tombstone Data arrival as duplicate or late.
    const fn data_tombstone_arrival(
        category: TombstoneCategory,
        matched: MatcherDecision,
    ) -> TombstoneArrival {
        match (category, matched) {
            (TombstoneCategory::ResponseMatched, MatcherDecision::Secondary)
            | (TombstoneCategory::AbortReceived, MatcherDecision::Abort) => {
                TombstoneArrival::Duplicate
            }
            _ => TombstoneArrival::Late,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Registry tests cover atomic capacity, all operation classes, response and
    //! timer races, write outcomes, FIFO terminal memory, closing, and indexes.

    use std::collections::HashSet;

    use crate::{
        hsms::{model::ids::TimerId, protocol::header::DataHeader},
        secs2::SecsItem,
    };

    use super::*;

    /// Creates a registry for generation seven with the supplied independent bounds.
    fn registry(request_capacity: usize, tombstone_capacity: usize) -> TransactionRegistry {
        TransactionRegistry::new(
            ConnectionGeneration::new(7),
            request_capacity,
            tombstone_capacity,
        )
        .expect("non-zero registry capacities")
    }

    /// Returns the shared valid Data Session ID used by response tests.
    fn session() -> SessionId {
        SessionId::new(3).expect("data session")
    }

    /// Returns the shared valid SECS stream used by response tests.
    fn stream() -> Stream {
        Stream::new(5).expect("stream")
    }

    /// Reserves a valid S5F1 request for `operation`.
    fn reserve_request(registry: &mut TransactionRegistry, operation: u64) -> ReservedRequest {
        registry
            .reserve_request(
                OperationId::new(operation),
                session(),
                stream(),
                Function::new(1),
            )
            .expect("request reservation")
    }

    /// Builds a semantic Data message from explicit header fields and no body.
    fn data_message(
        system_bytes: SystemBytes,
        session_id: u16,
        stream_value: u8,
        function: u8,
        reply_expected: bool,
    ) -> DataMessage {
        data_message_with_body(
            system_bytes,
            session_id,
            stream_value,
            function,
            reply_expected,
            None,
        )
    }

    /// Builds a semantic Data message from explicit header fields and optional body.
    fn data_message_with_body(
        system_bytes: SystemBytes,
        session_id: u16,
        stream_value: u8,
        function: u8,
        reply_expected: bool,
        body: Option<SecsItem>,
    ) -> DataMessage {
        DataMessage::new(
            DataHeader::new(
                SessionId::new(session_id).expect("data session"),
                Stream::new(stream_value).expect("stream"),
                Function::new(function),
                reply_expected,
                system_bytes,
            ),
            body,
        )
    }

    /// Builds an exact S5F2 response for a reserved S5F1 request.
    fn exact_response(reserved: ReservedRequest) -> DataMessage {
        data_message(reserved.system_bytes(), 3, 5, 2, false)
    }

    /// Builds an exact S5F0 abort for a reserved S5F1 request.
    fn exact_abort(reserved: ReservedRequest) -> DataMessage {
        data_message(reserved.system_bytes(), 3, 5, 0, false)
    }

    /// Creates a unique T3 token from a test-local numeric identity.
    fn t3(id: u64) -> TimerToken {
        TimerToken::new(TimerId::new(id), TimeoutKind::T3)
    }

    /// Creates a unique T6 token from a test-local numeric identity.
    fn t6(id: u64) -> TimerToken {
        TimerToken::new(TimerId::new(id), TimeoutKind::T6)
    }

    /// Asserts agreement between every primary store and its secondary indexes.
    fn assert_index_invariants(registry: &TransactionRegistry) {
        let expected_operations = registry.requests.len()
            + registry.one_way.len()
            + usize::from(registry.control.is_some());
        assert_eq!(registry.operations.len(), expected_operations);

        let mut occupied = HashSet::new();
        for (system_bytes, entry) in &registry.requests {
            assert!(occupied.insert(*system_bytes));
            assert_eq!(entry.matcher.system_bytes(), *system_bytes);
            assert_eq!(
                registry.operations.get(&entry.operation_id),
                Some(&OperationLocator::Request(*system_bytes))
            );
            if let Some(token) = entry.timer {
                assert_eq!(
                    registry.timers.get(&token),
                    Some(&TimerOwner::Request {
                        operation_id: entry.operation_id,
                        system_bytes: *system_bytes,
                    })
                );
            }
        }
        for (system_bytes, entry) in &registry.one_way {
            assert!(occupied.insert(*system_bytes));
            assert_eq!(entry.system_bytes, *system_bytes);
            assert_eq!(
                registry.operations.get(&entry.operation_id),
                Some(&OperationLocator::OneWay(*system_bytes, entry.kind))
            );
        }
        if let Some(entry) = registry.control {
            assert!(occupied.insert(entry.system_bytes));
            assert_eq!(
                registry.operations.get(&entry.operation_id),
                Some(&OperationLocator::Control(entry.system_bytes, entry.kind))
            );
            if let Some(token) = entry.timer {
                assert_eq!(
                    registry.timers.get(&token),
                    Some(&TimerOwner::Control {
                        operation_id: entry.operation_id,
                        system_bytes: entry.system_bytes,
                    })
                );
            }
        }
        for (token, owner) in &registry.timers {
            match owner {
                TimerOwner::Request {
                    operation_id,
                    system_bytes,
                } => {
                    let entry = registry
                        .requests
                        .get(system_bytes)
                        .expect("timer request owner");
                    assert_eq!(entry.operation_id, *operation_id);
                    assert_eq!(entry.timer, Some(*token));
                }
                TimerOwner::Control {
                    operation_id,
                    system_bytes,
                } => {
                    let entry = registry.control.expect("timer control owner");
                    assert_eq!(entry.operation_id, *operation_id);
                    assert_eq!(entry.system_bytes, *system_bytes);
                    assert_eq!(entry.timer, Some(*token));
                }
            }
        }

        assert_eq!(registry.tombstone_order.len(), registry.tombstones.len());
        let mut ordered_tombstones = HashSet::new();
        for system_bytes in &registry.tombstone_order {
            assert!(ordered_tombstones.insert(*system_bytes));
            let tombstone = registry
                .tombstones
                .get(system_bytes)
                .expect("FIFO tombstone index");
            assert_eq!(tombstone.system_bytes, *system_bytes);
            assert!(occupied.insert(*system_bytes));
        }
        assert_eq!(ordered_tombstones.len(), registry.tombstones.len());
    }

    /// Confirms both bounded capacities reject zero independently.
    #[test]
    fn construction_rejects_zero_capacities() {
        assert!(matches!(
            TransactionRegistry::new(ConnectionGeneration::new(1), 0, 1),
            Err(RegistryBuildError::ZeroRequestCapacity)
        ));
        assert!(matches!(
            TransactionRegistry::new(ConnectionGeneration::new(1), 1, 0),
            Err(RegistryBuildError::ZeroTombstoneCapacity)
        ));
    }

    /// Confirms invalid function and full-capacity failures do not advance allocation.
    #[test]
    fn request_reservation_failures_are_atomic() {
        let mut registry = registry(1, 4);

        assert_eq!(
            registry.reserve_request(OperationId::new(1), session(), stream(), Function::new(2),),
            Err(ReserveError::InvalidPrimaryFunction {
                function: Function::new(2),
            })
        );
        assert_eq!(registry.allocator.next_candidate().get(), 1);

        let first = reserve_request(&mut registry, 1);
        assert_eq!(first.system_bytes().get(), 1);
        assert_eq!(
            registry.reserve_request(OperationId::new(2), session(), stream(), Function::new(3),),
            Err(ReserveError::RequestCapacityExhausted { capacity: 1 })
        );
        assert_eq!(registry.allocator.next_candidate().get(), 2);

        let one_way = registry
            .reserve_one_way(OperationId::new(3), OneWayKind::Data)
            .expect("capacity failure did not consume System Bytes");
        assert_eq!(one_way.system_bytes().get(), 2);
        assert_index_invariants(&registry);
    }

    /// Confirms the allocator skips every live class and retained tombstones.
    #[test]
    fn allocation_skips_request_one_way_control_and_tombstone_owners() {
        let mut registry = registry(2, 4);
        let request = reserve_request(&mut registry, 1);
        let live_one_way = registry
            .reserve_one_way(OperationId::new(2), OneWayKind::Data)
            .expect("one-way");
        let control = registry
            .reserve_control(OperationId::new(3), ControlKind::Select)
            .expect("control");
        let completed_one_way = registry
            .reserve_one_way(OperationId::new(4), OneWayKind::Separate)
            .expect("second one-way");
        assert!(matches!(
            registry.finish_one_way(completed_one_way.operation_id()),
            FinishDecision::Finished { .. }
        ));

        registry.allocator = SystemBytesAllocator::with_next(1);
        let reserved = registry
            .reserve_one_way(OperationId::new(5), OneWayKind::Data)
            .expect("candidate after four occupied values");

        assert_eq!(request.system_bytes().get(), 1);
        assert_eq!(live_one_way.system_bytes().get(), 2);
        assert_eq!(control.system_bytes().get(), 3);
        assert_eq!(completed_one_way.system_bytes().get(), 4);
        assert_eq!(reserved.system_bytes().get(), 5);
        assert_index_invariants(&registry);
    }

    /// Confirms full W=1 Data capacity does not block one-way or control reservations.
    #[test]
    fn full_data_capacity_does_not_block_one_way_or_control() {
        let mut registry = registry(1, 4);
        reserve_request(&mut registry, 1);

        let one_way = registry
            .reserve_one_way(OperationId::new(2), OneWayKind::Data)
            .expect("one-way has no request-capacity cost");
        let control = registry
            .reserve_control(OperationId::new(3), ControlKind::Linktest)
            .expect("control has its own slot");

        assert_eq!(registry.request_len(), 1);
        assert_eq!(registry.one_way_len(), 1);
        assert!(registry.has_control());
        assert_eq!(one_way.system_bytes().get(), 2);
        assert_eq!(control.system_bytes().get(), 3);
        assert_index_invariants(&registry);
    }

    /// Confirms a second control reservation fails atomically at the single slot.
    #[test]
    fn control_slot_failure_does_not_advance_allocator() {
        let mut registry = registry(1, 4);
        registry
            .reserve_control(OperationId::new(1), ControlKind::Select)
            .expect("first control");
        let next = registry.allocator.next_candidate();

        assert_eq!(
            registry.reserve_control(OperationId::new(2), ControlKind::Deselect),
            Err(ReserveError::ControlSlotOccupied {
                pending: ControlKind::Select,
            })
        );
        assert_eq!(registry.allocator.next_candidate(), next);
        assert_index_invariants(&registry);
    }

    /// Confirms an exact response can terminate before commit and suppress later T3.
    #[test]
    fn fast_response_is_terminal_before_write_commit() {
        let mut registry = registry(1, 4);
        let request = reserve_request(&mut registry, 1);

        let decision = registry.classify_inbound(exact_response(request));
        assert!(matches!(
            decision,
            InboundDataDecision::MatchedSecondary {
                operation_id,
                cancel_t3: None,
                visibility: OperationVisibility::NotVisible,
                ..
            } if operation_id == request.operation_id()
        ));
        assert_eq!(
            registry.mark_committed(request.operation_id(), t3(1)),
            CommitDecision::AlreadyTerminal {
                category: TombstoneCategory::ResponseMatched,
            }
        );
        assert!(registry.timers.is_empty());
        assert_eq!(registry.request_len(), 0);
        assert_index_invariants(&registry);
    }

    /// Confirms a response winning after commit cancels T3 and makes expiry stale.
    #[test]
    fn response_wins_serialized_t3_race() {
        let mut registry = registry(1, 4);
        let request = reserve_request(&mut registry, 1);
        let token = t3(1);
        assert!(matches!(
            registry.mark_committed(request.operation_id(), token),
            CommitDecision::ArmTimer { .. }
        ));

        assert!(matches!(
            registry.classify_inbound(exact_response(request)),
            InboundDataDecision::MatchedSecondary {
                cancel_t3: Some(cancel),
                visibility: OperationVisibility::Committed,
                ..
            } if cancel == token
        ));
        assert_eq!(registry.expire_t3(token), ExpiryDecision::Stale);
        assert!(matches!(
            registry.classify_inbound(exact_response(request)),
            InboundDataDecision::Tombstoned {
                category: TombstoneCategory::ResponseMatched,
                arrival: TombstoneArrival::Duplicate,
                ..
            }
        ));
        assert_index_invariants(&registry);
    }

    /// Confirms exact T3 expiry winning first makes the later response late.
    #[test]
    fn t3_expiry_wins_serialized_response_race() {
        let mut registry = registry(1, 4);
        let request = reserve_request(&mut registry, 1);
        let token = t3(1);
        registry.mark_committed(request.operation_id(), token);

        assert!(matches!(
            registry.expire_t3(token),
            ExpiryDecision::Expired {
                operation_id,
                visibility: OperationVisibility::Committed,
                ..
            } if operation_id == request.operation_id()
        ));
        assert_eq!(registry.expire_t3(token), ExpiryDecision::Stale);
        assert!(matches!(
            registry.classify_inbound(exact_response(request)),
            InboundDataDecision::Tombstoned {
                category: TombstoneCategory::T3Expired,
                arrival: TombstoneArrival::Late,
                ..
            }
        ));
        assert_index_invariants(&registry);
    }

    /// Confirms stale and wrong-kind timer tokens never mutate a live request.
    #[test]
    fn only_exact_t3_token_can_expire_request() {
        let mut registry = registry(1, 4);
        let request = reserve_request(&mut registry, 1);
        let exact = t3(1);
        registry.mark_committed(request.operation_id(), exact);

        assert_eq!(registry.expire_t3(t3(2)), ExpiryDecision::Stale);
        assert_eq!(
            registry.expire_t3(t6(1)),
            ExpiryDecision::WrongTimerKind {
                expected: TimeoutKind::T3,
                actual: TimeoutKind::T6,
            }
        );
        assert_eq!(registry.request_len(), 1);
        assert!(matches!(
            registry.classify_inbound(exact_response(request)),
            InboundDataDecision::MatchedSecondary { .. }
        ));
        assert_index_invariants(&registry);
    }

    /// Confirms a same-System-Bytes mismatch preserves the live request for a later match.
    #[test]
    fn mismatched_secondary_does_not_consume_request() {
        let mut registry = registry(1, 4);
        let request = reserve_request(&mut registry, 1);
        let wrong_stream = data_message(request.system_bytes(), 3, 6, 2, false);

        assert!(matches!(
            registry.classify_inbound(wrong_stream),
            InboundDataDecision::Mismatch {
                collision: CollisionSource::LiveRequest {
                    field: MismatchField::Stream,
                },
                ..
            }
        ));
        assert_eq!(registry.request_len(), 1);
        assert!(matches!(
            registry.classify_inbound(exact_response(request)),
            InboundDataDecision::MatchedSecondary { .. }
        ));
        assert_index_invariants(&registry);
    }

    /// Confirms exact F0 aborts while a mismatched F0 leaves the request live.
    #[test]
    fn f0_requires_same_transaction_fields() {
        let mut registry = registry(1, 4);
        let request = reserve_request(&mut registry, 1);

        assert!(matches!(
            registry.classify_inbound(data_message(request.system_bytes(), 4, 5, 0, false,)),
            InboundDataDecision::Mismatch {
                collision: CollisionSource::LiveRequest {
                    field: MismatchField::SessionId,
                },
                ..
            }
        ));
        assert_eq!(registry.request_len(), 1);
        assert!(matches!(
            registry.classify_inbound(exact_abort(request)),
            InboundDataDecision::Aborted {
                operation_id,
                ..
            } if operation_id == request.operation_id()
        ));
        assert_index_invariants(&registry);
    }

    /// Confirms live F0 with W=true preserves an armed T3 until an exact abort.
    #[test]
    fn live_f0_with_w_bit_does_not_consume_request() {
        let mut registry = registry(1, 4);
        let request = reserve_request(&mut registry, 1);
        let exact_token = t3(1);
        assert_eq!(
            registry.mark_committed(request.operation_id(), exact_token),
            CommitDecision::ArmTimer {
                operation_id: request.operation_id(),
                system_bytes: request.system_bytes(),
                class: OperationClass::Request,
                token: exact_token,
            }
        );

        assert!(matches!(
            registry.classify_inbound(data_message(request.system_bytes(), 3, 5, 0, true,)),
            InboundDataDecision::Mismatch {
                collision: CollisionSource::LiveRequest {
                    field: MismatchField::ReplyExpected,
                },
                ..
            }
        ));
        assert_eq!(registry.request_len(), 1);
        assert_index_invariants(&registry);
        assert!(matches!(
            registry.classify_inbound(exact_abort(request)),
            InboundDataDecision::Aborted {
                operation_id,
                cancel_t3: Some(cancel),
                visibility: OperationVisibility::Committed,
                ..
            } if operation_id == request.operation_id() && cancel == exact_token
        ));
        assert_eq!(registry.expire_t3(exact_token), ExpiryDecision::Stale);
        assert_index_invariants(&registry);
    }

    /// Confirms live F0 with Message Text preserves an armed T3 until an exact abort.
    #[test]
    fn live_f0_with_message_text_does_not_consume_request() {
        let mut registry = registry(1, 4);
        let request = reserve_request(&mut registry, 1);
        let exact_token = t3(1);
        assert_eq!(
            registry.mark_committed(request.operation_id(), exact_token),
            CommitDecision::ArmTimer {
                operation_id: request.operation_id(),
                system_bytes: request.system_bytes(),
                class: OperationClass::Request,
                token: exact_token,
            }
        );
        let f0_with_body = data_message_with_body(
            request.system_bytes(),
            3,
            5,
            0,
            false,
            Some(SecsItem::U1(vec![1])),
        );

        assert!(matches!(
            registry.classify_inbound(f0_with_body),
            InboundDataDecision::Mismatch {
                collision: CollisionSource::LiveRequest {
                    field: MismatchField::MessageText,
                },
                ..
            }
        ));
        assert_eq!(registry.request_len(), 1);
        assert_index_invariants(&registry);
        assert!(matches!(
            registry.classify_inbound(exact_abort(request)),
            InboundDataDecision::Aborted {
                operation_id,
                cancel_t3: Some(cancel),
                visibility: OperationVisibility::Committed,
                ..
            } if operation_id == request.operation_id() && cancel == exact_token
        ));
        assert_eq!(registry.expire_t3(exact_token), ExpiryDecision::Stale);
        assert_index_invariants(&registry);
    }

    /// Confirms odd Data is Primary-candidate and orphan even/F0 is never Primary.
    #[test]
    fn inbound_function_parity_controls_primary_candidacy() {
        let mut registry = registry(1, 4);
        let request = reserve_request(&mut registry, 1);

        assert!(matches!(
            registry.classify_inbound(data_message(request.system_bytes(), 3, 5, 3, true,)),
            InboundDataDecision::PrimaryCandidate { .. }
        ));
        assert_eq!(registry.request_len(), 1);
        assert!(matches!(
            registry.classify_inbound(data_message(SystemBytes::new(999), 3, 5, 2, false,)),
            InboundDataDecision::OrphanSecondary { .. }
        ));
        assert!(matches!(
            registry.classify_inbound(data_message(SystemBytes::new(998), 3, 5, 0, false,)),
            InboundDataDecision::OrphanSecondary { .. }
        ));
        assert_index_invariants(&registry);
    }

    /// Confirms write outcomes create tombstones exactly when visibility requires them.
    #[test]
    fn write_outcomes_apply_visibility_tombstone_rules() {
        let mut registry = registry(4, 8);
        let invisible = reserve_request(&mut registry, 1);
        assert!(matches!(
            registry.finish_not_written(invisible.operation_id()),
            FinishDecision::Finished {
                visibility: OperationVisibility::NotVisible,
                tombstone: None,
                ..
            }
        ));
        assert!(!registry.tombstones.contains_key(&invisible.system_bytes()));

        let visible = reserve_request(&mut registry, 2);
        registry.mark_visible(visible.operation_id());
        assert!(matches!(
            registry.finish_not_written(visible.operation_id()),
            FinishDecision::Finished {
                visibility: OperationVisibility::MayBeVisible,
                tombstone: Some(TombstoneCategory::NotWrittenAfterVisibility),
                ..
            }
        ));

        let indeterminate = reserve_request(&mut registry, 3);
        assert!(matches!(
            registry.finish_indeterminate(indeterminate.operation_id()),
            FinishDecision::Finished {
                visibility: OperationVisibility::MayBeVisible,
                tombstone: Some(TombstoneCategory::DeliveryIndeterminate),
                ..
            }
        ));

        let one_way = registry
            .reserve_one_way(OperationId::new(4), OneWayKind::Data)
            .expect("one-way");
        assert!(matches!(
            registry.finish_one_way(one_way.operation_id()),
            FinishDecision::Finished {
                visibility: OperationVisibility::Committed,
                tombstone: Some(TombstoneCategory::OneWayCommitted),
                ..
            }
        ));
        assert_index_invariants(&registry);
    }

    /// Confirms exact tombstones classify duplicate versus late response forms.
    #[test]
    fn request_tombstone_matching_is_exact_and_structured() {
        let mut registry = registry(1, 4);
        let request = reserve_request(&mut registry, 1);
        registry.classify_inbound(exact_abort(request));

        assert!(matches!(
            registry.classify_inbound(exact_abort(request)),
            InboundDataDecision::Tombstoned {
                category: TombstoneCategory::AbortReceived,
                arrival: TombstoneArrival::Duplicate,
                ..
            }
        ));
        assert!(matches!(
            registry.classify_inbound(exact_response(request)),
            InboundDataDecision::Tombstoned {
                category: TombstoneCategory::AbortReceived,
                arrival: TombstoneArrival::Late,
                ..
            }
        ));
        assert!(matches!(
            registry.classify_inbound(data_message(request.system_bytes(), 3, 6, 2, false,)),
            InboundDataDecision::Mismatch {
                collision: CollisionSource::RequestTombstone {
                    category: TombstoneCategory::AbortReceived,
                    field: MismatchField::Stream,
                },
                ..
            }
        ));
        assert_index_invariants(&registry);
    }

    /// Confirms request tombstones reject F0/W=true instead of reporting duplicate or late.
    #[test]
    fn request_tombstone_rejects_f0_with_w_bit() {
        let mut registry = registry(1, 4);
        let request = reserve_request(&mut registry, 1);
        registry.classify_inbound(exact_abort(request));

        assert!(matches!(
            registry.classify_inbound(data_message(request.system_bytes(), 3, 5, 0, true,)),
            InboundDataDecision::Mismatch {
                collision: CollisionSource::RequestTombstone {
                    category: TombstoneCategory::AbortReceived,
                    field: MismatchField::ReplyExpected,
                },
                ..
            }
        ));
        assert_eq!(registry.tombstone_len(), 1);
        assert!(matches!(
            registry.classify_inbound(exact_abort(request)),
            InboundDataDecision::Tombstoned {
                category: TombstoneCategory::AbortReceived,
                arrival: TombstoneArrival::Duplicate,
                ..
            }
        ));
        assert_index_invariants(&registry);
    }

    /// Confirms request tombstones reject body-bearing F0 without losing the tombstone.
    #[test]
    fn request_tombstone_rejects_f0_with_message_text() {
        let mut registry = registry(1, 4);
        let request = reserve_request(&mut registry, 1);
        registry.classify_inbound(exact_abort(request));
        let f0_with_body = data_message_with_body(
            request.system_bytes(),
            3,
            5,
            0,
            false,
            Some(SecsItem::List(Vec::new())),
        );

        assert!(matches!(
            registry.classify_inbound(f0_with_body),
            InboundDataDecision::Mismatch {
                collision: CollisionSource::RequestTombstone {
                    category: TombstoneCategory::AbortReceived,
                    field: MismatchField::MessageText,
                },
                ..
            }
        ));
        assert_eq!(registry.tombstone_len(), 1);
        assert!(matches!(
            registry.classify_inbound(exact_abort(request)),
            InboundDataDecision::Tombstoned {
                category: TombstoneCategory::AbortReceived,
                arrival: TombstoneArrival::Duplicate,
                ..
            }
        ));
        assert_index_invariants(&registry);
    }

    /// Confirms tombstone capacity evicts the oldest exact identity in FIFO order.
    #[test]
    fn tombstone_eviction_is_fifo() {
        let mut registry = registry(1, 2);
        let first = reserve_request(&mut registry, 1);
        registry.classify_inbound(exact_response(first));
        let second = reserve_request(&mut registry, 2);
        registry.classify_inbound(exact_response(second));
        let third = reserve_request(&mut registry, 3);
        registry.classify_inbound(exact_response(third));

        assert_eq!(registry.tombstone_len(), 2);
        assert!(!registry.tombstones.contains_key(&first.system_bytes()));
        assert!(registry.tombstones.contains_key(&second.system_bytes()));
        assert!(registry.tombstones.contains_key(&third.system_bytes()));
        assert!(matches!(
            registry.classify_inbound(exact_response(first)),
            InboundDataDecision::OrphanSecondary { .. }
        ));
        assert!(matches!(
            registry.classify_inbound(exact_response(second)),
            InboundDataDecision::Tombstoned { .. }
        ));
        assert_index_invariants(&registry);
    }

    /// Confirms control response consumption requires both typed kind and System Bytes.
    #[test]
    fn control_take_matches_kind_and_system_bytes() {
        let mut registry = registry(1, 4);
        let control = registry
            .reserve_control(OperationId::new(1), ControlKind::Select)
            .expect("control");

        assert!(matches!(
            registry.take_control(ControlKind::Deselect, control.system_bytes()),
            ControlTakeDecision::Mismatch {
                collision: ControlCollision::Live {
                    kind: ControlKind::Select,
                    ..
                },
            }
        ));
        assert!(matches!(
            registry.take_control(ControlKind::Select, SystemBytes::new(999)),
            ControlTakeDecision::Mismatch {
                collision: ControlCollision::Live { .. },
            }
        ));
        assert!(registry.has_control());
        assert!(matches!(
            registry.take_control(ControlKind::Select, control.system_bytes()),
            ControlTakeDecision::Matched {
                operation_id,
                cancel_t6: None,
                ..
            } if operation_id == control.operation_id()
        ));
        assert!(matches!(
            registry.take_control(ControlKind::Select, control.system_bytes()),
            ControlTakeDecision::Tombstoned {
                category: TombstoneCategory::ControlResponseMatched,
                arrival: TombstoneArrival::Duplicate,
                ..
            }
        ));
        assert_index_invariants(&registry);
    }

    /// Confirms exact committed control expiry wins and later response is late.
    #[test]
    fn control_expiry_requires_exact_t6() {
        let mut registry = registry(1, 4);
        let control = registry
            .reserve_control(OperationId::new(1), ControlKind::Linktest)
            .expect("control");
        let token = t6(1);
        assert!(matches!(
            registry.mark_committed(control.operation_id(), token),
            CommitDecision::ArmTimer {
                class: OperationClass::Control(ControlKind::Linktest),
                ..
            }
        ));

        assert_eq!(registry.expire_control(t6(2)), ExpiryDecision::Stale);
        assert!(matches!(
            registry.expire_control(token),
            ExpiryDecision::Expired {
                class: OperationClass::Control(ControlKind::Linktest),
                ..
            }
        ));
        assert_eq!(registry.expire_control(token), ExpiryDecision::Stale);
        assert!(matches!(
            registry.take_control(ControlKind::Linktest, control.system_bytes()),
            ControlTakeDecision::Tombstoned {
                category: TombstoneCategory::ControlExpired,
                arrival: TombstoneArrival::Late,
                ..
            }
        ));
        assert_index_invariants(&registry);
    }

    /// Confirms one exact timer token cannot be attached to two live requests.
    #[test]
    fn timer_token_ownership_is_unique_and_atomic() {
        let mut registry = registry(2, 4);
        let first = reserve_request(&mut registry, 1);
        let second = reserve_request(&mut registry, 2);
        let shared = t3(1);
        registry.mark_committed(first.operation_id(), shared);

        assert_eq!(
            registry.mark_committed(second.operation_id(), shared),
            CommitDecision::TimerTokenInUse { token: shared }
        );
        assert!(matches!(
            registry.mark_committed(second.operation_id(), t3(2)),
            CommitDecision::ArmTimer { .. }
        ));
        assert_index_invariants(&registry);
    }

    /// Confirms the first close drains all classes and later close calls are inert.
    #[test]
    fn begin_close_is_fenced_structured_and_idempotent() {
        let mut registry = registry(2, 8);
        let request = reserve_request(&mut registry, 1);
        let request_timer = t3(1);
        registry.mark_committed(request.operation_id(), request_timer);
        let invisible = registry
            .reserve_one_way(OperationId::new(2), OneWayKind::Data)
            .expect("one-way");
        let control = registry
            .reserve_control(OperationId::new(3), ControlKind::Deselect)
            .expect("control");
        registry.mark_visible(control.operation_id());

        let close = registry.begin_close();

        assert!(close.began_close());
        assert_eq!(close.operations().len(), 3);
        assert_eq!(
            close
                .operations()
                .iter()
                .map(|entry| entry.operation_id().get())
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(close.operations()[0].cancel_timer(), Some(request_timer));
        assert_eq!(
            close.operations()[0].tombstone(),
            Some(TombstoneCategory::ClosedAfterVisibility)
        );
        assert_eq!(
            close.operations()[1].visibility(),
            OperationVisibility::NotVisible
        );
        assert_eq!(close.operations()[1].tombstone(), None);
        assert_eq!(
            close.operations()[2].tombstone(),
            Some(TombstoneCategory::ClosedAfterVisibility)
        );
        assert_eq!(registry.request_len(), 0);
        assert_eq!(registry.one_way_len(), 0);
        assert!(!registry.has_control());
        assert!(registry.timers.is_empty());
        assert!(matches!(
            registry.classify_inbound(exact_response(request)),
            InboundDataDecision::Tombstoned {
                category: TombstoneCategory::ClosedAfterVisibility,
                arrival: TombstoneArrival::Late,
                ..
            }
        ));
        assert!(matches!(
            registry.take_control(ControlKind::Deselect, control.system_bytes()),
            ControlTakeDecision::Tombstoned {
                category: TombstoneCategory::ClosedAfterVisibility,
                ..
            }
        ));
        assert!(!registry.begin_close().began_close());
        assert!(registry.begin_close().operations().is_empty());
        assert_eq!(
            registry.reserve_one_way(OperationId::new(4), OneWayKind::Separate),
            Err(ReserveError::Closing)
        );
        assert!(!registry.tombstones.contains_key(&invisible.system_bytes()));
        assert_index_invariants(&registry);
    }

    /// Confirms a mixed mutation sequence preserves every index invariant.
    #[test]
    fn mixed_operations_preserve_index_invariants() {
        let mut registry = registry(3, 3);
        let first = reserve_request(&mut registry, 1);
        let second = reserve_request(&mut registry, 2);
        let one_way = registry
            .reserve_one_way(OperationId::new(3), OneWayKind::Separate)
            .expect("one-way");
        let control = registry
            .reserve_control(OperationId::new(4), ControlKind::Select)
            .expect("control");
        assert_index_invariants(&registry);

        registry.mark_committed(first.operation_id(), t3(1));
        registry.mark_visible(second.operation_id());
        registry.mark_committed(control.operation_id(), t6(1));
        assert_index_invariants(&registry);

        registry.classify_inbound(exact_response(first));
        registry.finish_indeterminate(second.operation_id());
        registry.finish_one_way(one_way.operation_id());
        registry.take_control(ControlKind::Select, control.system_bytes());
        assert_index_invariants(&registry);
        assert_eq!(registry.tombstone_len(), 3);
    }

    /// Confirms transaction sources contain no async runtime, network, clock, or lock dependency.
    #[test]
    fn transaction_module_has_no_async_runtime_dependencies() {
        let sources = [
            include_str!("allocator.rs"),
            include_str!("matcher.rs"),
            include_str!("registry.rs"),
            include_str!("mod.rs"),
        ];
        let forbidden = [
            concat!("to", "kio"),
            concat!("std::", "net"),
            concat!("async", " fn"),
            concat!("std::time::", "Instant"),
            concat!("std::sync::", "Mutex"),
            concat!("std::sync::", "RwLock"),
            concat!("sync::mpsc", "::channel"),
        ];

        for source in sources {
            for dependency in forbidden {
                assert!(
                    !source.contains(dependency),
                    "transaction source unexpectedly contains {dependency}"
                );
            }
        }
    }
}
