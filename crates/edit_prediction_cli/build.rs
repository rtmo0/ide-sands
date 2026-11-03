fn main() {
    println!("cargo:rerun-if-changed=../zed/Cargo.toml");
    let cargo_toml = std::fs::read_to_string("../sands/Cargo.toml")
        .expect("Failed to read crates/sands/Cargo.toml");
    let version = cargo_toml
        .lines()
        .find(|line| line.starts_with("version = "))
        .expect("Version not found in crates/sands/Cargo.toml")
        .split('=')
        .nth(1)
        .expect("Invalid version format")
        .trim()
        .trim_matches('"');
    println!("cargo:rustc-env=ZED_PKG_VERSION={}", version);
}
