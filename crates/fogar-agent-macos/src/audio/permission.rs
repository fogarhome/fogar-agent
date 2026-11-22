//! Microphone permission (TCC) through `AVCaptureDevice` (AVFoundation), by Objective-C message.
//!
//! - `status()` = `+[AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio]`, which
//!   never prompts (AVCaptureDevice.h). Anything unreadable maps to `NotDetermined` (fail closed;
//!   the port forbids `Unknown` on a platform with a permission model).
//! - `request()` = `+requestAccessForMediaType:completionHandler:`, called ONLY when the status
//!   is `NotDetermined` (a decided status is answered at once without touching TCC). It may show
//!   the system prompt; per the S2 spike TCC attributes it to the app bundle ("Fogar Desktop"),
//!   which must carry `NSMicrophoneUsageDescription` and `com.apple.security.device.audio-input`.
//!   A binary without the usage string is killed by TCC when it asks, so `request` checks the
//!   main bundle's Info.plist first and returns an error instead of asking.

use fogar_ports::{
    audio::{AudioError, MicPermission},
    events::EventSink,
};
use objc2::{
    msg_send,
    rc::{Retained, autoreleasepool},
    runtime::{AnyClass, AnyObject, Bool},
};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Mutex,
};

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

/// What the completion handler captures. It runs on an arbitrary dispatch queue, so it must be
/// Send + Sync (checked at compile time below).
type Slot = Mutex<Option<EventSink<MicPermission>>>;
const _: fn() = || {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Slot>();
};

/// `NSMicrophoneUsageDescription` is in the main bundle's Info.plist. Without it TCC KILLS the
/// process that asks for the microphone, so `request` refuses instead of asking.
fn has_usage_description() -> bool {
    autoreleasepool(|_| {
        let (Some(bundle_cls), Some(string_cls)) =
            (AnyClass::get(c"NSBundle"), AnyClass::get(c"NSString"))
        else {
            return false;
        };
        // SAFETY: `+[NSBundle mainBundle]` takes nothing and returns an NSBundle (or nil); objc2
        // retains the autoreleased (+0) result into `Retained` and checks for nil via `Option`.
        let bundle: Option<Retained<AnyObject>> = unsafe { msg_send![bundle_cls, mainBundle] };
        let Some(bundle) = bundle else {
            return false;
        };
        // SAFETY: `+[NSString stringWithUTF8String:]` takes a NUL-terminated UTF-8 C string; the
        // literal is 'static and NUL-terminated. Result: autoreleased NSString or nil.
        let key: Option<Retained<AnyObject>> = unsafe {
            msg_send![string_cls, stringWithUTF8String: c"NSMicrophoneUsageDescription".as_ptr()]
        };
        let Some(key) = key else {
            return false;
        };
        // SAFETY: `-[NSBundle objectForInfoDictionaryKey:]` takes an NSString key (`key`, alive
        // for the call) on a live NSBundle (`bundle`) and returns an object or nil (+0).
        let value: Option<Retained<AnyObject>> =
            unsafe { msg_send![&*bundle, objectForInfoDictionaryKey: &*key] };
        value.is_some()
    })
}

/// The completion handler's work: one answer, then the sink is dropped.
fn answer(slot: &Slot, granted: Bool) {
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
}

/// Answers on `sink` exactly once, then drops it (the stream then yields `None`). Errors only
/// when the permission is undecided and the process cannot ask (no usage description).
pub(crate) fn request(sink: EventSink<MicPermission>) -> Result<(), AudioError> {
    let current = status();
    let cls = match (current, capture_device_class()) {
        (MicPermission::NotDetermined, Some(cls)) => cls,
        _ => {
            // Already decided (or AVFoundation missing): answer without touching TCC.
            let _ = sink.push(current);
            return Ok(());
        }
    };
    if !has_usage_description() {
        return Err(AudioError::Backend(
            "no NSMicrophoneUsageDescription in the main bundle: asking would get the agent killed"
                .into(),
        ));
    }
    let slot: Slot = Mutex::new(Some(sink));
    let handler = block2::RcBlock::new(move |granted: Bool| {
        // Never unwind into Objective-C (UB): a panic here aborts the agent instead.
        if catch_unwind(AssertUnwindSafe(|| answer(&slot, granted))).is_err() {
            std::process::abort();
        }
    });
    let media = media_type_audio();
    // SAFETY: `+requestAccessForMediaType:completionHandler:` (macOS 10.14+) takes an
    // `AVMediaType` and a `void (^)(BOOL)` block and returns void. AVFoundation copies the block
    // (Block_copy) before returning, so our reference only has to live for the call; the copy
    // keeps the closure (and the sink inside it) alive until it has run. Everything the closure
    // captures is Send + Sync (asserted above), as it runs on another thread.
    let _: () =
        unsafe { msg_send![cls, requestAccessForMediaType: media, completionHandler: &*handler] };
    Ok(())
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

    /// A test binary is not a bundle: no usage description, so an undecided permission is never
    /// requested from here (TCC would kill the process).
    #[test]
    fn a_bare_binary_has_no_usage_description() {
        assert!(!has_usage_description());
    }

    #[test]
    fn the_handler_answers_once_and_fails_closed() {
        let (sink, mut stream) = fogar_ports::events::event_channel(std::num::NonZeroUsize::MIN);
        let slot: Slot = Mutex::new(Some(sink));
        answer(&slot, Bool::YES);
        answer(&slot, Bool::NO);
        drop(slot);
        assert_eq!(stream.try_next(), Some(MicPermission::Granted));
        assert_eq!(stream.try_next(), None);
        let (sink, mut stream) = fogar_ports::events::event_channel(std::num::NonZeroUsize::MIN);
        answer(&Mutex::new(Some(sink)), Bool::NO);
        let got = stream.try_next();
        assert!(matches!(
            got,
            Some(MicPermission::Denied | MicPermission::Restricted)
        ));
    }
}
