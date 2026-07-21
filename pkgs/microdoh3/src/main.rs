//! microdoh3 — minimal DNS-over-HTTP/3 proxy.
//!
//! Prefork model: the supervisor parses the CLI, then forks one child per
//! physical CPU core. Each child pins itself to its core, opens its own
//! SO_REUSEPORT DNS socket and its own QUIC (HTTP/3) connection upstream,
//! and runs a single-threaded epoll event loop — no async runtime anywhere.

mod base64url;
mod bootstrap;
mod dns;
mod event;
mod h3;
mod huffman_table;
mod qpack;
mod qpack_static;
mod quic;
mod url;
mod worker;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::process::exit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use nix::sys::signal::{kill, sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{fork, ForkResult, Pid};

use worker::{WorkerConfig, run as worker_run};

/// DNS-over-HTTP/3 proxy (QUIC 0-RTT, prefork, per-core pinning).
#[derive(Parser, Debug)]
#[command(name = "microdoh3", version)]
pub struct Cli {
    /// Address to listen on for DNS queries (UDP).
    #[arg(long, short = 'l', default_value = "0.0.0.0:5300")]
    pub listen: String,

    /// DoH upstream URL (HTTP/3 only, e.g. `https://dns.google/dns-query`).
    #[arg(long, short = 'u', default_value = "https://dns.google/dns-query")]
    pub upstream: String,

    /// Bearer token for `Authorization` header. Read from `$MICRODOH_TOKEN` if not given.
    #[arg(long, env = "MICRODOH_TOKEN")]
    pub token: Option<String>,

    /// Read the bearer token from this file (overrides --token / env).
    #[arg(long)]
    pub token_file: Option<String>,

    /// Bootstrap DNS server for resolving the DoH upstream hostname.
    #[arg(long, default_value = "8.8.8.8")]
    pub bootstrap_dns: String,

    /// Request timeout in seconds.
    #[arg(long, default_value = "30")]
    pub timeout_secs: u64,

    /// Pad DNS queries with EDNS0 padding to 128-byte blocks (RFC 8467).
    #[arg(long)]
    pub pad: bool,

    /// Number of worker processes (0 = one per physical CPU core).
    #[arg(long, default_value = "0")]
    pub workers: u32,

    /// Comma-separated CPU core IDs to pin workers to (overrides --workers).
    #[arg(long)]
    pub cpus: Option<String>,

    /// Enable SO_BUSY_POLL on the DNS socket (lower latency, more CPU).
    #[arg(long)]
    pub busy_poll: bool,

    /// Do a non-blocking event sweep before sleeping in epoll.
    #[arg(long)]
    pub spin: bool,

    /// Lock all current and future memory (avoid page faults in hot path).
    #[arg(long)]
    pub mlockall: bool,

    /// Enable verbose logging (debug level).
    #[arg(long, short = 'v')]
    pub verbose: bool,
}

/// Supervisor shutdown flag set by the signal handler.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_supervisor_signal(_sig: i32) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Discover physical CPU cores via sysfs topology; returns representative
/// CPU IDs sorted by (package, core). Falls back to all online CPUs.
fn physical_cores() -> Vec<u32> {
    let mut cores: std::collections::BTreeMap<(i64, i64), u32> = Default::default();
    let mut fallback: Vec<u32> = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/sys/devices/system/cpu") {
        for entry in rd.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(cpu) = name.strip_prefix("cpu").and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            fallback.push(cpu);
            let topo = entry.path().join("topology");
            let pkg = std::fs::read_to_string(topo.join("physical_package_id"))
                .ok()
                .and_then(|s| s.trim().parse::<i64>().ok());
            let core = std::fs::read_to_string(topo.join("core_id"))
                .ok()
                .and_then(|s| s.trim().parse::<i64>().ok());
            if let (Some(pkg), Some(core)) = (pkg, core) {
                cores
                    .entry((pkg, core))
                    .and_modify(|e| *e = (*e).min(cpu))
                    .or_insert(cpu);
            }
        }
    }
    if cores.is_empty() {
        if fallback.is_empty() {
            // No sysfs (sandbox/container): fall back to the std probe.
            let n = std::thread::available_parallelism()
                .map(|v| v.get())
                .unwrap_or(1);
            return (0..n as u32).collect();
        }
        fallback.sort_unstable();
        fallback
    } else {
        cores.into_values().collect()
    }
}

