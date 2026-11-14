//! The commit this build was made from, as the build helper and release
//! tags name it.

use std::process::Command;

fn main() {
    // A change of branch or commit has to run this again, even when none
    // of the crate's own sources changed.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");
    let build = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    println!("cargo:rustc-env=SKYWARD_BUILD={build}");
}
