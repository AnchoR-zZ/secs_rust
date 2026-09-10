//! Generation-local owned commands and typed completions for Session Core.
//!
//! The Driver translates public intents into these runtime-neutral values and
//! transfers ownership to Core. Completion values contain only facts owned by
//! Core; the Driver adds generation and wire-order facts at the API boundary.

use crate::{
    hsms::{
        error::OperationError,
        model::ids::{CommandId, WriteId},
    },
    secs2::SecsItem,
};

pub(crate) use crate::secs2::PrimaryMessage as OutboundPrimary;

/// One Driver-accepted command presented to Session Core.
#[derive(Debug, PartialEq)]
pub(crate) struct CoreCommand {
    /// Driver-assigned identity of this accepted generation-local command.
    command_id: CommandId,
    /// Owned protocol intent transferred to Core exactly once.
    kind: CoreCommandKind,
}

impl CoreCommand {
    /// Creates a Core command for `kind` correlated by `command_id`.
    pub(crate) const fn new(command_id: CommandId, kind: CoreCommandKind) -> Self {
        Self { command_id, kind }
    }

    /// Returns the Driver-assigned command identity without consuming payload.
    #[cfg(test)]
    pub(crate) const fn command_id(&self) -> CommandId {
        self.command_id
    }

    /// Consumes the command into its identity and owned protocol intent.
    pub(crate) fn into_parts(self) -> (CommandId, CoreCommandKind) {
        (self.command_id, self.kind)
    }
}

/// Protocol intents implemented by the runtime-neutral Session Core.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CoreCommandKind {
    /// Drains Data work before initiating a transactional HSMS Deselect.
    Deselect,
    /// Initiates an active HSMS Select transaction.
    Select,
    /// Initiates an active HSMS Linktest transaction.
    Linktest,
    /// Initiates the unacknowledged HSMS Separate shutdown procedure.
    Separate,
    /// Sends one W=0 Data Primary and completes at local writer commit.
    Send(OutboundPrimary),
    /// Sends one W=1 Data Primary and awaits a matching Secondary or abort.
    Request(OutboundPrimary),
}

/// Core-owned proof that one outbound Send reached the local commit point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CommittedWrite {
    /// Core-assigned write identity resolved by Driver to a wire sequence.
    write_id: WriteId,
}

impl CommittedWrite {
    /// Creates commit proof for the exact `write_id` reported by Writer.
    pub(crate) const fn new(write_id: WriteId) -> Self {
        Self { write_id }
    }

    /// Returns the committed Core write identity for Driver correlation.
    pub(crate) const fn write_id(self) -> WriteId {
        self.write_id
    }
}

/// Core-validated Secondary content returned by an outbound Request.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MatchedSecondary {
    /// Complete received header proven to match the pending response contract.
    header: crate::hsms::protocol::header::DataHeader,
    /// Matched Message Text, preserving absent and typed-empty forms.
    body: Option<SecsItem>,
}

impl MatchedSecondary {
    /// Creates a result from fields already validated by the Core matcher.
    pub(crate) const fn new(
        header: crate::hsms::protocol::header::DataHeader,
        body: Option<SecsItem>,
    ) -> Self {
        Self { header, body }
    }

    /// Consumes the result into fields used to construct the public Secondary.
    pub(crate) fn into_parts(
        self,
    ) -> (crate::hsms::protocol::header::DataHeader, Option<SecsItem>) {
        (self.header, self.body)
    }
}

/// Typed terminal result delivered for one accepted Core command.
#[derive(Debug, PartialEq)]
pub(crate) enum CoreCommandResult {
    /// T3 expiry retaining the exact outbound header for application diagnostics.
    RequestTimedOut(crate::hsms::protocol::header::DataHeader),
    /// Completion of a control operation with its stable operation error.
    Control(Result<(), OperationError>),
    /// Completion of an outbound W=0 Primary at commit or stable failure.
    Send(Result<CommittedWrite, OperationError>),
    /// Completion of an outbound W=1 request with matched content or failure.
    Request(Result<MatchedSecondary, OperationError>),
}

#[cfg(test)]
mod tests {
    use crate::{
        hsms::{
            error::OperationError,
            model::ids::{CommandId, Function, Stream, WriteId},
        },
        secs2::SecsItem,
    };

    use super::{
        CommittedWrite, CoreCommand, CoreCommandKind, CoreCommandResult, MatchedSecondary,
        OutboundPrimary,
    };

    /// Returns the fixed valid stream used by owned-command value tests.
    fn stream() -> Stream {
        Stream::new(7).expect("fixture stream must be valid")
    }

    /// Confirms command consumption transfers identity and owned body exactly.
    #[test]
    fn command_transfers_identity_kind_and_body() {
        let body = Some(SecsItem::List(Vec::new()));
        let command = CoreCommand::new(
            CommandId::new(41),
            CoreCommandKind::Request(OutboundPrimary::new(
                stream(),
                Function::new(3),
                body.clone(),
            )),
        );

        assert_eq!(command.command_id(), CommandId::new(41));
        let (command_id, kind) = command.into_parts();
        let CoreCommandKind::Request(primary) = kind else {
            panic!("request command must retain its owned payload");
        };
        assert_eq!(command_id, CommandId::new(41));
        assert_eq!(primary.into_parts(), (stream(), Function::new(3), body));
    }

    /// Confirms all B1 control and B2 Data intents remain distinct variants.
    #[test]
    fn command_kinds_are_distinct_values() {
        assert_ne!(CoreCommandKind::Select, CoreCommandKind::Linktest);
        assert_ne!(CoreCommandKind::Linktest, CoreCommandKind::Separate);
        assert_ne!(
            CoreCommandKind::Send(OutboundPrimary::new(stream(), Function::new(1), None)),
            CoreCommandKind::Request(OutboundPrimary::new(stream(), Function::new(1), None))
        );
    }

    /// Confirms typed results retain Core-owned success and failure facts.
    #[test]
    fn typed_results_preserve_terminal_outcomes() {
        let write_id = WriteId::new(13);
        let body = Some(SecsItem::U1(Vec::new()));

        assert_eq!(
            CoreCommandResult::Control(Err(OperationError::ConnectionLost)),
            CoreCommandResult::Control(Err(OperationError::ConnectionLost))
        );
        assert_eq!(
            CoreCommandResult::Send(Ok(CommittedWrite::new(write_id))),
            CoreCommandResult::Send(Ok(CommittedWrite::new(write_id)))
        );
        assert_eq!(CommittedWrite::new(write_id).write_id(), write_id);

        let header = crate::hsms::protocol::header::DataHeader::new(
            crate::hsms::SessionId::new(7).unwrap(),
            stream(),
            Function::new(2),
            false,
            crate::hsms::model::ids::SystemBytes::new(42),
        );
        let matched = MatchedSecondary::new(header, body.clone());
        assert_eq!(matched.into_parts(), (header, body));
    }
}
