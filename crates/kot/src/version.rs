//! Build identity: the crate version plus the exact commit it was built
//! from, so a binary already deployed to a host can be matched back to
//! source without guessing (`kot --version`, and every node's `/meta`).

/// `<crate-version>+<short-sha>`, with `-dirty` folded into the sha half if
/// the tree had uncommitted changes at build time. `KOT_GIT_SHA` comes from
/// `build.rs`.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("KOT_GIT_SHA"));
