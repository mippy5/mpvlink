fn main() {
    // The mpv_* symbols are provided by the mpv executable at load time, so we
    // don't link against libmpv. On macOS the linker must be told to leave them
    // undefined; on Linux, shared libraries allow undefined symbols by default.
    // Windows has neither, so there ffi.rs calls through pointers mpv fills in.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-cdylib-link-arg=-undefined");
        println!("cargo:rustc-cdylib-link-arg=dynamic_lookup");
    }
}
