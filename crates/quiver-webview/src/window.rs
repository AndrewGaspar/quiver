//! The sign-in window: a GTK window holding a WebKit view with an ephemeral (in-memory)
//! website-data manager. It watches each finished load; when one is the redirect page it
//! reads the page's body and takes the code, and when Epic says `null` it loads the
//! sign-in page again.
//!
//! GTK's callbacks are plain C functions, so the window's state is a thread-local (GTK runs
//! on this one thread). Callbacks never call into GTK while holding the state borrowed:
//! closing a window, say, runs the destroy handler before the call returns.

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_int};
use std::ptr;

use crate::args::{Options, Redirect, is_redirect, read_redirect};
use crate::ffi::{self, Api, GAsyncReadyCallback, GCallback, GSourceFunc, LOAD_FINISHED, Ptr, WINDOW_TOPLEVEL};

/// Times Epic may answer `null` (or something other than its JSON) before giving up.
const MAX_RETRIES: u32 = 3;
pub const TITLE: &str = "Quiver — Sign in to Epic Games";

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Code(String),
    Cancelled,
    TimedOut,
    Failed(String),
}

struct State {
    api: &'static Api,
    window: Ptr,
    view: Ptr,
    login: CString,
    prefix: String,
    close_after_load: bool,
    closed: bool,
    reading: bool,
    retries: u32,
    outcome: Option<Outcome>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.borrow_mut().as_mut().map(f))
}

/// Record the outcome (the first one wins) and leave the main loop.
fn finish(o: Outcome) {
    let api = with(|s| {
        if s.outcome.is_none() {
            s.outcome = Some(o);
        }
        s.api
    });
    if let Some(api) = api {
        // SAFETY: called on GTK's thread while gtk_main runs.
        unsafe { (api.gtk_main_quit)() };
    }
}

/// `load-changed (WebKitWebView *view, WebKitLoadEvent event, gpointer)`.
unsafe extern "C" fn on_load_changed(view: Ptr, event: c_int, _: Ptr) {
    if event != LOAD_FINISHED {
        return;
    }
    let Some((api, prefix, close)) = with(|s| (s.api, s.prefix.clone(), s.close_after_load && !s.closed)) else { return };
    // SAFETY: the view is alive (this is its signal); the URI is owned by the view.
    let uri = unsafe { (api.webkit_web_view_get_uri)(view) };
    let uri = if uri.is_null() { String::new() } else { unsafe { CStr::from_ptr(uri) }.to_string_lossy().into_owned() };
    if is_redirect(&uri, &prefix) {
        if with(|s| std::mem::replace(&mut s.reading, true)) == Some(true) {
            return;
        }
        // SAFETY: the main resource belongs to the view; get_data calls back on this thread.
        let res = unsafe { (api.webkit_web_view_get_main_resource)(view) };
        if res.is_null() {
            redirect_read(Redirect::Invalid);
            return;
        }
        unsafe { (api.webkit_web_resource_get_data)(res, ptr::null_mut(), on_data as GAsyncReadyCallback, ptr::null_mut()) };
    } else if close {
        // Tests: close the window as a person would, through the same delete/destroy path.
        let win = with(|s| {
            s.closed = true;
            s.window
        });
        if let Some(w) = win {
            unsafe { (api.gtk_window_close)(w) };
        }
    }
}

/// `webkit_web_resource_get_data`'s callback: the redirect page's bytes.
unsafe extern "C" fn on_data(resource: Ptr, result: Ptr, _: Ptr) {
    let Some(api) = with(|s| s.api) else { return };
    let mut len = 0usize;
    let mut err: Ptr = ptr::null_mut();
    // SAFETY: the resource and result are the ones GIO passed; the data is ours to free.
    let data = unsafe { (api.webkit_web_resource_get_data_finish)(resource, result, &mut len, &mut err) };
    if !err.is_null() {
        unsafe { (api.g_error_free)(err) };
    }
    let r = if data.is_null() {
        Redirect::Invalid
    } else {
        let body = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
        unsafe { (api.g_free)(data.cast()) };
        read_redirect(&body)
    };
    redirect_read(r);
}