fn pin_to_cpu(cpu: u32) {
    use nix::sched::{sched_setaffinity, CpuSet};
    let mut set = CpuSet::new();
    if set.set(cpu as usize).is_ok() {
        let _ = sched_setaffinity(Pid::from_raw(0), &set);
    }
}

fn main() {
    let cli = Cli::parse();
    let default_level = if cli.verbose { "debug" } else { "info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_level))
        .init();

    // ── Parse and validate configuration once, before forking ──
    let listen: SocketAddr = match cli.listen.parse() {
        Ok(a) => a,
        Err(e) => {
            log::error!("invalid --listen {}: {e}", cli.listen);
            exit(2);
        }
    };
    let upstream = match url::HttpsUrl::parse(&cli.upstream) {
        Ok(u) => u,
        Err(e) => {
            log::error!("invalid --upstream: {e}");
            exit(2);
        }
    };
    let bootstrap_dns: IpAddr = match cli.bootstrap_dns.parse() {
        Ok(a) => a,
        Err(e) => {
            log::error!("invalid --bootstrap-dns: {e}");
            exit(2);
        }
    };
    let token: Option<Arc<str>> = if let Some(ref path) = cli.token_file {
        match std::fs::read_to_string(path) {
            Ok(s) => Some(Arc::from(s.trim())),
            Err(e) => {
                log::error!("cannot read --token-file {path}: {e}");
                exit(2);
            }
        }
    } else {
        cli.token.as_deref().map(Arc::from)
    };

    // ── Determine worker count and CPU assignments ──
    let cores = physical_cores();
    let cpus: Vec<u32> = if let Some(ref list) = cli.cpus {
        match list
            .split(',')
            .map(|s| s.trim().parse::<u32>())
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(v) if !v.is_empty() => v,
            _ => {
                log::error!("invalid --cpus list: {list}");
                exit(2);
            }
        }
    } else {
        let n = if cli.workers == 0 {
            cores.len()
        } else {
            (cli.workers as usize).min(cores.len())
        };
        cores[..n.max(1)].to_vec()
    };

    log::info!(
        "microdoh3: {} worker(s) on cpu(s) {:?}, upstream https://{}:{}{}",
        cpus.len(),
        cpus,
        upstream.host,
        upstream.port,
        upstream.path
    );

    // ── Install supervisor signal handlers ──
    let action = SigAction::new(
        SigHandler::Handler(on_supervisor_signal),
        SaFlags::empty(), // no SA_RESTART: we want waitpid interrupted
        SigSet::empty(),
    );
    // NOTE: no SIGCHLD handler — waitpid already wakes on child death, and a
    // handler would clobber the SHUTDOWN flag semantics.
    unsafe {
        let _ = sigaction(Signal::SIGTERM, &action);
        let _ = sigaction(Signal::SIGINT, &action);
    }

    // ── Prefork workers ──
    // pid → (cpu, started_at, fast_failures)
    let mut children: HashMap<Pid, (u32, Instant, u32)> = HashMap::new();
    for &cpu in &cpus {
        spawn_worker(cpu, &cli, &listen, &upstream, &bootstrap_dns, &token, &mut children);
    }

    // ── Supervise: restart on crash, kill all on shutdown ──
    loop {
        match waitpid(None, None) {
            Ok(WaitStatus::Exited(pid, code)) => {
                log::warn!("worker {pid} exited with code {code}");
                handle_child_exit(pid, &cli, &listen, &upstream, &bootstrap_dns, &token, &mut children);
            }
            Ok(WaitStatus::Signaled(pid, sig, _)) => {
                log::warn!("worker {pid} killed by {sig}");
                handle_child_exit(pid, &cli, &listen, &upstream, &bootstrap_dns, &token, &mut children);
            }
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => {}
            Err(nix::errno::Errno::ECHILD) => {
                if SHUTDOWN.load(Ordering::SeqCst) || children.is_empty() {
                    break;
                }
            }
            Err(e) => {
                log::error!("waitpid: {e}");
                break;
            }
        }

        if SHUTDOWN.load(Ordering::SeqCst) {
            if children.is_empty() {
                break;
            }
            log::info!("supervisor: terminating {} worker(s)", children.len());
            for pid in children.keys() {
                let _ = kill(*pid, Signal::SIGTERM);
            }
            // Give children a moment, then SIGKILL stragglers.
            std::thread::sleep(Duration::from_millis(500));
            for pid in children.keys() {
                let _ = kill(*pid, Signal::SIGKILL);
            }
            children.clear();
            break;
        }
    }
    log::info!("supervisor: exit");
}

