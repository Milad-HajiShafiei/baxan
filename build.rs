fn main() {
    if cfg!(target_os = "linux") || cfg!(target_os = "macos") {
        let out_dir = std::env::var("OUT_DIR").unwrap();
        let src = "src/tracker.c";

        let (lib_name, extra_flags): (&str, &[&str]) = if cfg!(target_os = "macos") {
            ("libbaxan_tracker.dylib", &["-dynamiclib"])
        } else {
            ("libbaxan_tracker.so", &["-shared"])
        };

        let output = format!("{out_dir}/{lib_name}");

        let status = std::process::Command::new("cc")
            .args(extra_flags)
            .args(["-O2", "-o", &output, src, "-ldl"])
            .status()
            .expect("Failed to invoke C compiler. Is cc/clang/gcc installed?");
        assert!(status.success(), "Failed to compile baxan tracker shared library");
    }

    println!("cargo:rerun-if-changed=src/tracker.c");
}
