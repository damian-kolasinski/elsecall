fn main() {
    println!("cargo:rerun-if-changed=native/airdrop.m");
    cc::Build::new()
        .file("native/airdrop.m")
        .flag("-fobjc-arc")
        .compile("elsecall_airdrop");
    println!("cargo:rustc-link-lib=framework=AppKit");
}
