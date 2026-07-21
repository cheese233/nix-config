//! Read-only shared-memory IPC for upstream address distribution.
//!
//! The supervisor is the sole writer of the bootstrap resolution result;
//! worker children are read-only consumers. Layout is a single 4 KiB page in
//! a `memfd`, shared via `MAP_SHARED` and guarded by a seqlock (single
//! writer, multiple readers, lock-free on the read side).
//!
//! Enforcement: the supervisor maps the page `PROT_READ|PROT_WRITE` before
//! forking; children map the inherited fd `PROT_READ` — they cannot write
//! the mapping (they could re-map the fd RW in principle, but all code here
//! is one privilege domain; the type system simply never exposes a writer
//! to children).

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd, RawFd};
use std::sync::atomic::{fence, AtomicU32, Ordering};
use std::time::Instant;

use nix::sys::memfd::{memfd_create, MFdFlags};
use nix::sys::mman::{mmap, MapFlags, ProtFlags};
use nix::unistd::ftruncate;

/// Max upstream addresses stored (dns.google has 4; 8 is plenty).
pub const MAX_ADDRS: usize = 8;
const SHM_SIZE: usize = 4096;

/// One stored socket address (IP only; the port is fixed by configuration).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawAddr {
    bytes: [u8; 16],
    is_v6: u8,
    _pad: [u8; 15],
}

impl RawAddr {
    fn from_ip(ip: IpAddr) -> Self {
        let mut raw = RawAddr::default();
        match ip {
            IpAddr::V4(v4) => {
                raw.bytes[..4].copy_from_slice(&v4.octets());
                raw.is_v6 = 0;
            }
            IpAddr::V6(v6) => {
                raw.bytes = v6.octets();
                raw.is_v6 = 1;
            }
        }
        raw
    }

    fn to_ip(&self) -> IpAddr {
        if self.is_v6 == 1 {
            IpAddr::V6(Ipv6Addr::from(self.bytes))
        } else {
            IpAddr::V4(Ipv4Addr::new(
                self.bytes[0],
                self.bytes[1],
                self.bytes[2],
                self.bytes[3],
            ))
        }
    }
}

/// The shared page layout (repr(C), seqlock-guarded).
#[repr(C)]
struct SharedResolve {
    /// Sequence counter: odd while the writer is publishing, even otherwise.
    seq: AtomicU32,
    count: u32,
    /// CLOCK_MONOTONIC seconds when this resolution expires (informational).
    expires_mono_secs: u64,
    addrs: [RawAddr; MAX_ADDRS],
}

