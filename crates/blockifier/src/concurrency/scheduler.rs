use std::cmp::min;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use crate::concurrency::utils::lock_mutex_in_array;
use crate::concurrency::TxIndex;

#[cfg(test)]
#[path = "scheduler_test.rs"]
pub mod test;

#[derive(Debug, Default)]
pub struct Scheduler {
    execution_index: AtomicUsize,
    validation_index: AtomicUsize,
    /// Read twice upon checking the chunk completion. Used to detect if validation or execution
    /// index decreased from their observed values after ensuring that the number of active tasks
    /// is zero.
    decrease_counter: AtomicUsize,
    n_active_tasks: AtomicUsize,
    chunk_size: usize,
    // TODO(Avi, 15/05/2024): Consider using RwLock instead of Mutex.
    tx_statuses: Box<[Mutex<TransactionStatus>]>,
    /// Updated by the `check_done` procedure, providing a cheap way for all threads to exit their
    /// main loops.
    done_marker: AtomicBool,
}

impl Scheduler {
    pub fn new(chunk_size: usize) -> Scheduler {
        Scheduler {
            execution_index: AtomicUsize::new(0),
            validation_index: AtomicUsize::new(chunk_size),
            decrease_counter: AtomicUsize::new(0),
            n_active_tasks: AtomicUsize::new(0),
            chunk_size,
            tx_statuses: std::iter::repeat_with(|| Mutex::new(TransactionStatus::ReadyToExecute))
                .take(chunk_size)
                .collect(),
            done_marker: AtomicBool::new(false),
        }
    }

    /// Returns the done marker.
    fn done(&self) -> bool {
        self.done_marker.load(Ordering::Acquire)
    }

    pub fn next_task(&self) -> Task {
        if self.done() {
            return Task::Done;
        }

        let index_to_validate = self.validation_index.load(Ordering::Acquire);
        let index_to_execute = self.execution_index.load(Ordering::Acquire);

        if min(index_to_validate, index_to_execute) >= self.chunk_size {
            return Task::NoTask;
        }

        if index_to_validate < index_to_execute {
            if let Some(tx_index) = self.next_version_to_validate() {
                return Task::ValidationTask(tx_index);
            }
        }

        if let Some(tx_index) = self.next_version_to_execute() {
            return Task::ExecutionTask(tx_index);
        }

        Task::NoTask
    }

    /// Updates the Scheduler that an execution task has been finished and triggers the creation of
    /// new tasks accordingly: schedules validation for the current and higher transactions, if not
    /// already scheduled.
    pub fn finish_execution(&self, tx_index: TxIndex) {
        self.set_executed_status(tx_index);
        if self.validation_index.load(Ordering::Acquire) > tx_index {
            self.decrease_validation_index(tx_index);
        }
        self.safe_decrement_n_active_tasks();
    }

    pub fn try_validation_abort(&self, tx_index: TxIndex) -> bool {
        let mut status = self.lock_tx_status(tx_index);
        if *status == TransactionStatus::Executed {
            *status = TransactionStatus::Aborting;
            return true;
        }
        false
    }

    /// Updates the Scheduler that a validation task has been finished and triggers the creation of
    /// new tasks in case of failure: schedules validation for higher transactions + re-executes the
    /// current transaction (if ready).
    pub fn finish_validation(&self, tx_index: TxIndex, aborted: bool) -> Task {
        if aborted {
            self.set_ready_status(tx_index);
            if self.execution_index.load(Ordering::Acquire) > tx_index
                && self.try_incarnate(tx_index)
            {
                return Task::ExecutionTask(tx_index);
            }
        }
        self.safe_decrement_n_active_tasks();

        Task::NoTask
    }

    /// Checks if all transactions have been executed and validated.
    fn check_done(&self) {
        let observed_decrease_counter = self.decrease_counter.load(Ordering::Acquire);

        if min(
            self.validation_index.load(Ordering::Acquire),
            self.execution_index.load(Ordering::Acquire),
        ) >= self.chunk_size
            && self.n_active_tasks.load(Ordering::Acquire) == 0
            && observed_decrease_counter == self.decrease_counter.load(Ordering::Acquire)
        {
            self.done_marker.store(true, Ordering::Release);
        }
    }

    fn safe_decrement_n_active_tasks(&self) {
        let previous_n_active_tasks = self.n_active_tasks.fetch_sub(1, Ordering::SeqCst);
        assert!(previous_n_active_tasks > 0, "n_active_tasks underflow");
    }

    fn lock_tx_status(&self, tx_index: TxIndex) -> MutexGuard<'_, TransactionStatus> {
        lock_mutex_in_array(&self.tx_statuses, tx_index)
    }

    fn set_executed_status(&self, tx_index: TxIndex) {
        let mut status = self.lock_tx_status(tx_index);
        assert_eq!(
            *status,
            TransactionStatus::Executing,
            "Only executing transactions can gain status executed. Transaction {tx_index} is not \
             executing. Transaction status: {status:?}."
        );
        *status = TransactionStatus::Executed;
    }

    fn set_ready_status(&self, tx_index: TxIndex) {
        let mut status = self.lock_tx_status(tx_index);
        assert_eq!(
            *status,
            TransactionStatus::Aborting,
            "Only aborting transactions can be re-executed. Transaction {tx_index} is not \
             aborting. Transaction status: {status:?}."
        );
        *status = TransactionStatus::ReadyToExecute;
    }

    fn decrease_validation_index(&self, target_index: TxIndex) {
        let previous_validation_index =
            self.validation_index.fetch_min(target_index, Ordering::SeqCst);
        if target_index < previous_validation_index {
            self.decrease_counter.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Updates a transaction's status to `Executing` if it is ready to execute.
    fn try_incarnate(&self, tx_index: TxIndex) -> bool {
        if tx_index < self.chunk_size {
            let mut status = self.lock_tx_status(tx_index);
            if *status == TransactionStatus::ReadyToExecute {
                *status = TransactionStatus::Executing;
                return true;
            }
        }
        self.safe_decrement_n_active_tasks();
        false
    }

    fn next_version_to_validate(&self) -> Option<TxIndex> {
        let index_to_validate = self.validation_index.load(Ordering::Acquire);
        if index_to_validate >= self.chunk_size {
            self.check_done();
            return None;
        }
        self.n_active_tasks.fetch_add(1, Ordering::SeqCst);
        let index_to_validate = self.validation_index.fetch_add(1, Ordering::SeqCst);
        if index_to_validate < self.chunk_size {
            let status = self.lock_tx_status(index_to_validate);
            if *status == TransactionStatus::Executed {
                return Some(index_to_validate);
            }
        }
        self.safe_decrement_n_active_tasks();
        None
    }

    fn next_version_to_execute(&self) -> Option<TxIndex> {
        let index_to_execute = self.execution_index.load(Ordering::Acquire);
        if index_to_execute >= self.chunk_size {
            self.check_done();
            return None;
        }
        self.n_active_tasks.fetch_add(1, Ordering::SeqCst);
        let index_to_execute = self.execution_index.fetch_add(1, Ordering::SeqCst);
        if self.try_incarnate(index_to_execute) {
            return Some(index_to_execute);
        }
        None
    }

    #[cfg(test)]
    fn set_tx_status(&self, tx_index: TxIndex, status: TransactionStatus) {
        if tx_index < self.chunk_size {
            let mut tx_status = self.lock_tx_status(tx_index);
            *tx_status = status;
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Task {
    ExecutionTask(TxIndex),
    ValidationTask(TxIndex),
    NoTask,
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum TransactionStatus {
    ReadyToExecute,
    Executing,
    Executed,
    Aborting,
}
