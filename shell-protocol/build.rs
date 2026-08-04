fn main() {
    println!("cargo:rerun-if-changed=proto/shell.proto");
    prost_build::Config::new()
        .compile_protos(&["proto/shell.proto"], &["proto"])
        .expect("compile the shell protocol");
}
