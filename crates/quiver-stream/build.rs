//! Builds the libav shim (csrc/qs_av.c) against the system's FFmpeg and Vulkan headers, and
//! links libopus.

fn main() {
    println!("cargo:rerun-if-changed=csrc/qs_av.c");
    println!("cargo:rerun-if-changed=csrc/qs_av.h");
    let mut build = cc::Build::new();
    build.file("csrc/qs_av.c").warnings(true).flag_if_supported("-Wno-deprecated-declarations");
    for lib in ["libavcodec", "libavformat", "libavutil"] {
        let l = pkg_config::Config::new().probe(lib).unwrap_or_else(|e| panic!("{lib}: {e}"));
        for p in l.include_paths {
            build.include(p);
        }
    }
    if let Ok(vk) = pkg_config::Config::new().cargo_metadata(false).probe("vulkan") {
        for p in vk.include_paths {
            build.include(p);
        }
    }
    build.compile("qs_av");
    pkg_config::Config::new().probe("opus").expect("libopus");
}
