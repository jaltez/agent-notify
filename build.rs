//! Captures the compiler identity for `agent-notify version`
//! (cli.rs's cmd_version prints it where Go prints runtime.Version(),
//! which is baked into a Go binary at build time).

fn main() {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let version = std::process::Command::new(&rustc)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "rustc".to_string());
    println!("cargo:rustc-env=RUSTC_VERSION={version}");
}
