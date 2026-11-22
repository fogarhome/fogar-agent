//! Microphone permission (TCC) through `AVCaptureDevice` (AVFoundation), by Objective-C message.
//!
//! - `status()` = `+[AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio]`, which
//!   never prompts (AVCaptureDevice.h). Anything unreadable maps to `NotDetermined` (fail closed;
//!   the port forbids `Unknown` on a platform with a permission model).
//! - `request()` = `+requestAccessForMediaType:completionHandler:`, called ONLY when the status
//!   is `NotDetermined` (a decided status is answered at once without touching TCC). It may show
//!   the system prompt; per the S2 spike TCC attributes it to the app bundle ("Fogar Desktop"),
//!   which must carry `NSMicrophoneUsageDescription` and `com.apple.security.device.audio-input`.
//!   A binary without the usage string is killed by TCC when it asks: that is why the contract
//!   suite never requests an undecided permission.

use fogar_ports::{audio::MicPermission, events::EventSink};
use objc2::{
    msg_send,
    runtime::{AnyClass, AnyObject, Bool},
};
use std::sync::Mutex;

#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {
    /// `AVMediaType const AVMediaTypeAudio` (an immutable `NSString *`), AVMediaFormat.h.
    static AVMediaTypeAudio: &'static AnyObject;
}

/// `AVAuthorizationStatus` (NSInteger), AVCaptureDevice.h.
fn map_status(raw: isize) -> MicPermission {
    match raw {
        3 => MicPermission::Granted,
        2 => MicPermission::Denied,
        1 => MicPermission::Restricted,
        _ => MicPermission::NotDetermined,
    }
}

fn capture_device_class() -> Option<&'static AnyClass> {
    AnyClass::get(c"AVCaptureDevice")
}

fn media_type_audio() -> &'static AnyObject {
    // SAFETY: reading an immutable extern static. AVFoundation is linked above, so the symbol
    // resolves at load time to a valid, never-freed NSString constant (lifetime 'static).
    unsafe { AVMediaTypeAudio }
}

/// Current microphone permission. Never prompts.
pub(crate) fn status() -> MicPermission {
    let Some(cls) = capture_device_class() else {
        return MicPermission::NotDetermined;
    };
    let media = media_type_audio();
    // SAFETY: `+[AVCaptureDevice authorizationStatusForMediaType:]` (macOS 10.14+) takes an
    // `AVMediaType` (NSString *) and returns `AVAuthorizationStatus` (NSInteger = isize). `cls` is
    // the real AVCaptureDevice class (looked up by name, not assumed), `media` a valid NSString.
    // objc2 checks the argument/return encodings against the method in debug builds.
    let raw: isize = unsafe { msg_send![cls, authorizationStatusForMediaType: media] };
    map_status(raw)
}

/// Answers on `sink` exactly once, then drops it (the stream then yields `None`).
pub(crate) fn request(sink: EventSink<MicPermission>) {
    let current = status();
    let cls = match (current, capture_device_class()) {
        (MicPermission::NotDetermined, Some(cls)) => cls,
        _ => {
            // Already decided (or AVFoundation missing): answer without touching TCC.
            let _ = sink.push(current);
            return;
        }
    };
    // The handler runs once, on an arbitrary dispatch queue: hand the sink over through a Mutex
    // (Send + Sync), take it on the first call. Nothing in it can panic (no unwinding into ObjC).
    let slot = Mutex::new(Some(sink));
    let handler = block2::RcBlock::new(move |granted: Bool| {
        let taken = match slot.lock() {
            Ok(mut g) => g.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(sink) = taken {
            let answer = if granted.as_bool() {
                MicPermission::Granted
            } else {
                // Not granted: report what TCC now says, never `Granted`/`NotDetermined`.
                match status() {
                    MicPermission::Restricted => MicPermission::Restricted,
                    _ => MicPermission::Denied,
                }
            };
            let _ = sink.push(answer);
        }
    });
    let media = media_type_audio();
    // SAFETY: `+requestAccessForMediaType:completionHandler:` (macOS 10.14+) takes an
    // `AVMediaType` and a `void (^)(BOOL)` block and returns void. AVFoundation copies the block
    // (Block_copy) before returning, so our reference only has to live for the call; the copy
    // keeps the closure (and the sink inside it) alive until it has run. Everything the closure
    // captures is Send + Sync, as it runs on another thread.
    let _: () =
        unsafe { msg_send![cls, requestAccessForMediaType: media, completionHandler: &*handler] };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_values_map_fail_closed() {
        assert_eq!(map_status(3), MicPermission::Granted);
        assert_eq!(map_status(2), MicPermission::Denied);
        assert_eq!(map_status(1), MicPermission::Restricted);
        assert_eq!(map_status(0), MicPermission::NotDetermined);
        assert_eq!(map_status(42), MicPermission::NotDetermined);
        assert_eq!(map_status(-1), MicPermission::NotDetermined);
    }

    #[test]
    fn the_class_and_constant_resolve_and_status_never_prompts() {
        assert!(capture_device_class().is_some(), "AVFoundation not linked");
        // Never `Unknown` on macOS; reading it twice gives the same answer (no prompt happened).
        let s = status();
        assert_ne!(s, MicPermission::Unknown);
        assert_eq!(s, status());
    }
}
