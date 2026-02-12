use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=frontend/src");
    println!("cargo:rerun-if-changed=frontend/package.json");

    if std::env::var("CARGO_FEATURE_EMBEDDED_FRONTEND").is_ok() {
        build_frontend();
    }
}

fn build_frontend() {
    let frontend_dir = std::path::Path::new("frontend");
    assert!(frontend_dir.exists(), "frontend directory not found");

    let pkg_mgr = if which::which("bun").is_ok() {
        "bun"
    } else if which::which("npm").is_ok() {
        "npm"
    } else {
        panic!("bun or npm required to build frontend");
    };

    run_command(
        pkg_mgr,
        &["install"],
        frontend_dir,
        "failed to install frontend dependencies",
    );
    run_command(
        pkg_mgr,
        &["run", "build"],
        frontend_dir,
        "failed to build frontend",
    );
}

fn run_command(pkg_mgr: &str, args: &[&str], frontend_dir: &std::path::Path, error: &str) {
    let status = Command::new(pkg_mgr)
        .args(args)
        .current_dir(frontend_dir)
        .status()
        .expect(error);
    assert!(status.success(), "{error}");
}
