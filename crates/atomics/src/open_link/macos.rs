//! macOS hands `atomics://` links to an app as an Apple event (get URL), not as arguments. A
//! handler registered with the event manager queues them, and the app opens them on its next tick.

use std::ffi::{CStr, c_char};
use std::sync::Mutex;

use objc2::runtime::{AnyObject, NSObject};
use objc2::{AllocAnyThread, ClassType, class, define_class, msg_send, sel};

/// 'GURL': the class and id of the get-URL event.
const GET_URL: u32 = 0x4755_524C;
/// '----': the keyword of an event's direct parameter, here the URL.
const DIRECT_OBJECT: u32 = 0x2D2D_2D2D;

static INBOX: Mutex<Vec<String>> = Mutex::new(Vec::new());
static WAKE: Mutex<Option<egui::Context>> = Mutex::new(None);

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "AtomicsLinkHandler"]
    struct LinkHandler;

    impl LinkHandler {
        #[unsafe(method(handleURL:withReplyEvent:))]
        fn handle_url(&self, event: &AnyObject, _reply: &AnyObject) {
            if let Some(url) = url_of(event) {
                INBOX.lock().unwrap().push(url);
                if let Some(ctx) = WAKE.lock().unwrap().as_ref() {
                    ctx.request_repaint();
                }
            }
        }
    }
);

fn url_of(event: &AnyObject) -> Option<String> {
    // SAFETY: the event is an NSAppleEventDescriptor; both messages exist on it and return objects or nil.
    unsafe {
        let parameter: Option<&AnyObject> = msg_send![event, paramDescriptorForKeyword: DIRECT_OBJECT];
        let text: Option<&AnyObject> = msg_send![parameter?, stringValue];
        let bytes: *const c_char = msg_send![text?, UTF8String];
        (!bytes.is_null()).then(|| CStr::from_ptr(bytes).to_string_lossy().into_owned())
    }
}

/// Starts queueing links. Call once the application has launched (from the window's creation
/// callback), so that the system's own handler is already in place and this one replaces it.
pub fn register(ctx: &egui::Context) {
    *WAKE.lock().unwrap() = Some(ctx.clone());
    let handler: objc2::rc::Retained<LinkHandler> = unsafe { msg_send![LinkHandler::alloc(), init] };
    // SAFETY: the manager is a singleton; the selector matches the handler's method.
    unsafe {
        let manager: *mut AnyObject = msg_send![class!(NSAppleEventManager), sharedAppleEventManager];
        let _: () = msg_send![
            manager,
            setEventHandler: &*handler,
            andSelector: sel!(handleURL:withReplyEvent:),
            forEventClass: GET_URL,
            andEventID: GET_URL
        ];
    }
    // The event manager does not retain its handler.
    std::mem::forget(handler);
}

/// The links that arrived since the last call.
pub fn take() -> Vec<String> {
    std::mem::take(&mut *INBOX.lock().unwrap())
}
