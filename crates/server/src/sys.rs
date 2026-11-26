//! The few OS-specific bits, declared by hand so the binary needs no libc crate:
//!
//! * SIGINT/SIGTERM set a flag the main thread polls (a second signal exits at once);
//! * a send timeout on the listening socket, which Linux copies to every accepted connection,
//!   so a client that stops reading can't hold a request worker forever. (A receive timeout
//!   would also apply to `accept` itself, which tiny_http treats as fatal, so reads are guarded
//!   differently; see `http`.)
//! * glibc malloc tuning that keeps the resident set small: big temporary buffers (argon2's
//!   19 MiB, image bytes) are mapped and unmapped instead of lingering in the heap, and a couple of
//!   arenas serve all threads.
//!
//! Everything here is best effort: on other platforms the functions do nothing.

use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// True once SIGINT or SIGTERM arrived.
pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

#[cfg(unix)]
pub fn install_signal_handlers() {
    use std::os::raw::c_int;

    unsafe extern "C" {
        fn signal(signum: c_int, handler: usize) -> usize;
        fn _exit(status: c_int) -> !;
    }

    extern "C" fn on_signal(_signum: c_int) {
        // Only async-signal-safe work here: an atomic swap, and _exit on the second signal.
        if SHUTDOWN.swap(true, Ordering::SeqCst) {
            // SAFETY: _exit is async-signal-safe and takes no pointers.
            unsafe { _exit(130) };
        }
    }

    const SIGINT: c_int = 2;
    const SIGTERM: c_int = 15;
    let handler = on_signal as extern "C" fn(c_int) as usize;
    // SAFETY: installing a handler that only touches an atomic (see above).
    unsafe {
        signal(SIGINT, handler);
        signal(SIGTERM, handler);
    }
}

#[cfg(not(unix))]
pub fn install_signal_handlers() {}

/// Give every connection accepted on `listener` a send timeout of `secs` seconds.
#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )
))]
pub fn set_send_timeout(listener: &TcpListener, secs: i64) -> bool {
    use std::os::fd::AsRawFd;
    use std::os::raw::{c_int, c_long, c_void};

    #[repr(C)]
    struct Timeval {
        tv_sec: c_long,
        tv_usec: c_long,
    }

    unsafe extern "C" {
        fn setsockopt(
            fd: c_int,
            level: c_int,
            name: c_int,
            value: *const c_void,
            len: u32,
        ) -> c_int;
    }

    const SOL_SOCKET: c_int = 1;
    const SO_SNDTIMEO: c_int = 21;
    let tv = Timeval {
        tv_sec: secs as c_long,
        tv_usec: 0,
    };
    // SAFETY: the fd is a valid socket owned by `listener`, and `tv` outlives the call.
    unsafe {
        setsockopt(
            listener.as_raw_fd(),
            SOL_SOCKET,
            SO_SNDTIMEO,
            (&tv as *const Timeval).cast(),
            size_of::<Timeval>() as u32,
        ) == 0
    }
}

#[cfg(not(all(
    target_os = "linux",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )
)))]
pub fn set_send_timeout(_listener: &TcpListener, _secs: i64) -> bool {
    false
}

/// Keep glibc's heap from holding on to memory the process no longer uses.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn tune_malloc() {
    use std::os::raw::c_int;

    unsafe extern "C" {
        fn mallopt(param: c_int, value: c_int) -> c_int;
    }

    const M_TRIM_THRESHOLD: c_int = -1;
    const M_MMAP_THRESHOLD: c_int = -3;
    const M_ARENA_MAX: c_int = -8;
    // SAFETY: mallopt only adjusts allocator parameters; called before any thread starts.
    unsafe {
        mallopt(M_ARENA_MAX, 2);
        mallopt(M_MMAP_THRESHOLD, 256 * 1024);
        mallopt(M_TRIM_THRESHOLD, 1024 * 1024);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn tune_malloc() {}
