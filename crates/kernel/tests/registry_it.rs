//! What a workspace's effective schema costs a replica that keeps it (K6), against a real PostgreSQL
//! (`FERRUM_REGISTRY_TEST_DATABASE_URL`; the database is reset first). The cache's bound is tested beside the cache
//! (`registry::tests::cache_is_bounded`, no database needed); this prints the number that sets its capacity.

use ferrum_kernel::KernelSettings;
use ferrum_meta::catalog::load_package_dir;
use ferrum_meta::effective;
use ferrum_testkit::{Kit, database, run};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::Path;

thread_local! {
    /// Bytes this thread has allocated minus freed. A thread's own, so other tests running beside this one do not count.
    static NET: Cell<i64> = const { Cell::new(0) };
}

struct Counting;

fn add(n: i64) {
    NET.with(|c| c.set(c.get() + n));
}

// SAFETY: every call is the system allocator's own; only a counter beside it changes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            add(l.size() as i64);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        add(-(l.size() as i64));
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            add(l.size() as i64);
        }
        p
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            add(new as i64 - l.size() as i64);
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Bytes the effective schema of a workspace with every package of the catalog installed holds once built: what
/// `Registry::schema` keeps per workspace. Printed for the platform's own packages; with `FERRUM_SCHEMA_SIZE_PACKAGES`
/// naming directories of packages (colon-separated, each holding package directories) also for those added.
///
/// ```text
/// cargo test -p ferrum-kernel --test registry_it schema_size_is_printed -- --nocapture
/// ```
#[test]
fn schema_size_is_printed() {
    run(async {
        let mut extra = Vec::new();
        for dir in std::env::var("FERRUM_SCHEMA_SIZE_PACKAGES").unwrap_or_default().split(':').filter(|d| !d.is_empty()) {
            let mut sub: Vec<_> = std::fs::read_dir(dir)
                .expect("a package directory")
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.join("package.yaml").is_file())
                .collect();
            sub.sort();
            extra.extend(sub.iter().map(|p| load_package_dir(Path::new(p)).expect("a package")));
        }
        let added = extra.len();
        let kit = Kit::boot_with(&database("FERRUM_REGISTRY_TEST_DATABASE_URL"), &[], extra, KernelSettings::default()).await;
        let catalog = kit.k.registry.catalog();
        let names: Vec<String> = catalog.packages().map(|p| p.name().to_string()).collect();
        // No await between the two readings: the same thread counts both.
        let before = NET.with(Cell::get);
        let schema = effective::build(&catalog, &names, &[], 1).expect("the schema of every package");
        let bytes = NET.with(Cell::get) - before;
        println!("schema_size: {bytes} bytes for {} packages ({added} added), {} entities", names.len(), schema.entities.len());
        assert!(bytes > 10_000, "a schema of {} entities holds more than {bytes} bytes", schema.entities.len());
    });
}
