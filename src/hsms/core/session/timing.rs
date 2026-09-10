//! Logical protocol deadlines and autonomous idle probes for SessionCore.
//!
//! All time is supplied by the owner. This module neither sleeps nor reads a
//! physical clock; the Driver schedules its single sleep from next_deadline.

use crate::hsms::{
    error::{OperationError, TimeoutKind},
    model::runtime::{CloseBarrier, CommunicationsTimeoutKind, GenerationCloseReason, MonoTime},
};

use super::{ControlTransaction, CoreActions, SessionCore, TransactionKind};

#[cfg(test)]
mod tests;

impl SessionCore {
    /// Returns the earliest active deadline, or none before connect/while closing.
    pub(crate) fn next_deadline(&self) -> Option<MonoTime> {
        if self.closing || self.state.is_none() {
            return None;
        }
        self.selection_deadline
            .into_iter()
            .chain(
                self.active_transaction
                    .and_then(|transaction| transaction.deadline),
            )
            .chain(
                self.data_transactions
                    .values()
                    .filter_map(|transaction| transaction.deadline),
            )
            .chain(
                self.idle_deadline
                    .filter(|_| self.active_transaction.is_none() && self.deselect_drain.is_none()),
            )
            .chain(self.deselect_drain.map(|drain| drain.deadline))
            .min()
    }

    /// Arms one NotSelected tenure from `now`; overflow is an invariant failure.
    pub(super) fn arm_selection_timeout(&mut self, now: MonoTime, actions: &mut CoreActions) {
        self.selection_deadline = now.checked_add(self.config.timeouts.t7());
        if self.selection_deadline.is_none() {
            self.fail_runtime_invariant(actions);
        }
    }

    /// Records real activity without letting a delayed writer fact move it back.
    pub(super) fn record_activity(&mut self, occurred_at: MonoTime, actions: &mut CoreActions) {
        let latest = self
            .last_activity
            .map_or(occurred_at, |previous| previous.max(occurred_at));
        self.last_activity = Some(latest);
        self.idle_deadline = match self.config.timeouts.linktest() {
            Some(interval) => match latest.checked_add(interval) {
                Some(deadline) => Some(deadline),
                None => {
                    self.fail_runtime_invariant(actions);
                    None
                }
            },
            None => None,
        };
    }

    /// Completes application control work; autonomous transactions have no sender.
    pub(super) fn complete_control_transaction(
        &mut self,
        transaction: ControlTransaction,
        result: Result<(), OperationError>,
        actions: &mut CoreActions,
    ) {
        if let Some(command_id) = transaction.command_id {
            self.complete_control(command_id, result, actions);
        }
    }

    /// Expires closing timers first, then T3 by CommandId, then the idle probe.
    ///
    /// T3 only retires its request. A communications timeout closes the whole
    /// generation. An autonomous probe occupies the existing sole control slot.
    pub(super) fn expire_deadlines(&mut self, now: MonoTime, actions: &mut CoreActions) {
        if self.closing || self.state.is_none() {
            return;
        }
        if self
            .selection_deadline
            .is_some_and(|deadline| deadline <= now)
        {
            self.complete_all(OperationError::ConnectionLost, actions);
            self.request_close(
                GenerationCloseReason::CommunicationsTimeout(CommunicationsTimeoutKind::T7),
                CloseBarrier::Immediate,
                actions,
            );
            return;
        }
        if let Some(transaction) = self
            .active_transaction
            .filter(|transaction| transaction.deadline.is_some_and(|deadline| deadline <= now))
        {
            self.active_transaction = None;
            self.complete_control_transaction(
                transaction,
                Err(OperationError::Timeout(TimeoutKind::T6)),
                actions,
            );
            self.complete_all(OperationError::ConnectionLost, actions);
            self.request_close(
                GenerationCloseReason::CommunicationsTimeout(CommunicationsTimeoutKind::T6),
                CloseBarrier::Immediate,
                actions,
            );
            return;
        }
        let mut expired: Vec<_> = self
            .data_transactions
            .values()
            .filter(|transaction| transaction.deadline.is_some_and(|deadline| deadline <= now))
            .copied()
            .collect();
        expired.sort_unstable_by_key(|transaction| transaction.command_id);
        for transaction in expired {
            self.data_transactions
                .remove(&transaction.response_contract.system_bytes());
            self.tombstones.insert(transaction.response_contract);
            self.emit_completion(
                transaction.command_id,
                super::CoreCommandResult::RequestTimedOut(
                    transaction.response_contract.request_header(),
                ),
                actions,
            );
        }
        self.progress_deselect(now, actions);
        if self.closing {
            return;
        }
        if self.active_transaction.is_none()
            && self.deselect_drain.is_none()
            && self.idle_deadline.is_some_and(|deadline| deadline <= now)
        {
            self.idle_deadline = None;
            self.start_transaction(TransactionKind::Linktest, None, actions);
        }
    }
}
