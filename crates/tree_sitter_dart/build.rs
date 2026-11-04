fn main() {
    let source_dir = std::path::Path::new("src");
    let mut build = cc::Build::new();
    build
        .include(source_dir)
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-but-set-variable")
        .flag_if_supported("-Wno-trigraphs")
        .flag_if_supported("-Wno-unused-function");
    #[cfg(target_env = "msvc")]
    build.flag("-utf-8");
    for file in ["parser.c", "scanner.c"] {
        let path = source_dir.join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        build.file(path);
    }
    build.compile("tree-sitter-dart");
}
