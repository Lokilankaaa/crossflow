use std::{
    env,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn output(program: &str, args: &[&str]) -> Option<String> {
    let result = Command::new(program).args(args).output().ok()?;
    result
        .status
        .success()
        .then(|| String::from_utf8_lossy(&result.stdout).trim().to_owned())
}

fn main() {
    // A source archive without .git is explicitly unversioned, never assumed clean.
    for path in [
        "src",
        "Cargo.toml",
        "Cargo.lock",
        "build.rs",
        ".git/HEAD",
        ".git/index",
        ".git/refs/heads",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    let has_repository = std::path::Path::new(".git").exists();
    let revision = has_repository
        .then(|| output("git", &["rev-parse", "HEAD"]))
        .flatten()
        .unwrap_or_else(|| "unknown".into());
    let dirty = has_repository
        .then(|| {
            output(
                "git",
                &["status", "--porcelain", "--untracked-files=normal"],
            )
        })
        .flatten()
        .map(|status| (!status.is_empty()).to_string())
        .unwrap_or_else(|| "unknown".into());
    let compiler = env::var("RUSTC")
        .ok()
        .and_then(|rustc| output(&rustc, &["--version"]))
        .unwrap_or_else(|| "unknown".into());
    let built = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before Unix epoch")
        .as_secs();
    println!("cargo:rustc-env=CROSSFLOW_BUILD_REVISION={revision}");
    println!("cargo:rustc-env=CROSSFLOW_BUILD_DIRTY={dirty}");
    println!("cargo:rustc-env=CROSSFLOW_BUILD_COMPILER={compiler}");
    println!("cargo:rustc-env=CROSSFLOW_BUILD_UNIX_SECONDS={built}");
}
