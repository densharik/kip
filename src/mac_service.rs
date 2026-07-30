//! macOS AppKit glue.
//!
//! Finder Services integration: "New kip Window Here". An `NSServices` entry in
//! Info.plist adds the item to Finder's Services submenu for files and folders;
//! macOS routes the pick to the provider registered here, which queues the
//! chosen folder (a file's containing folder) for the egui loop to open as a new
//! session - the same way Warp/iTerm expose "New Window Here".
//!
//! Plus the Cmd+V monitor, see `hook_paste`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{define_class, msg_send, AnyThread, MainThreadMarker};
use objc2_app_kit::{
    NSApplication, NSEvent, NSEventMask, NSEventModifierFlags, NSPasteboard, NSPasteboardType,
    NSPasteboardTypeString, NSUpdateDynamicServices,
};
use objc2_foundation::NSString;

/// Folders picked via the Service, waiting for the UI loop to open them.
static PENDING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
/// egui context, so the ObjC callback can wake the (possibly idle) UI.
static CTX: OnceLock<egui::Context> = OnceLock::new();

define_class!(
    // SAFETY: NSObject has no subclassing requirements; the class holds no
    // state and has no Drop impl.
    #[unsafe(super(NSObject))]
    #[name = "KipServiceProvider"]
    struct Provider;

    impl Provider {
        // Selector openInKip:userData:error: - the NSMessage in Info.plist.
        #[unsafe(method(openInKip:userData:error:))]
        fn open_in_kip(
            &self,
            pboard: &NSPasteboard,
            _user_data: *mut AnyObject,
            _error: *mut *mut AnyObject,
        ) {
            handle(pboard);
        }
    }
);

impl Provider {
    fn new() -> Retained<Self> {
        unsafe { msg_send![Self::alloc(), init] }
    }
}

/// Read the selected paths off the service pasteboard and queue a directory for
/// each: a folder as-is, otherwise a file's containing folder (the "...Here"
/// behaviour of Warp/iTerm). Duplicate folders collapse to one session.
fn handle(pboard: &NSPasteboard) {
    let mut raw: Vec<String> = Vec::new();
    #[allow(deprecated)]
    let files_ty: &NSPasteboardType = unsafe { objc2_app_kit::NSFilenamesPboardType };
    // Finder selections arrive as a filenames array; read it via raw message
    // sends to avoid pinning the generic element type.
    if let Some(list) = pboard.propertyListForType(files_ty) {
        let count: usize = unsafe { msg_send![&*list, count] };
        for i in 0..count {
            let s: Retained<NSString> = unsafe { msg_send![&*list, objectAtIndex: i] };
            raw.push(s.to_string());
        }
    }
    // Fallback: a plain-text selection that is itself a path.
    if raw.is_empty() {
        if let Some(s) = pboard.stringForType(unsafe { NSPasteboardTypeString }) {
            raw.push(s.to_string());
        }
    }

    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut seen = HashSet::new();
    for r in raw {
        let p = PathBuf::from(r.trim());
        let dir = if p.is_dir() {
            Some(p)
        } else {
            p.parent().filter(|d| d.is_dir()).map(Path::to_path_buf)
        };
        if let Some(d) = dir {
            if seen.insert(d.clone()) {
                dirs.push(d);
            }
        }
    }
    if dirs.is_empty() {
        return;
    }
    if let Ok(mut q) = PENDING.lock() {
        q.extend(dirs);
    }
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint();
    }
}

/// Register the Services provider with the running NSApp. Call once, on the
/// main thread, during startup.
pub fn register(ctx: egui::Context) {
    let _ = CTX.set(ctx);
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let provider = Provider::new();
    let obj: &AnyObject = &provider;
    unsafe { app.setServicesProvider(Some(obj)) };
    // The app keeps only a weak reference to the provider; leak ours so the
    // object lives for the whole process.
    std::mem::forget(provider);
    // Pick up the Info.plist NSServices entry now instead of after a relogin.
    NSUpdateDynamicServices();
}

/// Cmd+V seen since the last `take_paste`.
static PASTE: AtomicBool = AtomicBool::new(false);

/// Watch Cmd+V ourselves. egui answers the shortcut by reading the clipboard as
/// text and pushes no event at all when there is none - which is exactly the
/// case for a copied Finder file or a screenshot (those pasteboards carry no
/// string type), so the app never learned the user pasted. The monitor sees the
/// keystroke regardless of what the clipboard holds; the UI loop then goes to
/// the pasteboard for a path (`plat::clipboard_paths`). Main thread, once.
pub fn hook_paste() {
    // kVK_ANSI_V. The code is the physical key, so this also fires in non-latin
    // layouts - matching how egui resolves the paste shortcut for text.
    const KEY_V: u16 = 9;
    let block = RcBlock::new(|ev: NonNull<NSEvent>| -> *mut NSEvent {
        let e = unsafe { ev.as_ref() };
        if e.keyCode() == KEY_V && e.modifierFlags().contains(NSEventModifierFlags::Command) {
            PASTE.store(true, Ordering::Relaxed);
            if let Some(ctx) = CTX.get() {
                ctx.request_repaint();
            }
        }
        // Hand the event back untouched: a text paste is still egui's job.
        ev.as_ptr()
    });
    // SAFETY: the block returns the event it was given, which is a valid pointer.
    unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &block) };
    // The monitor is never removed, so the block has to outlive this call.
    std::mem::forget(block);
}

/// Cmd+V arrived since the last call.
pub fn take_paste() -> bool {
    PASTE.swap(false, Ordering::Relaxed)
}

/// Folders queued by the Service since the last call, for the UI loop to open.
pub fn take_pending() -> Vec<PathBuf> {
    PENDING
        .lock()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default()
}
