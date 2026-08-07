//! Session（任务）级录音时长上限的 Tauri Runtime Adapter（运行时适配器）。

use std::{
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

use remtene_application::ports::{
    PortError, RecordingDeadlineGuard, RecordingDeadlinePort, RecordingDeadlineTask,
};

#[derive(Default)]
pub(crate) struct TauriRecordingDeadline;

struct DeadlineState {
    cancelled: bool,
    task: Option<RecordingDeadlineTask>,
}

impl RecordingDeadlinePort for TauriRecordingDeadline {
    fn schedule(
        &self,
        duration: Duration,
        on_elapsed: RecordingDeadlineTask,
    ) -> Result<RecordingDeadlineGuard, PortError> {
        let shared = Arc::new((
            Mutex::new(DeadlineState {
                cancelled: false,
                task: Some(on_elapsed),
            }),
            Condvar::new(),
        ));
        let worker_state = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("remtene-recording-deadline".to_owned())
            .spawn(move || {
                let (lock, wake) = &*worker_state;
                let state = lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let (mut state, timeout) = wake
                    .wait_timeout_while(state, duration, |state| !state.cancelled)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let task = (!state.cancelled && timeout.timed_out())
                    .then(|| state.task.take())
                    .flatten();
                drop(state);
                if let Some(task) = task {
                    tauri::async_runtime::spawn(task);
                }
            })
            .map_err(|_| PortError {
                code: "recording.deadline_unavailable".to_owned(),
                safe_message_key: "errors.recording.deadline_unavailable".to_owned(),
                retryable: true,
            })?;

        Ok(RecordingDeadlineGuard::new(move || {
            let (lock, wake) = &*shared;
            let mut state = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.cancelled = true;
            state.task = None;
            wake.notify_all();
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn dropping_guard_cancels_a_pending_deadline() {
        let (sender, receiver) = mpsc::channel();
        let guard = TauriRecordingDeadline
            .schedule(
                Duration::from_millis(40),
                Box::pin(async move {
                    let _ = sender.send(());
                }),
            )
            .expect("schedule deadline");
        drop(guard);

        assert!(receiver.recv_timeout(Duration::from_millis(80)).is_err());
    }

    #[test]
    fn elapsed_deadline_dispatches_exactly_one_task() {
        let (sender, receiver) = mpsc::channel();
        let guard = TauriRecordingDeadline
            .schedule(
                Duration::from_millis(10),
                Box::pin(async move {
                    let _ = sender.send(());
                }),
            )
            .expect("schedule deadline");

        receiver
            .recv_timeout(Duration::from_millis(100))
            .expect("elapsed deadline should dispatch");
        assert!(receiver.recv_timeout(Duration::from_millis(30)).is_err());
        drop(guard);
    }
}