/// CLOCK_MONOTONIC now in seconds (system-wide epoch, comparable across processes).
fn mono_now_secs() -> u64 {
    nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC)
        .map(|ts| ts.tv_sec() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Writer (supervisor only)
// ---------------------------------------------------------------------------

/// The supervisor's writable handle to the shared page.
pub struct ResolveWriter {
    ptr: *mut SharedResolve,
}

// The raw pointer is only dereferenced in the supervisor process.
unsafe impl Send for ResolveWriter {}

impl ResolveWriter {
    /// Create the memfd and map it shared+writable. The returned fd is
    /// inherited by children (who map it read-only).
    pub fn create() -> io::Result<(OwnedFd, Self)> {
        let fd = memfd_create("microdoh3-resolve", MFdFlags::MFD_CLOEXEC)?;
        ftruncate(&fd, SHM_SIZE as _)?;
        let ptr = unsafe {
            mmap(
                None,
                SHM_SIZE.try_into().unwrap(),
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                &fd,
                0,
            )?
        };
        let writer = Self {
            ptr: ptr.as_ptr().cast(),
        };
        // Initialize to a clean even sequence.
        writer.raw().seq.store(0, Ordering::Relaxed);
        Ok((fd, writer))
    }

    fn raw(&self) -> &SharedResolve {
        unsafe { &*self.ptr }
    }

    fn raw_mut(&mut self) -> &mut SharedResolve {
        unsafe { &mut *self.ptr }
    }

    /// Publish a new resolution result.
    pub fn publish(&mut self, addrs: &[IpAddr], expires_at: Instant) {
        let old = self.raw().seq.load(Ordering::Relaxed);
        self.raw().seq.store(old.wrapping_add(1), Ordering::Release); // odd: write in progress
        {
            let p = self.raw_mut();
            p.count = addrs.len().min(MAX_ADDRS) as u32;
            for (i, &ip) in addrs.iter().take(MAX_ADDRS).enumerate() {
                p.addrs[i] = RawAddr::from_ip(ip);
            }
            p.expires_mono_secs =
                mono_now_secs() + expires_at.saturating_duration_since(Instant::now()).as_secs();
        }
        fence(Ordering::SeqCst);
        self.raw().seq.store(old.wrapping_add(2), Ordering::Release); // even: committed
    }
}

impl Drop for ResolveWriter {
    fn drop(&mut self) {
        unsafe {
            let _ = nix::sys::mman::munmap(
                std::ptr::NonNull::new_unchecked(self.ptr.cast::<std::ffi::c_void>()),
                SHM_SIZE,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Reader (worker children, read-only)
// ---------------------------------------------------------------------------

/// A child's read-only view of the shared page.
pub struct ResolveReader {
    ptr: *const SharedResolve,
    last_seq: u32,
}

unsafe impl Send for ResolveReader {}

impl ResolveReader {
    /// Map the inherited fd read-only.
    pub fn map_readonly(fd: RawFd) -> io::Result<Self> {
        let borrowed: BorrowedFd<'_> = unsafe { BorrowedFd::borrow_raw(fd) };
        let ptr = unsafe {
            mmap(
                None,
                SHM_SIZE.try_into().unwrap(),
                ProtFlags::PROT_READ,
                MapFlags::MAP_SHARED,
                borrowed.as_fd(),
                0,
            )?
        };
        Ok(Self {
            ptr: ptr.as_ptr().cast(),
            last_seq: 0,
        })
    }

    fn raw(&self) -> &SharedResolve {
        unsafe { &*self.ptr }
    }

    /// Expiry (CLOCK_MONOTONIC seconds), informational only.
    #[allow(dead_code)]
    pub fn expires_mono_secs(&self) -> u64 {
        self.raw().expires_mono_secs
    }

    /// Whether the published resolution is past its TTL.
    #[allow(dead_code)]
    pub fn is_stale(&self) -> bool {
        mono_now_secs() > self.raw().expires_mono_secs
    }

    /// Read the current address set if it changed since the last call.
    /// Returns None when unchanged, when the writer is mid-publish (retry
    /// next time), or when a torn read was detected (retry next time).
    pub fn read_if_changed(&mut self) -> Option<Vec<IpAddr>> {
        let first = self.raw().seq.load(Ordering::Acquire);
        if first == self.last_seq || first % 2 == 1 {
            return None;
        }
        fence(Ordering::SeqCst);
        let p = self.raw();
        let count = p.count.min(MAX_ADDRS as u32);
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            out.push(p.addrs[i].to_ip());
        }
        let expires = p.expires_mono_secs;
        fence(Ordering::SeqCst);
        let second = self.raw().seq.load(Ordering::Acquire);
        if first != second {
            return None; // torn read — retry on the next pass
        }
        let _ = expires;
        self.last_seq = first;
        Some(out)
    }

    /// Blocking read of the current set (used once at child startup; the
    /// supervisor always publishes before forking, so this never spins long).
    pub fn read_initial(&mut self) -> Vec<IpAddr> {
        loop {
            if let Some(v) = self.read_if_changed() {
                if !v.is_empty() {
                    return v;
                }
            }
            std::thread::yield_now();
        }
    }
}

impl Drop for ResolveReader {
    fn drop(&mut self) {
        unsafe {
            let _ = nix::sys::mman::munmap(
                std::ptr::NonNull::new_unchecked(self.ptr.cast_mut().cast::<std::ffi::c_void>()),
                SHM_SIZE,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn write_then_read_roundtrip() {
        let (fd, mut writer) = ResolveWriter::create().unwrap();
        let ips = vec![
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            IpAddr::V6("2001:4860:4860::8888".parse().unwrap()),
        ];
        writer.publish(&ips, Instant::now() + Duration::from_secs(300));

        let mut reader = ResolveReader::map_readonly(std::os::fd::AsRawFd::as_raw_fd(&fd)).unwrap();
        let got = reader.read_if_changed().expect("first read must see data");
        assert_eq!(got, ips);
        // Unchanged since last read → None.
        assert!(reader.read_if_changed().is_none());
        assert!(!reader.is_stale());
    }

    #[test]
    fn republish_is_seen() {
        let (fd, mut writer) = ResolveWriter::create().unwrap();
        let mut reader = ResolveReader::map_readonly(std::os::fd::AsRawFd::as_raw_fd(&fd)).unwrap();
        writer.publish(&[IpAddr::V4(Ipv4Addr::LOCALHOST)], Instant::now() + Duration::from_secs(60));
        assert_eq!(reader.read_if_changed().unwrap().len(), 1);
        writer.publish(
            &[
                IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
                IpAddr::V4(Ipv4Addr::new(1, 0, 0, 1)),
            ],
            Instant::now() + Duration::from_secs(60),
        );
        let got = reader.read_if_changed().unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)));
    }

    #[test]
    fn stale_detection() {
        let (_fd, mut writer) = ResolveWriter::create().unwrap();
        writer.publish(&[IpAddr::V4(Ipv4Addr::LOCALHOST)], Instant::now());
        // Read back via a fresh reader on the same fd.
        let mut reader = ResolveReader::map_readonly(std::os::fd::AsRawFd::as_raw_fd(&_fd)).unwrap();
        let _ = reader.read_if_changed();
        // expires_mono_secs == mono_now (0-duration) → stale almost immediately.
        // Give it a second boundary.
        std::thread::sleep(Duration::from_millis(1100));
        assert!(reader.is_stale());
    }

    #[test]
    fn raw_addr_roundtrip() {
        let v4 = RawAddr::from_ip(IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9)));
        assert_eq!(v4.to_ip(), IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9)));
        let v6 = RawAddr::from_ip("2001:db8::1".parse::<IpAddr>().unwrap());
        assert_eq!(v6.to_ip(), "2001:db8::1".parse::<IpAddr>().unwrap());
    }
}