#[allow(clippy::too_many_arguments)]
fn handle_child_exit(
    pid: Pid,
    cli: &Cli,
    listen: &SocketAddr,
    upstream: &url::HttpsUrl,
    bootstrap_dns: &IpAddr,
    token: &Option<Arc<str>>,
    children: &mut HashMap<Pid, (u32, Instant, u32)>,
) {
    let Some((cpu, started, fails)) = children.remove(&pid) else {
        return;
    };
    if SHUTDOWN.load(Ordering::SeqCst) {
        return;
    }
    // Backoff if the child died quickly after start.
    let lived = started.elapsed();
    let fails = if lived < Duration::from_secs(10) {
        fails + 1
    } else {
        0
    };
    if fails > 0 {
        let delay = Duration::from_secs(1 << fails.min(5)); // 2..32s
        log::warn!("worker on cpu {cpu} died after {lived:.1?}; restart #{fails} in {delay:.1?}");
        std::thread::sleep(delay);
    }
    spawn_worker(cpu, cli, listen, upstream, bootstrap_dns, token, children);
}

#[allow(clippy::too_many_arguments)]
fn spawn_worker(
    cpu: u32,
    cli: &Cli,
    listen: &SocketAddr,
    upstream: &url::HttpsUrl,
    bootstrap_dns: &IpAddr,
    token: &Option<Arc<str>>,
    children: &mut HashMap<Pid, (u32, Instant, u32)>,
) {
    let fails = children.values().find(|(c, _, _)| *c == cpu).map(|(_, _, f)| *f).unwrap_or(0);
    match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => {
            children.insert(child, (cpu, Instant::now(), fails));
            log::info!("worker pid {child} on cpu {cpu}");
        }
        Ok(ForkResult::Child) => {
            // Child: pin to core and run. All crypto/RNG init happens here,
            // after the fork. Never return to the supervisor.
            pin_to_cpu(cpu);
            let cfg = WorkerConfig {
                listen: *listen,
                upstream: upstream.clone(),
                bootstrap_dns: *bootstrap_dns,
                token: token.clone(),
                timeout: Duration::from_secs(cli.timeout_secs),
                pad: cli.pad,
                busy_poll: cli.busy_poll,
                spin: cli.spin,
                mlockall: cli.mlockall,
            };
            match worker_run(cfg) {
                Ok(()) => exit(0),
                Err(e) => {
                    log::error!("worker on cpu {cpu} failed: {e}");
                    exit(1);
                }
            }
        }
        Err(e) => {
            log::error!("fork: {e}");
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_cores_nonempty() {
        let cores = physical_cores();
        assert!(!cores.is_empty());
    }

    #[test]
    fn cli_defaults() {
        let cli = Cli::parse_from(["microdoh3"]);
        assert_eq!(cli.listen, "0.0.0.0:5300");
        assert_eq!(cli.upstream, "https://dns.google/dns-query");
        assert_eq!(cli.bootstrap_dns, "8.8.8.8");
        assert_eq!(cli.timeout_secs, 30);
        assert_eq!(cli.workers, 0);
        assert!(cli.cpus.is_none());
        assert!(!cli.pad);
        assert!(!cli.busy_poll);
        assert!(!cli.spin);
        assert!(!cli.mlockall);
    }

    #[test]
    fn cli_custom() {
        let cli = Cli::parse_from([
            "microdoh3",
            "-l", "[::1]:5443",
            "-u", "https://dns.nextdns.io/abc",
            "--bootstrap-dns", "1.1.1.1",
            "--timeout-secs", "10",
            "--token", "secret",
            "--workers", "2",
            "--cpus", "0,2",
            "--pad",
            "--busy-poll",
        ]);
        assert_eq!(cli.listen, "[::1]:5443");
        assert_eq!(cli.timeout_secs, 10);
        assert_eq!(cli.token.as_deref(), Some("secret"));
        assert_eq!(cli.workers, 2);
        assert_eq!(cli.cpus.as_deref(), Some("0,2"));
        assert!(cli.pad);
        assert!(cli.busy_poll);
    }
}
