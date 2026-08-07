use std::{env, path::PathBuf, process::Command};

fn main() {
    let qt = pkg_config::Config::new()
        .atleast_version("6.10")
        .probe("Qt6Quick")
        .expect("Qt 6.10 or newer with Qt Quick is required");
    pkg_config::Config::new()
        .probe("egl")
        .expect("EGL development files are required");
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is not set"));
    let moc = PathBuf::from(
        pkg_config::get_variable("Qt6Core", "libexecdir")
            .expect("Qt6Core does not publish its libexec directory"),
    )
    .join("moc");
    let generated = [("src/browser_surface.h", "moc_browser_surface.cpp")].map(|(source, name)| {
        let generated = output.join(name);
        let status = Command::new(&moc)
            .arg(source)
            .arg("-o")
            .arg(&generated)
            .status()
            .expect("could not run Qt moc");
        assert!(status.success(), "Qt moc failed for {source}");
        generated
    });

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .file("src/browser_surface.cpp")
        .flag_if_supported("-std=c++17");
    for generated in generated {
        build.file(generated);
    }
    for include in qt.include_paths {
        build.include(include);
    }
    build.compile("browser_surface");

    println!("cargo:rerun-if-changed=src/browser_surface.cpp");
    println!("cargo:rerun-if-changed=src/browser_surface.h");
}
