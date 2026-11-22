//! Opt-in sampling profiler (SESSIONS_PROF_OUT=path): SIGPROF every 100us of process CPU time records the
//! interrupted instruction pointer; `dump` writes PIE-relative offsets for addr2line. No effect when unset.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const CAP: usize = 1 << 21;
static SAMPLES: [AtomicU64; CAP] = [const { AtomicU64::new(0) }; CAP];
static N: AtomicUsize = AtomicUsize::new(0);

extern "C" fn on_prof(_sig: libc::c_int, _info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
    unsafe {
        let uc = ctx as *const libc::ucontext_t;
        let rip = (*uc).uc_mcontext.gregs[libc::REG_RIP as usize] as u64;
        let i = N.fetch_add(1, Ordering::Relaxed);
        if i < CAP {
            SAMPLES[i].store(rip, Ordering::Relaxed);
        }
    }
}

/// Current sample count (phase markers for SESSIONS_PROF_OUT).
pub fn mark() -> usize {
    N.load(Ordering::Relaxed)
}

pub fn start() {
    if std::env::var_os("SESSIONS_PROF_OUT").is_none() {
        return;
    }
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_prof as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
        libc::sigaction(libc::SIGPROF, &sa, std::ptr::null_mut());
        let tv = libc::timeval { tv_sec: 0, tv_usec: 100 };
        let it = libc::itimerval { it_interval: tv, it_value: tv };
        libc::setitimer(libc::ITIMER_PROF, &it, std::ptr::null_mut());
    }
}

pub fn dump() {
    let Some(path) = std::env::var_os("SESSIONS_PROF_OUT") else { return };
    unsafe {
        let it: libc::itimerval = std::mem::zeroed();
        libc::setitimer(libc::ITIMER_PROF, &it, std::ptr::null_mut());
    }
    // executable's load base from /proc/self/maps (first mapping of our own binary)
    let exe = std::fs::read_link("/proc/self/exe").unwrap_or_default();
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
    let mut base = 0u64;
    for l in maps.lines() {
        if l.ends_with(exe.to_str().unwrap_or("?")) {
            let a = l.split('-').next().unwrap_or("0");
            base = u64::from_str_radix(a, 16).unwrap_or(0);
            break;
        }
    }
    let n = N.load(Ordering::Relaxed).min(CAP);
    let mut out = String::with_capacity(n * 10);
    for s in SAMPLES.iter().take(n) {
        let ip = s.load(Ordering::Relaxed);
        out.push_str(&format!("{:x}\n", ip.wrapping_sub(base)));
    }
    let _ = std::fs::write(&path, out);
    // raw absolute IPs + maps, to attribute samples outside the binary (libc, vdso, ...)
    let mut raw = String::new();
    for s in SAMPLES.iter().take(n) {
        raw.push_str(&format!("{:x}\n", s.load(Ordering::Relaxed)));
    }
    let mut p2 = path.clone();
    p2.push(".abs");
    let _ = std::fs::write(&p2, raw);
    let mut p3 = path.clone();
    p3.push(".maps");
    let _ = std::fs::write(&p3, maps);
}