fn redirect_read(r: Redirect) {
    match r {
        Redirect::Code(c) => finish(Outcome::Code(c)),
        Redirect::Null | Redirect::Invalid => {
            let again = with(|s| {
                s.reading = false;
                s.retries += 1;
                (s.retries <= MAX_RETRIES).then(|| (s.api, s.view, s.login.clone()))
            })
            .flatten();
            match again {
                Some((api, view, login)) => {
                    eprintln!(
                        "quiver-webview: {}; opening the sign-in page again",
                        if r == Redirect::Null { "Epic's page had no code yet" } else { "Epic's redirect page wasn't what Quiver expected" }
                    );
                    unsafe { (api.webkit_web_view_load_uri)(view, login.as_ptr()) };
                }
                None => finish(Outcome::Failed(format!("Epic's sign-in ended without a code {} times; sign in again, or paste the code instead", MAX_RETRIES + 1))),
            }
        }
    }
}

/// `destroy (GtkWidget *, gpointer)`: the window was closed.
unsafe extern "C" fn on_destroy(_: Ptr, _: Ptr) {
    finish(Outcome::Cancelled);
}

unsafe extern "C" fn on_timeout(_: Ptr) -> c_int {
    finish(Outcome::TimedOut);
    0 // G_SOURCE_REMOVE
}

unsafe extern "C" fn on_signal(_: Ptr) -> c_int {
    finish(Outcome::Cancelled);
    0
}

fn connect(api: &Api, obj: Ptr, signal: &str, cb: GCallback) {
    let name = ffi::cstring(signal);
    // SAFETY: obj is a live GObject with this signal; cb has the signal's signature.
    unsafe { (api.g_signal_connect_data)(obj, name.as_ptr(), cb, ptr::null_mut(), ptr::null_mut(), 0) };
}

/// Why the window couldn't open at all (no display): `Err` is for exit code 4.
pub fn run(api: &'static Api, o: &Options) -> Result<Outcome, String> {
    // SAFETY: every call below is on this (GTK's) thread, with objects created here.
    unsafe {
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            (api.g_unix_signal_add)(sig, on_signal as GSourceFunc, ptr::null_mut());
        }
        if (api.gtk_init_check)(ptr::null_mut(), ptr::null_mut()) == 0 {
            return Err("there is no display to open the sign-in window on".into());
        }
        let data = (api.webkit_website_data_manager_new_ephemeral)();
        let context = (api.webkit_web_context_new_with_website_data_manager)(data);
        let view = (api.webkit_web_view_new_with_context)(context);
        let window = (api.gtk_window_new)(WINDOW_TOPLEVEL);
        if data.is_null() || context.is_null() || view.is_null() || window.is_null() {
            return Err("WebKitGTK could not make a web view".into());
        }
        let title = ffi::cstring(TITLE);
        (api.gtk_window_set_title)(window, title.as_ptr());
        (api.gtk_window_set_default_size)(window, 560, 780);
        (api.gtk_container_add)(window, view);
        STATE.with(|s| {
            *s.borrow_mut() = Some(State {
                api,
                window,
                view,
                login: ffi::cstring(&o.url),
                prefix: o.redirect_prefix.clone(),
                close_after_load: o.close_after_load,
                closed: false,
                reading: false,
                retries: 0,
                outcome: None,
            })
        });
        let load_changed: unsafe extern "C" fn(Ptr, c_int, Ptr) = on_load_changed;
        let destroy: unsafe extern "C" fn(Ptr, Ptr) = on_destroy;
        connect(api, view, "load-changed", std::mem::transmute::<unsafe extern "C" fn(Ptr, c_int, Ptr), GCallback>(load_changed));
        connect(api, window, "destroy", std::mem::transmute::<unsafe extern "C" fn(Ptr, Ptr), GCallback>(destroy));
        (api.g_timeout_add_seconds)(o.timeout, on_timeout as GSourceFunc, ptr::null_mut());
        (api.gtk_widget_show_all)(window);
        let login = ffi::cstring(&o.url);
        (api.webkit_web_view_load_uri)(view, login.as_ptr());
        (api.gtk_main)();
    }
    let outcome = STATE.with(|s| s.borrow_mut().as_mut().and_then(|s| s.outcome.take()));
    Ok(outcome.unwrap_or(Outcome::Cancelled))
}
