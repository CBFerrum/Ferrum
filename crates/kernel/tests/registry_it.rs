//! What a workspace's effective schema costs a replica that keeps it (K6), against a real PostgreSQL
//! (`FERRUM_REGISTRY_TEST_DATABASE_URL`; the database is reset first). The cache's bound is tested beside the cache
//! (`registry::tests::cache_is_bounded`, no database needed); this prints the number that sets its capacity.

use ferrum_kernel::KernelSettings;
use ferrum_meta::catalog::load_package_dir;
use ferrum_meta::effective;
use ferrum_testkit::{Kit, database, run};
use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};

/// Bytes allocated minus freed, by every thread: this binary has the one test, and nothing else runs while it counts.
static NET: AtomicI64 = AtomicI64::new(0);

struct Counting;

fn add(n: i64) {
    NET.fetch_add(n, Ordering::Relaxed);
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
        let before = NET.load(Ordering::Relaxed);
        let schema = effective::build(&catalog, &names, &[], 1).expect("the schema of every package");
        let bytes = NET.load(Ordering::Relaxed) - before;
        println!("schema_size: {bytes} bytes for {} packages ({added} added), {} entities", names.len(), schema.entities.len());
        // What building costs, and how much of what is built is the entities (each its own `Arc`, which the cache could share).
        let took = (0..3)
            .map(|_| {
                let at = std::time::Instant::now();
                let _ = effective::build(&catalog, &names, &[], 1).expect("a schema");
                at.elapsed()
            })
            .min()
            .expect("three builds");
        let before = NET.load(Ordering::Relaxed);
        let copies: Vec<_> = schema.entities.values().map(|e| (**e).clone()).collect();
        let entity_bytes = NET.load(Ordering::Relaxed) - before;
        drop(copies);
        println!("schema_build: {took:?} for one build (best of 3, unoptimised test profile); entities about {entity_bytes} bytes of the {bytes}");
        // Workspaces with the same packages each hold a schema of their own: five held at once, and 200 by multiplication.
        let before = NET.load(Ordering::Relaxed);
        let five: Vec<_> = (0..5).map(|v| effective::build(&catalog, &names, &[], v).expect("a schema")).collect();
        let held = NET.load(Ordering::Relaxed) - before;
        println!("schema_held: {held} bytes for 5 workspaces with the same packages, {} for 200", held / 5 * 200);
        drop(five);
        // The same five as workspaces of a registry, which builds the schema once and shares what it can (registry.rs).
        let all: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut tenants = Vec::new();
        for i in 0..5 {
            tenants.push(kit.workspace(&format!("held{i}"), &all, serde_json::Value::Null).await.tenant);
        }
        kit.k.registry.invalidate_all();
        let before = NET.load(Ordering::Relaxed);
        kit.k.registry.schema(tenants[0]).await.expect("a schema");
        let one = NET.load(Ordering::Relaxed) - before;
        for t in &tenants[1..] {
            kit.k.registry.schema(*t).await.expect("a schema");
        }
        let five = NET.load(Ordering::Relaxed) - before;
        let each = (five - one) / 4;
        println!(
            "schema_shared: {five} bytes for 5 workspaces with the same packages ({one} for the first, {each} for each after it), {} for 200",
            one + 199 * each
        );
        assert!(bytes > 10_000, "a schema of {} entities holds more than {bytes} bytes", schema.entities.len());
    });
}
