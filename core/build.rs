use std::path::PathBuf;
fn main() {
    println!("cargo:rerun-if-env-changed=DOLTLITE_AMALGAMATION");
    let source = PathBuf::from(
        std::env::var_os("DOLTLITE_AMALGAMATION")
            .expect("set DOLTLITE_AMALGAMATION to the generated sqlite3.c"),
    );
    println!("cargo:rerun-if-changed={}", source.display());
    let mut build = cc::Build::new();
    build
        .file(&source)
        .flag_if_supported("-w")
        .opt_level(2)
        .define("NDEBUG", None)
        .define("DOLTLITE_PROLLY", "1")
        .define("DOLTLITE_VERSION", "\"v0.50.2\"")
        .define("SQLITE_THREADSAFE", "1")
        .define("SQLITE_ENABLE_MATH_FUNCTIONS", None)
        .define("SQLITE_ENABLE_FTS5", None)
        .define("SQLITE_ENABLE_RTREE", None)
        .define("SQLITE_ENABLE_DBSTAT_VTAB", None);
    if std::env::var("CARGO_CFG_TARGET_VENDOR").unwrap() == "apple" {
        build.define("DOLTLITE_APPLE_SYSTEM_TRUST", "1");
        println!("cargo:rustc-link-lib=framework=Security");
    }
    if std::env::var("CARGO_CFG_TARGET_VENDOR").unwrap() == "apple" {
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
    }
    // Use a repository-relative name in C diagnostics and the packaged library.
    if let Some(parent) = source.parent() {
        build.flag_if_supported(format!("-ffile-prefix-map={}=vendor/doltlite", parent.display()));
    }
    build.compile("doltlite");
    println!("cargo:rustc-link-lib=z");
}
