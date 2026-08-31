fn main() {
    println!("cargo::rustc-check-cfg=cfg(build_script_cfg)");
}
