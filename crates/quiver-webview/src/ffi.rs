//! The few GTK 3, GLib and WebKitGTK (`webkit2gtk-4.1`) functions the sign-in window uses,
//! opened at run time with `dlopen`.
//!
//! Nothing links WebKitGTK at build time, so Quiver builds (and its package installs)
//! without it, and the window is there whenever `libwebkit2gtk-4.1.so.0` is. `dlsym`
//! through the WebKit handle also finds GTK's, GLib's and GObject's symbols, because those
//! are WebKit's own dependencies and are searched with it.
//!
//! Every pointer type is opaque (`*mut c_void`); the signatures are the C API's, from the
//! WebKitGTK 2.x and GTK 3 reference. Only these functions are called.

use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_ulong, c_void};

pub type Ptr = *mut c_void;
/// `void (*)(void)`: GLib's untyped callback, cast to each signal's real signature.
pub type GCallback = unsafe extern "C" fn();
/// `gboolean (*)(gpointer)`.
pub type GSourceFunc = unsafe extern "C" fn(Ptr) -> c_int;
/// `void (*)(GObject *, GAsyncResult *, gpointer)`.
pub type GAsyncReadyCallback = unsafe extern "C" fn(Ptr, Ptr, Ptr);

/// `WebKitLoadEvent`.
pub const LOAD_FINISHED: c_int = 3;
/// `GTK_WINDOW_TOPLEVEL`.
pub const WINDOW_TOPLEVEL: c_int = 0;

/// The library to open (the window's only run-time dependency).
pub const WEBKIT_LIB: &str = "libwebkit2gtk-4.1.so.0";

macro_rules! api {
    ($($name:ident : fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        #[allow(non_snake_case)]
        pub struct Api {
            $(pub $name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*
        }

        impl Api {
            /// Open `lib` and look every function up. The error is one line for people.
            pub fn load(lib: &str) -> Result<Api, String> {
                let name = CString::new(lib).map_err(|_| "bad library name".to_owned())?;
                // SAFETY: dlopen with a valid C string; the handle is never closed, so the
                // symbols stay valid for the life of the process.
                let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
                if handle.is_null() {
                    return Err(format!("{lib} could not be loaded ({})", dlerror()));
                }
                Ok(Api {
                    $($name: {
                        let sym = concat!(stringify!($name), "\0");
                        // SAFETY: a NUL-terminated name.
                        let p = unsafe { libc::dlsym(handle, sym.as_ptr().cast()) };
                        if p.is_null() {
                            return Err(format!("{lib} has no {}", stringify!($name)));
                        }
                        // SAFETY: the symbol is the C function with this signature (see the
                        // module docs); a data pointer and a function pointer have the same
                        // size on every platform Quiver builds for.
                        unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($arg),*) $(-> $ret)?>(p) }
                    },)*
                })
            }
        }
    };
}

api! {
    gtk_init_check: fn(*mut c_int, *mut *mut *mut c_char) -> c_int;
    gtk_window_new: fn(c_int) -> Ptr;
    gtk_window_set_title: fn(Ptr, *const c_char);
    gtk_window_set_default_size: fn(Ptr, c_int, c_int);
    gtk_window_close: fn(Ptr);
    gtk_container_add: fn(Ptr, Ptr);
    gtk_widget_show_all: fn(Ptr);
    gtk_main: fn();
    gtk_main_quit: fn();
    g_signal_connect_data: fn(Ptr, *const c_char, GCallback, Ptr, Ptr, c_int) -> c_ulong;
    g_timeout_add_seconds: fn(c_uint, GSourceFunc, Ptr) -> c_uint;
    g_unix_signal_add: fn(c_int, GSourceFunc, Ptr) -> c_uint;
    g_free: fn(Ptr);
    g_error_free: fn(Ptr);
    webkit_website_data_manager_new_ephemeral: fn() -> Ptr;
    webkit_web_context_new_with_website_data_manager: fn(Ptr) -> Ptr;
    webkit_web_view_new_with_context: fn(Ptr) -> Ptr;
    webkit_web_view_load_uri: fn(Ptr, *const c_char);
    webkit_web_view_get_uri: fn(Ptr) -> *const c_char;
    webkit_web_view_get_main_resource: fn(Ptr) -> Ptr;
    webkit_web_resource_get_data: fn(Ptr, Ptr, GAsyncReadyCallback, Ptr);
    webkit_web_resource_get_data_finish: fn(Ptr, Ptr, *mut usize, *mut Ptr) -> *mut u8;
}

fn dlerror() -> String {
    // SAFETY: dlerror returns NULL or a C string valid until the next dl* call.
    let e = unsafe { libc::dlerror() };
    if e.is_null() { "unknown error".into() } else { unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned() }
}

/// A C string from Rust text without interior NULs (they're dropped).
pub fn cstring(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}
