//! Microphone, Accessibility, and UI Automation permission probes belong here.

use remtene_application::ports::{
    MicrophoneAccess, MicrophonePermissionPort, PortError, PortFuture,
};

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod macos;

#[cfg(target_os = "macos")]
pub use macos::MacOsMicrophonePermission;
// Re-export status enum defined below for callers that probe without prompting.

/// Read-only operating-system authorization state for status presentation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MicrophoneAuthorizationStatus {
    NotDetermined,
    Restricted,
    Denied,
    Authorized,
    Unavailable,
}

/// Fail-closed adapter used on platforms whose native implementation has not
/// yet been installed. It never opens a microphone or requests a permission.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailableMicrophonePermission;

impl MicrophonePermissionPort for UnavailableMicrophonePermission {
    fn request_recording_access(&self) -> PortFuture<'_, Result<MicrophoneAccess, PortError>> {
        Box::pin(async { Ok(MicrophoneAccess::Unavailable) })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        sync::Arc,
        task::{Context, Poll, Wake, Waker},
        thread,
    };

    use super::*;

    #[test]
    fn unsupported_adapter_fails_closed_without_error_guessing() {
        assert_eq!(
            block_on(UnavailableMicrophonePermission.request_recording_access())
                .expect("unsupported platform state is a known result"),
            MicrophoneAccess::Unavailable
        );
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        struct ThreadWake(thread::Thread);

        impl Wake for ThreadWake {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }

        let waker = Waker::from(Arc::new(ThreadWake(thread::current())));
        let mut context = Context::from_waker(&waker);
        let mut future = Box::pin(future);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(value) => return value,
                Poll::Pending => thread::park(),
            }
        }
    }
}
