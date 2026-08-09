use std::{env, path::PathBuf, process::Command};

fn run_qt_tool(tool: &PathBuf, arguments: &[&str], output: &PathBuf) {
    let status = Command::new(tool)
        .args(arguments)
        .arg("-o")
        .arg(output)
        .status()
        .unwrap_or_else(|error| panic!("could not run {}: {error}", tool.display()));
    assert!(status.success(), "{} failed", tool.display());
}

fn main() {
    let quick = pkg_config::Config::new()
        .atleast_version("6.10")
        .probe("Qt6Quick")
        .expect("Qt 6.10 or newer with Qt Quick is required");
    let widgets = pkg_config::Config::new()
        .atleast_version("6.10")
        .probe("Qt6Widgets")
        .expect("Qt 6.10 or newer with Qt Widgets is required");
    pkg_config::Config::new()
        .probe("egl")
        .expect("EGL development files are required");
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is not set"));
    let moc = PathBuf::from(
        pkg_config::get_variable("Qt6Core", "libexecdir")
            .expect("Qt6Core does not publish its libexec directory"),
    )
    .join("moc");
    let rcc = moc.with_file_name("rcc");
    let generated = [
        ("src/browser_surface.h", "moc_browser_surface.cpp"),
        ("src/kde_shell.h", "moc_kde_shell.cpp"),
    ]
    .map(|(source, name)| {
        let generated = output.join(name);
        run_qt_tool(&moc, &[source], &generated);
        generated
    });
    let resources = output.join("qrc_deb_resources.cpp");
    run_qt_tool(
        &rcc,
        &["--name", "deb_resources", "src/deb_resources.qrc"],
        &resources,
    );

    let kf6_include_root = env::var_os("KF6_INCLUDE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            ["/usr/include/KF6", "/usr/local/include/KF6"]
                .into_iter()
                .map(PathBuf::from)
                .find(|path| path.join("KXmlGui/KXmlGuiWindow").is_file())
        })
        .expect(
            "KF6 development headers are required (set KF6_INCLUDE_DIR for a nonstandard prefix)",
        );

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .file("src/browser_surface.cpp")
        .file("src/kde_shell.cpp")
        .file(resources)
        .flag_if_supported("-std=c++17");
    for generated in generated {
        build.file(generated);
    }
    let mut qt_include_paths = quick.include_paths;
    qt_include_paths.extend(widgets.include_paths);
    qt_include_paths.sort();
    qt_include_paths.dedup();
    for include in qt_include_paths {
        build.include(include);
    }
    build.include(&kf6_include_root);
    for framework in [
        "KXmlGui",
        "KConfig",
        "KConfigCore",
        "KConfigGui",
        "KConfigWidgets",
        "KCoreAddons",
        "KGuiAddons",
        "KI18n",
        "KWidgetsAddons",
    ] {
        build.include(kf6_include_root.join(framework));
    }
    build.compile("browser_surface");

    println!("cargo:rustc-link-lib=dylib=KF6XmlGui");
    println!("cargo:rustc-link-lib=dylib=KF6ConfigCore");
    println!("cargo:rustc-link-lib=dylib=KF6CoreAddons");
    println!("cargo:rustc-link-lib=dylib=KF6I18n");

    println!("cargo:rerun-if-changed=src/browser_surface.cpp");
    println!("cargo:rerun-if-changed=src/browser_surface.h");
    println!("cargo:rerun-if-changed=src/kde_shell.cpp");
    println!("cargo:rerun-if-changed=src/kde_shell.h");
    println!("cargo:rerun-if-changed=src/deb_resources.qrc");
    println!("cargo:rerun-if-changed=src/debui.rc");
    println!("cargo:rerun-if-env-changed=KF6_INCLUDE_DIR");
}
