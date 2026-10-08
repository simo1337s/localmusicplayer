//! Keeps the C allocator from holding on to memory the app is done with.
//!
//! glibc gives every thread that allocates its own arena and keeps freed memory there, and it
//! raises its mmap threshold each time a large block is freed, so later large blocks (decoded
//! covers, HTTP bodies, scan results) come from the heap and stay resident after they are
//! freed. A player that decodes covers on worker threads slowly grows that way.

/// Call once at startup, before other threads exist.
pub fn tune() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: mallopt only changes allocator settings; called before other threads start.
    unsafe {
        // Two arenas are enough for a UI thread plus a couple of runtime workers.
        libc::mallopt(libc::M_ARENA_MAX, 2);
        // A fixed threshold: big blocks are mapped on their own and unmapped when freed.
        libc::mallopt(libc::M_MMAP_THRESHOLD, 256 * 1024);
        // Give the top of the heap back once 2 MiB of it is free.
        libc::mallopt(libc::M_TRIM_THRESHOLD, 2 * 1024 * 1024);
    }
}

/// Returns freed memory to the system: after bursts like a library scan or a sync, and now and
/// then while the app runs.
pub fn trim() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim is thread-safe and only releases free pages.
    unsafe {
        libc::malloc_trim(0);
    }
}
