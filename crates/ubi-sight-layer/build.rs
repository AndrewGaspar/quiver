fn main() {
    // The loader dlclose()s layers at vkDestroyInstance, but the layer's heartbeat thread keeps
    // running (and a game may create another instance later): keep the code mapped.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-cdylib-link-arg=-Wl,-z,nodelete");
    }
}
