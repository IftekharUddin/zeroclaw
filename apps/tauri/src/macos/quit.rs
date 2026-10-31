//! Route ordinary Dock Quit through the same explicit Quit decision as menus.
//! The Apple-event registration owns only a callback target, never process or
//! preference state. System logout, restart and shutdown keep Cocoa's normal
//! termination path and do not acquire new background-process authority.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::NSApplication;
use objc2_foundation::{MainThreadMarker, NSAppleEventDescriptor, NSAppleEventManager, NSObject};
use tauri::{AppHandle, Runtime};

const CORE_EVENT: u32 = u32::from_be_bytes(*b"aevt");
const QUIT_EVENT: u32 = u32::from_be_bytes(*b"quit");
const QUIT_REASON: u32 = u32::from_be_bytes(*b"why?");

struct QuitHandlerIvars {
    request: Box<dyn Fn()>,
}

define_class!(
    // SAFETY: NSObject imposes no subclassing requirements. The target and
    // its callback are confined to the main thread and have no custom Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = QuitHandlerIvars]
    #[name = "ZeroClawDesktopQuitHandler"]
    struct QuitHandler;

    impl QuitHandler {
        // SAFETY: NSAppleEventManager requires a void-returning selector with
        // two NSAppleEventDescriptor object arguments (event and reply).
        #[unsafe(method(handleQuit:withReplyEvent:))]
        fn handle_quit(&self, event: &NSAppleEventDescriptor, _reply: &NSAppleEventDescriptor) {
            if is_system_termination(event) {
                // Installing an event handler replaces the previous handler.
                // Use public Cocoa termination, not an assumed private handler.
                // Tao's existing delegate retains its normal system-exit path.
                NSApplication::sharedApplication(self.mtm()).terminate(None);
            } else {
                (self.ivars().request)();
            }
        }
    }
);

impl QuitHandler {
    fn new(mtm: MainThreadMarker, request: Box<dyn Fn()>) -> Retained<Self> {
        let target = Self::alloc(mtm).set_ivars(QuitHandlerIvars { request });
        // SAFETY: NSObject's parameter-free init returns this initialized target.
        unsafe { msg_send![super(target), init] }
    }
}

thread_local! {
    // Own the target for the whole app/main-thread lifetime. Never release it
    // while the native event manager can dispatch to its registered selector.
    static QUIT_HANDLER: RefCell<Option<Retained<QuitHandler>>> = const { RefCell::new(None) };
}

/// Install during Tauri setup on the main thread, after AppKit has registered
/// its default event handlers. The closure resolves the normal exit controller
/// through AppHandle at use time; it does not snapshot a choice or owned work.
pub fn install<R: Runtime>(app: &AppHandle<R>) -> std::io::Result<()> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| std::io::Error::other("Dock Quit must be installed on the main thread"))?;
    QUIT_HANDLER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_some() {
            return Err(std::io::Error::other("Dock Quit is already registered"));
        }
        let app = app.clone();
        let target = QuitHandler::new(mtm, Box::new(move || app.exit(0)));
        // SAFETY: target implements the selector with the exact documented
        // two-object/void ABI, and the retained slot keeps it alive for dispatch.
        unsafe {
            NSAppleEventManager::sharedAppleEventManager()
                .setEventHandler_andSelector_forEventClass_andEventID(
                    &target,
                    sel!(handleQuit:withReplyEvent:),
                    CORE_EVENT,
                    QUIT_EVENT,
                );
        }
        *slot = Some(target);
        Ok(())
    })
}

fn is_system_termination(event: &NSAppleEventDescriptor) -> bool {
    let Some(reason) = event.paramDescriptorForKeyword(QUIT_REASON) else {
        return false;
    };
    // AERegistry.h documents these four values for the optional 'why?' key.
    // Force Quit and signals do not pass through this Apple-event handler.
    [*b"quia", *b"shut", *b"rest", *b"rlgo"]
        .into_iter()
        .any(|code| reason.typeCodeValue() == u32::from_be_bytes(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dock_quit_and_system_quit_have_distinct_native_event_routes() {
        let event = NSAppleEventDescriptor::appleEventWithEventClass_eventID_targetDescriptor_returnID_transactionID(
            CORE_EVENT, QUIT_EVENT, None, -1, 0,
        );
        assert!(!is_system_termination(&event));
        for reason in [*b"quia", *b"shut", *b"rest", *b"rlgo"] {
            event.setParamDescriptor_forKeyword(
                &NSAppleEventDescriptor::descriptorWithTypeCode(u32::from_be_bytes(reason)),
                QUIT_REASON,
            );
            assert!(is_system_termination(&event));
        }
        event.setParamDescriptor_forKeyword(
            &NSAppleEventDescriptor::descriptorWithTypeCode(u32::from_be_bytes(*b"????")),
            QUIT_REASON,
        );
        assert!(!is_system_termination(&event));
    }
}
