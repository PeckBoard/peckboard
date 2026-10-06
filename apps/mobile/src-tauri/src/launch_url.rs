//! iOS deep links that tao drops.
//!
//! - Cold launch: tao 0.37 ignores the scene connection options, so a
//!   `peckboard://` URL (`URLContexts`) or a Universal Link
//!   (`userActivities`, `https://peckboard.com/pair#…`) that *launches* the
//!   app never becomes a `RunEvent::Opened`.
//! - Warm Universal Link: iOS hands a running app the link through
//!   `scene:continueUserActivity:`, which tao doesn't implement, so it is
//!   dropped too (custom-scheme URLs opened while running do arrive).
//!
//! [`install`] wraps tao's `scene:willConnectToSession:options:` and adds (or
//! wraps) `scene:continueUserActivity:` to stash those URLs; the app's run
//! loop drains them with [`take`].

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
type ContinueActivity =
    unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject, *mut AnyObject);

static ORIGINAL: OnceLock<WillConnect> = OnceLock::new();
static ORIGINAL_CONTINUE: OnceLock<ContinueActivity> = OnceLock::new();
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
    let continue_sel = sel!(scene:continueUserActivity:);
    // SAFETY: `continue_user_activity` matches the UISceneDelegate signature
    // `- (void)scene:(UIScene *)s continueUserActivity:(NSUserActivity *)a`
    // ("v@:@@"); an existing implementation is kept and called.
    unsafe {
        let ours: Imp = std::mem::transmute::<ContinueActivity, Imp>(continue_user_activity);
        match cls.instance_method(continue_sel) {
            Some(m) => {
                let old = m.set_implementation(ours);
                let _ = ORIGINAL_CONTINUE.set(std::mem::transmute::<Imp, ContinueActivity>(old));
            }
            None => {
                let added = objc2::ffi::class_addMethod(
                    cls as *const AnyClass as *mut AnyClass,
                    continue_sel,
                    ours,
                    c"v@:@@".as_ptr(),
                );
                if !added.as_bool() {
                    *STATUS.lock().unwrap() = "installed (continueUserActivity not added)";
                    return;
                }
            }
        }
    }
    *STATUS.lock().unwrap() = "installed";
}

pub fn status() -> &'static str {
    *STATUS.lock().unwrap()
}

/// URLs received outside tao (once each).
pub fn take() -> Vec<String> {
    std::mem::take(&mut *URLS.lock().unwrap())
}

/// `-[NSURL absoluteString]` as a Rust string.
///
/// # Safety
/// `url` is nil or an NSURL.
unsafe fn url_string(url: *mut AnyObject) -> Option<String> {
    if url.is_null() {
        return None;
    }
    unsafe {
        let s: *mut AnyObject = msg_send![url, absoluteString];
        if s.is_null() {
            return None;
        }
        let p: *const c_char = msg_send![s, UTF8String];
        (!p.is_null()).then(|| CStr::from_ptr(p).to_string_lossy().into_owned())
    }
}

/// Every object of an NSSet (nil-safe).
///
/// # Safety
/// `set` is nil or an NSSet.
unsafe fn set_objects(set: *mut AnyObject) -> Vec<*mut AnyObject> {
    if set.is_null() {
        return Vec::new();
    }
    unsafe {
        let all: *mut AnyObject = msg_send![set, allObjects];
        if all.is_null() {
            return Vec::new();
        }
        let n: usize = msg_send![all, count];
        (0..n).map(|i| msg_send![all, objectAtIndex: i]).collect()
    }
}

/// A Universal Link's URL (`NSUserActivity.webpageURL`), if any.
///
/// # Safety
/// `activity` is nil or an NSUserActivity.
unsafe fn activity_url(activity: *mut AnyObject) -> Option<String> {
    if activity.is_null() {
        return None;
    }
    unsafe {
        let url: *mut AnyObject = msg_send![activity, webpageURL];
        url_string(url)
    }
}

unsafe extern "C-unwind" fn will_connect(
    this: *mut AnyObject,
    sel: Sel,
    scene: *mut AnyObject,
    session: *mut AnyObject,
    options: *mut AnyObject,
) {
    // SAFETY: `options` is a UISceneConnectionOptions: URLContexts is an
    // NSSet<UIOpenURLContext>, userActivities an NSSet<NSUserActivity>
    // (either possibly nil — messaging nil returns nil/0).
    unsafe {
        *STATUS.lock().unwrap() = "hook ran";
        if !options.is_null() {
            let contexts: *mut AnyObject = msg_send![options, URLContexts];
            let activities: *mut AnyObject = msg_send![options, userActivities];
            let mut urls = URLS.lock().unwrap();
            for ctx in set_objects(contexts) {
                let url: *mut AnyObject = msg_send![ctx, URL];
                urls.extend(url_string(url));
            }
            for a in set_objects(activities) {
                urls.extend(activity_url(a));
            }
        }
        if let Some(orig) = ORIGINAL.get() {
            orig(this, sel, scene, session, options);
        }
    }
}

unsafe extern "C-unwind" fn continue_user_activity(
    this: *mut AnyObject,
    sel: Sel,
    scene: *mut AnyObject,
    activity: *mut AnyObject,
) {
    // SAFETY: `activity` is the NSUserActivity iOS hands the scene delegate.
    unsafe {
        if let Some(u) = activity_url(activity) {
            URLS.lock().unwrap().push(u);
        }
        if let Some(orig) = ORIGINAL_CONTINUE.get() {
            orig(this, sel, scene, activity);
        }
    }
}
