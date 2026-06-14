//! Deterministic allocation-count probe for sub-noise, in-process optimizations.
//!
//! End-to-end wall-time benches (`stream_e2e`) are dominated by git subprocess
//! spawn + pipe I/O, so they cannot gate changes that only remove a few
//! per-record allocations. This probe installs a counting global allocator and
//! reports the number of allocations performed *during* a real filter run over
//! a synthetic many-tiny-blobs repository, which is deterministic for a fixed
//! input and therefore a stable gate for clone/allocation-elimination work.
//!
//! It is `#[ignore]`d by default (it is a measurement tool, not a pass/fail
//! test). Run it before and after a change to compare:
//!
//! ```text
//! cargo test -p filter-repo-rs --test alloc_probe -- --ignored --nocapture
//! ```
//!
//! Only allocations in *this* process are counted; the git subprocesses run in
//! their own address spaces, so the delta between two code versions on the same
//! input isolates the in-process allocation change.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

mod common;
use common::*;
use filter_repo_rs as fr;

struct CountingAllocator;

static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[test]
#[ignore = "manual allocation probe; run with --ignored --nocapture"]
fn alloc_probe_many_tiny_blobs() {
    let blob_count: usize = std::env::var("FRRS_ALLOC_BLOBS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);

    let repo = init_repo();
    for i in 0..blob_count {
        // Distinct content => distinct blob => exercises the InBlob header path
        // (the `mark :` line) once per blob.
        write_file(&repo, &format!("f{i}.txt"), &format!("blob-{i}\n"));
    }
    assert_eq!(run_git(&repo, &["add", "."]).0, 0);
    assert_eq!(run_git(&repo, &["commit", "-m", "many tiny blobs"]).0, 0);

    let opts = fr::Options {
        source: repo.clone(),
        target: repo.clone(),
        refs: vec!["--all".to_string()],
        force: true,
        quiet: true,
        ..Default::default()
    };

    let allocs_before = ALLOC_COUNT.load(Ordering::Relaxed);
    let bytes_before = ALLOC_BYTES.load(Ordering::Relaxed);
    let _ = fr::run(&opts);
    let allocs_after = ALLOC_COUNT.load(Ordering::Relaxed);
    let bytes_after = ALLOC_BYTES.load(Ordering::Relaxed);

    println!(
        "ALLOC_PROBE blobs={} allocs={} bytes={}",
        blob_count,
        allocs_after - allocs_before,
        bytes_after - bytes_before
    );
}
