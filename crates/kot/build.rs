//! Stamps the exact commit a binary was built from into `KOT_GIT_SHA`, so
//! `kot --version` (and every node's `/meta`) can be matched back to source
//! without guessing which of several deployed builds is running where.
//!
//! No `cargo:rerun-if-changed` on purpose: without one, cargo reruns this
//! script on every build, which is what we want — the sha must always
//! reflect HEAD at build time, not whenever `.git/HEAD` last happened to
//! change.

use std::process::Command;

fn main() {
    let sha = git_output(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let dirty = git_output(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    let sha = if dirty { format!("{sha}-dirty") } else { sha };
    println!("cargo:rustc-env=KOT_GIT_SHA={sha}");
}

fn git_output(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}
