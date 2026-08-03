use std::{env, path::PathBuf, process::Command};

fn main() {
    let qt = pkg_config::Config::new()
        .atleast_version("6.10")
        .probe("Qt6Quick")
        .expect("Qt 6.10 or newer with Qt Quick is required");
    let generated = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is not set"))
        .join("moc_native_window_factory.cpp");
    let moc = PathBuf::from(
        pkg_config::get_variable("Qt6Core", "libexecdir")
            .expect("Qt6Core does not publish its libexec directory"),
    )
    .join("moc");
    let status = Command::new(moc)
        .arg("src/native_window_factory.h")
        .arg("-o")
        .arg(&generated)
        .status()
        .expect("could not run Qt moc");
    assert!(status.success(), "Qt moc failed");

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .file("src/native_window_factory.cpp")
        .file(generated)
        .flag_if_supported("-std=c++17");
    for include in qt.include_paths {
        build.include(include);
    }
    build.compile("native_window_factory");

    println!("cargo:rerun-if-changed=src/native_window_factory.cpp");
    println!("cargo:rerun-if-changed=src/native_window_factory.h");
}
