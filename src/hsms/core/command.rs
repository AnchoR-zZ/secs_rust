//! Generation-local commands accepted by the future deterministic Session Core.
//!
//! The Driver assigns command identities and translates public API intents into
//! these runtime-neutral values before invoking Core. Completion results remain
//! typed so future Data operations can add their own result shapes without
//! changing the ordered action envelope.

use crate::hsms::{error::OperationError, model::ids::CommandId};

/// One Driver-accepted command presented to Session Core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CoreCommand {
    /// Driver-assigned identity of this accepted generation-local command.
    command_id: CommandId,
    /// Protocol intent supported by the current Core slice.
    kind: CoreCommandKind,
}

impl CoreCommand {
    /// Creates a Core command for `kind` correlated by `command_id`.
    pub(crate) const fn new(command_id: CommandId, kind: CoreCommandKind) -> Self {
        Self { command_id, kind }
    }

    /// Returns the Driver-assigned command identity.
    pub(crate) const fn command_id(self) -> CommandId {
        self.command_id
    }

    /// Returns the protocol intent carried by this command.
    pub(crate) const fn kind(self) -> CoreCommandKind {
        self.kind
    }
}

/// Protocol intents implemented by the first control-only Core slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CoreCommandKind {
    /// Initiates an active HSMS Select transaction.
    Select,
    /// Initiates an active HSMS Linktest transaction.
    Linktest,
    /// Initiates the unacknowledged HSMS Separate shutdown procedure.
    Separate,
}

/// Typed terminal result delivered for one accepted Core command.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CoreCommandResult {
    /// Completion of a control operation with its stable operation error.
    Control(Result<(), OperationError>),
}

#[cfg(test)]
mod tests {
    use crate::hsms::{error::OperationError, model::ids::CommandId};

    use super::{CoreCommand, CoreCommandKind, CoreCommandResult};

    /// Confirms command construction preserves the Driver identity and exact
    /// control intent without allocating or translating either value.
    #[test]
    fn command_preserves_identity_and_kind() {
        let command_id = CommandId::new(41);
        let command = CoreCommand::new(command_id, CoreCommandKind::Linktest);

        assert_eq!(command.command_id(), command_id);
        assert_eq!(command.kind(), CoreCommandKind::Linktest);
    }

    /// Confirms the frozen command vocabulary retains all three B1 control
    /// intents as distinct value variants.
    #[test]
    fn command_kinds_are_distinct_values() {
        assert_ne!(CoreCommandKind::Select, CoreCommandKind::Linktest);
        assert_ne!(CoreCommandKind::Linktest, CoreCommandKind::Separate);
        assert_ne!(CoreCommandKind::Separate, CoreCommandKind::Select);
    }

    /// Confirms control completions preserve both success and stable operation
    /// errors without flattening the result to an untyped status.
    #[test]
    fn control_result_preserves_terminal_outcome() {
        assert_eq!(
            CoreCommandResult::Control(Ok(())),
            CoreCommandResult::Control(Ok(()))
        );
        assert_eq!(
            CoreCommandResult::Control(Err(OperationError::ConnectionLost)),
            CoreCommandResult::Control(Err(OperationError::ConnectionLost))
        );
    }
}
