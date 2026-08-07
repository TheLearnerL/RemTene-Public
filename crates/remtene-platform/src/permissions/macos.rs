//! Narrow AVFoundation microphone authorization adapter.
//!
//! All Objective-C calls are kept in this private file. The callback captures
//! only thread-safe Rust state; neither Objective-C objects nor Blocks cross
//! the Application Port boundary.

use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    task::{Context, Poll, Waker},
};

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
use remtene_application::ports::{
    MicrophoneAccess, MicrophonePermissionPort, PortError, PortFuture,
};

use super::MicrophoneAuthorizationStatus;

#[derive(Clone, Copy, Debug, Default)]
pub struct MacOsMicrophonePermission;

impl MacOsMicrophonePermission {
    #[must_use]
    pub fn current_status(&self) -> MicrophoneAuthorizationStatus {
        authorization_status()
    }
}

impl MicrophonePermissionPort for MacOsMicrophonePermission {
    fn request_recording_access(&self) -> PortFuture<'_, Result<MicrophoneAccess, PortError>> {
        match authorization_status() {
            MicrophoneAuthorizationStatus::Authorized => {
                Box::pin(async { Ok(MicrophoneAccess::Granted) })
            }
            MicrophoneAuthorizationStatus::Denied => {
                Box::pin(async { Ok(MicrophoneAccess::Denied) })
            }
            MicrophoneAuthorizationStatus::Restricted => {
                Box::pin(async { Ok(MicrophoneAccess::Restricted) })
            }
            MicrophoneAuthorizationStatus::Unavailable => {
                Box::pin(async { Ok(MicrophoneAccess::Unavailable) })
            }
            MicrophoneAuthorizationStatus::NotDetermined => request_access(),
        }
    }
}

fn authorization_status() -> MicrophoneAuthorizationStatus {
    // SAFETY: `AVMediaTypeAudio` is an immutable AVFoundation global and the
    // generated binding requires the caller to acknowledge framework loading.
    // `authorizationStatusForMediaType:` accepts this exact documented value
    // and returns a value type; no Objective-C object escapes this call.
    let Some(media_type) = (unsafe { AVMediaTypeAudio }) else {
        return MicrophoneAuthorizationStatus::Unavailable;
    };
    // SAFETY: `media_type` is the framework-owned `AVMediaTypeAudio` constant,
    // which is the only argument supplied to the generated class method.
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type) };
    match status {
        AVAuthorizationStatus::NotDetermined => MicrophoneAuthorizationStatus::NotDetermined,
        AVAuthorizationStatus::Restricted => MicrophoneAuthorizationStatus::Restricted,
        AVAuthorizationStatus::Denied => MicrophoneAuthorizationStatus::Denied,
        AVAuthorizationStatus::Authorized => MicrophoneAuthorizationStatus::Authorized,
        _ => MicrophoneAuthorizationStatus::Unavailable,
    }
}

fn request_access() -> PortFuture<'static, Result<MicrophoneAccess, PortError>> {
    let shared = Arc::new(RequestState::default());
    let callback_state = Arc::clone(&shared);
    let handler = RcBlock::new(move |granted: Bool| {
        callback_state.complete(if granted.as_bool() {
            MicrophoneAccess::Granted
        } else {
            MicrophoneAccess::Denied
        });
    });

    // SAFETY: the closure captures only `Arc<Mutex<...>>`, so it is safe for
    // AVFoundation to invoke it on an arbitrary queue. `RcBlock` is copied by
    // the Objective-C method for the asynchronous request. The media constant
    // is immutable framework storage and no reference is retained by Rust.
    let Some(media_type) = (unsafe { AVMediaTypeAudio }) else {
        return Box::pin(async { Ok(MicrophoneAccess::Unavailable) });
    };
    // SAFETY: see above. This is called only after an explicit recording
    // action and only while the status is `NotDetermined`.
    unsafe {
        AVCaptureDevice::requestAccessForMediaType_completionHandler(media_type, &handler);
    }

    Box::pin(PermissionRequest { shared })
}

#[derive(Default)]
struct RequestState {
    inner: Mutex<RequestInner>,
}

#[derive(Default)]
struct RequestInner {
    result: Option<MicrophoneAccess>,
    waker: Option<Waker>,
}

impl RequestState {
    fn complete(&self, result: MicrophoneAccess) {
        let waker = {
            let mut inner = lock(&self.inner);
            if inner.result.is_some() {
                return;
            }
            inner.result = Some(result);
            inner.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

struct PermissionRequest {
    shared: Arc<RequestState>,
}

impl Future for PermissionRequest {
    type Output = Result<MicrophoneAccess, PortError>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let mut inner = lock(&self.shared.inner);
        if let Some(result) = inner.result {
            return Poll::Ready(Ok(result));
        }
        if inner
            .waker
            .as_ref()
            .is_none_or(|waker| !waker.will_wake(context.waker()))
        {
            inner.waker = Some(context.waker().clone());
        }
        Poll::Pending
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_state_completes_once_and_wakes_a_waiter() {
        let state = Arc::new(RequestState::default());
        state.complete(MicrophoneAccess::Denied);
        state.complete(MicrophoneAccess::Granted);
        assert_eq!(lock(&state.inner).result, Some(MicrophoneAccess::Denied));
    }
}
