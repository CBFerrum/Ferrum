use std::process::Command;

// `sqlx::migrate!` embeds the migrations at compile time: rebuild when one is added or changed.
// The build's date (src/window.rs) is the commit's: `BUILD_DATE` (seconds since 1970, as `git log -1 --format=%ct`
// prints it; the image build passes it, having no git), else git's, else none. An image build is exact. A local build
// from git takes the date of the commit checked out when this script last ran (a commit does not rebuild the kernel and
// everything above it), so two local builds may look the same age: they are then level, as before the window, which is
// safe.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
    println!("cargo:rerun-if-env-changed=BUILD_DATE");
    let date = match std::env::var("BUILD_DATE").map(|v| v.trim().to_string()) {
        Ok(v) if !v.is_empty() => {
            assert!(v.parse::<i64>().is_ok(), "BUILD_DATE must be the commit's date in seconds since 1970 (git log -1 --format=%ct), not {v:?}");
            v
        }
        _ => git_date().unwrap_or_default(),
    };
    println!("cargo:rustc-env=FERRUM_BUILD_DATE={date}");
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The date of the commit checked out, when this file is in that checkout (not a copy unpacked in another one).
fn git_date() -> Option<String> {
    git(&["ls-files", "--error-unmatch", "build.rs"])?;
    git(&["log", "-1", "--format=%ct"]).filter(|d| d.parse::<i64>().is_ok())
}
