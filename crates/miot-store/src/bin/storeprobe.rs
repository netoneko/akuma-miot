//! Does ParityDB work here?
//!
//! Ships to a guest and answers the question rather than arguing about it.
//! `docs/MAPPING_REPORT.md` has claimed more than once that a state database is
//! the thing standing between a litter and a hobby kernel, on the grounds that
//! ParityDB mmaps large, sparse files. That was reasoning from requirements,
//! not from a run — and Akuma advertises `mmap`, demand paging and MMU-backed
//! isolation, and hosts `rustc`, which mmaps heavily.
//!
//! Each stage announces itself **before** doing the work, and the exit status
//! is the number of stages completed — the shape `akuma/userspace/amd64/ruststd`
//! uses, for its stated reason: a program that dies at stage 3 has no exit
//! status to report, so a truncated log has to name the wall.
//!
//! ```text
//!   [db] N <what>     printed before stage N runs
//!   exit(STAGES)      every stage completed
//! ```
//!
//!   storeprobe [dir]     default: ./storeprobe.db

use miot_store::Store;
use std::io::Write;

const STAGES: i32 = 7;

fn stage(n: u32, what: &str) {
    println!("[db] {n} {what}");
    let _ = std::io::stdout().flush();
}

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "storeprobe.db".into());
    let _ = std::fs::remove_dir_all(&dir);
    println!("[db] parity-db probe in {dir}");

    stage(1, "open (creates files, maps them)");
    let mut s = Store::open(&dir).expect("open");
    assert_eq!(s.head(), 0);

    stage(2, "append 256 blocks");
    for h in 1..=256u64 {
        s.append(h, format!("block-{h}").as_bytes()).expect("append");
    }
    assert_eq!(s.head(), 256);

    stage(3, "read back");
    assert_eq!(s.block(128).expect("get"), Some(b"block-128".to_vec()));

    stage(4, "compact (a state blob, and the blocks beneath it dropped)");
    let state = vec![0xABu8; 8 * 1024];
    s.compact(200, &state).expect("compact");
    assert_eq!(s.checkpoint_state().expect("cp"), Some(state.clone()));
    assert_eq!(s.block(100).expect("gone"), None);

    stage(5, "rewind to the compaction, discarding our own tail");
    let r = s.rewind_for_fork(250).expect("rewind");
    assert_eq!(r.height, 200);
    assert_eq!(r.dropped, 56);

    stage(6, "reopen — does it survive a process boundary?");
    drop(s);
    let s2 = Store::open(&dir).expect("reopen");
    assert_eq!(s2.head(), 200);
    assert_eq!(s2.last_checkpoint(), 200);
    assert_eq!(s2.checkpoint_state().expect("cp"), Some(state));

    stage(7, "a large value (64 KiB, the artifact cap)");
    drop(s2);
    let mut s3 = Store::open(&dir).expect("reopen 2");
    let big = vec![0x5Au8; 64 * 1024];
    s3.append(201, &big).expect("big append");
    assert_eq!(s3.block(201).expect("big get"), Some(big));

    let bytes: u64 = std::fs::read_dir(&dir)
        .map(|rd| rd.filter_map(|e| e.ok()).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum())
        .unwrap_or(0);
    println!("[db]   on-disk apparent size: {} KiB", bytes / 1024);
    println!("[db] all {STAGES} stages complete");
    let _ = std::io::stdout().flush();
    std::process::exit(STAGES);
}
