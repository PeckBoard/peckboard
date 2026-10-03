//! iOS cold-launch deep links. tao 0.37 ignores the scene connection
//! options, so a `peckboard://` URL that *launches* the app never becomes a
//! `RunEvent::Opened` (URLs opened while running do). [`install`] wraps
//! tao's `scene:willConnectToSession:options:` to stash the options' URLs
//! first; tao then runs the app's `setup` inside that same call, which
//! drains them with [`take`].

use std::ffi::{CStr, c_char};
use std::sync::{Mutex, OnceLock};

use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{msg_send, sel};

type WillConnect = unsafe extern "C-unwind" fn(
    *mut AnyObject,
    Sel,
    *mut AnyObject,
    *mut AnyObject,
    *mut AnyObject,
);

static ORIGINAL: OnceLock<WillConnect> = OnceLock::new();
static URLS: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// What happened, for the debug log (the logger starts after `install`).
static STATUS: Mutex<&'static str> = Mutex::new("not installed");

/// Call after `tauri::Builder::build` (which registers tao's scene
/// delegate class) and before `App::run` (which connects the first scene).
pub fn install() {
    let Some(cls) = AnyClass::get(c"TaoSceneDelegate") else {
        *STATUS.lock().unwrap() = "TaoSceneDelegate not found";
        return;
    };
    let Some(method) = cls.instance_method(sel!(scene:willConnectToSession:options:)) else {
        *STATUS.lock().unwrap() = "scene:willConnectToSession:options: not found";
        return;
    };
    // SAFETY: same signature as tao's implementation; the original is kept
    // and always called.
    unsafe {
        let ours: Imp = std::mem::transmute::<WillConnect, Imp>(will_connect);
        let old = method.set_implementation(ours);
        let _ = ORIGINAL.set(std::mem::transmute::<Imp, WillConnect>(old));
    }
    *STATUS.lock().unwrap() = "installed";
}

pub fn status() -> &'static str {
    *STATUS.lock().unwrap()
}

/// URLs the app was launched with (once).
pub fn take() -> Vec<String> {
    std::mem::take(&mut *URLS.lock().unwrap())
}

unsafe extern "C-unwind" fn will_connect(
    this: *mut AnyObject,
    sel: Sel,
    scene: *mut AnyObject,
    session: *mut AnyObject,
    options: *mut AnyObject,
) {
    // SAFETY: `options` is a UISceneConnectionOptions; URLContexts is an
    // NSSet<UIOpenURLContext> (possibly nil — messaging nil returns nil/0).
    unsafe {
        *STATUS.lock().unwrap() = "hook ran";
        if !options.is_null() {
            let contexts: *mut AnyObject = msg_send![options, URLContexts];
            let all: *mut AnyObject = if contexts.is_null() {
                std::ptr::null_mut()
            } else {
                msg_send![contexts, allObjects]
            };
            let n: usize = if all.is_null() {
                0
            } else {
                msg_send![all, count]
            };
            let mut urls = URLS.lock().unwrap();
            for i in 0..n {
                let ctx: *mut AnyObject = msg_send![all, objectAtIndex: i];
                let url: *mut AnyObject = msg_send![ctx, URL];
                if url.is_null() {
                    continue;
                }
                let s: *mut AnyObject = msg_send![url, absoluteString];
                if s.is_null() {
                    continue;
                }
                let p: *const c_char = msg_send![s, UTF8String];
                if !p.is_null() {
                    urls.push(CStr::from_ptr(p).to_string_lossy().into_owned());
                }
            }
        }
        if let Some(orig) = ORIGINAL.get() {
            orig(this, sel, scene, session, options);
        }
    }
}
