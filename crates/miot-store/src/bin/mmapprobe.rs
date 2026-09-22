//! Pins down exactly which mmap(2) shape the host refuses. 1 page each:
//! 1 anonymous RW, 2 file MAP_PRIVATE RO, 3 file MAP_SHARED RO,
//! 4 file MAP_SHARED RW (the one ParityDB's MmapMut needs).
//!
//! Raw syscalls, x86_64 only (the real `akuma` host's architecture); on any
//! other target the binary builds but refuses to run, since the point is to
//! ask *this* question on *that* kernel. Built live 2026-09-22 after
//! `storeprobe` stage 6 (reopen) died with `os error 38` there — see
//! docs/TOPOLOGY.md's `node5` section for what it found.

#[cfg(target_arch = "x86_64")]
fn main() {
    run()
}

#[cfg(not(target_arch = "x86_64"))]
fn main() {
    eprintln!("mmapprobe is x86_64-only (raw mmap syscall); cross-compile it.");
    std::process::exit(2);
}

#[cfg(target_arch = "x86_64")]
fn run() {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::fd::AsRawFd;

    let path = std::env::args().nth(1).unwrap_or_else(|| "mmapprobe.bin".into());
    let mut f = OpenOptions::new().create(true).read(true).write(true).open(&path).unwrap();
    f.write_all(&[0x41u8; 4096]).unwrap();

    unsafe {
        println!("[mm] 1 anonymous RW:   {}", try_map(|a, l| libc_mmap(a, l, 3, 0x02 | 0x20, -1, 0), 4096));
        println!("[mm] 2 file PRIV RO:  {}", try_map(|a, l| libc_mmap(a, l, 1, 0x02, f.as_raw_fd(), 0), 4096));
        println!("[mm] 3 file SHARED RO:{}", try_map(|a, l| libc_mmap(a, l, 1, 0x01, f.as_raw_fd(), 0), 4096));
        println!("[mm] 4 file SHARED RW:{}", try_map(|a, l| libc_mmap(a, l, 3, 0x01, f.as_raw_fd(), 0), 4096));
    }
}

#[cfg(target_arch = "x86_64")]
unsafe fn try_map(go: impl Fn(*mut u8, usize) -> *mut u8, len: usize) -> String {
    let p = go(std::ptr::null_mut(), len);
    if p as isize == -1 {
        format!("FAIL errno={}", std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        let _ = std::ptr::read_volatile(p);
        format!("ok ptr={:p}", p)
    }
}

#[cfg(target_arch = "x86_64")]
unsafe fn libc_mmap(a: *mut u8, l: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8 {
    syscall_mmap(a as u64, l as u64, prot as u64, flags as u64, fd as u64, off as u64) as *mut u8
}

#[cfg(target_arch = "x86_64")]
unsafe fn syscall_mmap(a: u64, l: u64, prot: u64, flags: u64, fd: u64, off: u64) -> u64 {
    let ret;
    core::arch::asm!(
        "syscall",
        inlateout("rax") 9usize as u64 => ret,
        in("rdi") a, in("rsi") l, in("rdx") prot, in("r10") flags, in("r8") fd, in("r9") off,
        lateout("rcx") _, lateout("r11") _,
    );
    ret
}
