//! epoll + timerfd + signal self-pipe wrapper (via `nix`; no async runtime).

use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::socket::{
    socketpair, AddressFamily, SockFlag, SockType,
};
use nix::sys::time::TimeSpec;
use nix::sys::timerfd::{ClockId, Expiration, TimerFd, TimerFlags, TimerSetTimeFlags};

/// Event tokens registered in the epoll set.
pub const TOKEN_DNS: u64 = 0;
pub const TOKEN_QUIC: u64 = 1;
pub const TOKEN_TIMER: u64 = 2;
pub const TOKEN_SIGNAL: u64 = 3;

/// Write end of the signal self-pipe (raw fd for the signal handler).
static SIG_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(_sig: i32) {
    let fd = SIG_WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        // write(2) is async-signal-safe; errors are irrelevant (pipe full).
        unsafe {
            nix::libc::write(fd, b"x".as_ptr().cast(), 1);
        }
    }
}

pub struct Poller {
    epoll: Epoll,
    timer: TimerFd,
    sig_read: OwnedFd,
}

impl Poller {
    pub fn new() -> io::Result<Self> {
        let epoll = Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC)?;
        let timer = TimerFd::new(
            ClockId::CLOCK_MONOTONIC,
            TimerFlags::TFD_NONBLOCK | TimerFlags::TFD_CLOEXEC,
        )?;
        let (sig_read, sig_write) = socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
        )?;
        SIG_WRITE_FD.store(sig_write.as_raw_fd(), Ordering::Relaxed);
        std::mem::forget(sig_write); // keep the write end open for the handler

        // SIGTERM / SIGINT → self-pipe; SIGPIPE must be ignored (UDP sends).
        let action = SigAction::new(
            SigHandler::Handler(on_signal),
            SaFlags::empty(), // no SA_RESTART: let epoll_wait wake up
            SigSet::empty(),
        );
        unsafe {
            let _ = sigaction(Signal::SIGTERM, &action);
            let _ = sigaction(Signal::SIGINT, &action);
            let _ = sigaction(
                Signal::SIGPIPE,
                &SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty()),
            );
        }

        epoll.add(&timer, EpollEvent::new(EpollFlags::EPOLLIN, TOKEN_TIMER))?;
        epoll.add(&sig_read, EpollEvent::new(EpollFlags::EPOLLIN, TOKEN_SIGNAL))?;

        Ok(Self {
            epoll,
            timer,
            sig_read,
        })
    }

    /// Register a socket fd with a token.
    pub fn add_socket(&self, fd: &impl AsFd, token: u64) -> io::Result<()> {
        self.epoll
            .add(fd, EpollEvent::new(EpollFlags::EPOLLIN, token))?;
        Ok(())
    }

    /// Arm the timer for `deadline` (relative; recomputed each loop).
    /// `None` disarms.
    pub fn arm_timer(&self, deadline: Option<Instant>) -> io::Result<()> {
        match deadline {
            Some(d) => {
                let rel = d.saturating_duration_since(Instant::now());
                // Clamp to at least 1ns: a zero TimeSpec would disarm.
                let rel = rel.max(Duration::from_nanos(1));
                self.timer.set(
                    Expiration::OneShot(TimeSpec::new(
                        rel.as_secs() as _,
                        rel.subsec_nanos() as _,
                    )),
                    TimerSetTimeFlags::empty(),
                )?;
            }
            None => self.timer.unset()?,
        }
        Ok(())
    }

    /// Drain timer expirations (after epoll reports TOKEN_TIMER).
    pub fn drain_timer(&self) {
        let mut buf = [0u8; 8];
        loop {
            match nix::unistd::read(&self.timer, &mut buf) {
                Ok(_) => continue,
                Err(Errno::EAGAIN) => break,
                Err(_) => break,
            }
        }
    }

    /// Check whether a shutdown signal arrived (drains the self-pipe).
    pub fn shutdown_signaled(&self) -> bool {
        let mut buf = [0u8; 16];
        match nix::unistd::read(&self.sig_read, &mut buf) {
            Ok(n) => n > 0,
            Err(_) => false,
        }
    }

    /// Wait for events. `spin_us` > 0 enables a short zero-timeout poll
    /// before blocking (lower wake latency at the cost of CPU).
    pub fn wait(&self, events: &mut [EpollEvent], spin: bool) -> io::Result<usize> {
        if spin {
            // One non-blocking sweep before blocking indefinitely.
            if let Ok(n) = self.epoll.wait(events, EpollTimeout::ZERO) {
                if n > 0 {
                    return Ok(n);
                }
            }
        }
        loop {
            match self.epoll.wait(events, EpollTimeout::NONE) {
                Ok(n) => return Ok(n),
                Err(Errno::EINTR) => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_fires() {
        let p = Poller::new().unwrap();
        p.arm_timer(Some(Instant::now() + Duration::from_millis(10)))
            .unwrap();
        let mut events = [EpollEvent::empty(); 4];
        let n = p.wait(&mut events, false).unwrap();
        assert!(n >= 1);
        assert_eq!(events[0].data(), TOKEN_TIMER);
        p.drain_timer();
    }

    #[test]
    fn timer_disarm() {
        let p = Poller::new().unwrap();
        p.arm_timer(Some(Instant::now() + Duration::from_secs(60)))
            .unwrap();
        p.arm_timer(None).unwrap();
        // Non-blocking wait should report nothing.
        let mut events = [EpollEvent::empty(); 4];
        let n = p.epoll.wait(&mut events, EpollTimeout::ZERO).unwrap();
        assert_eq!(n, 0);
    }
}
