//! curl Multi worker thread.
//!
//! Runs a `curl::multi::Multi` handle on a dedicated OS thread.
//! Uses `curl_multi_timeout()` + `multi.poll()` + `multi.perform()` for
//! cross‑platform event driving, with stall detection to prevent 100% CPU
//! when transfers are stuck.
//!
//! Communicates with the tokio UDP listener via channels.

use std::collections::HashMap;
use std::net::IpAddr;
use std::os::raw::c_long;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use curl::easy::Easy2;
use curl::multi::{Easy2Handle, Multi};
use tokio::sync::{mpsc, oneshot};

use crate::doh::{
    build_easy2_request, ensure_fresh, DnsRuntime, DohHandler,
};

/// Max new tasks to drain per loop iteration to avoid response starvation.
const MAX_DRAIN_PER_LOOP: usize = 64;



/// Rate‑limit for `ensure_fresh()` (seconds).
const DNS_REFRESH_INTERVAL_SECS: u64 = 60;

use crate::error::Result;
use crate::udp::DnsTask;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Spawn the curl worker thread.
pub fn spawn(
    rx: mpsc::UnboundedReceiver<DnsTask>,
    upstream: String,
    host: String,
    port: u16,
    bootstrap_dns: IpAddr,
    token: Option<Arc<str>>,
    timeout_secs: u64,
    verbose: bool,
    pad: bool,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("curl-worker".into())
        .spawn(move || {
            if let Err(e) = worker_loop(rx, &upstream, &host, port, bootstrap_dns, &token, timeout_secs, verbose, pad) {
                log::error!("curl worker exiting: {e}");
            }
        })
        .expect("failed to spawn curl worker thread")
}

// ---------------------------------------------------------------------------
// Pending transfer tracking
// ---------------------------------------------------------------------------

/// One in-flight transfer, keyed by a monotonically increasing token.
struct Pending {
    handle: Easy2Handle<DohHandler>,
    resp_tx: oneshot::Sender<Vec<u8>>,
    added_at: Instant,
}

// ---------------------------------------------------------------------------
// Worker loop (blocking, runs on its own OS thread)
// ---------------------------------------------------------------------------

fn worker_loop(
    mut rx: mpsc::UnboundedReceiver<DnsTask>,
    upstream: &str,
    host: &str,
    port: u16,
    bootstrap_dns: IpAddr,
    token: &Option<Arc<str>>,
    timeout_secs: u64,
    verbose: bool,
    pad: bool,
) -> Result<()> {
    // ── Bootstrap DNS runtime ──
    let dns_rt = DnsRuntime::new(bootstrap_dns)?;
    let state = Arc::new(RwLock::new(dns_rt.resolve(host, port)?));

    // ── curl Multi handle ──
    let mut multi = Multi::new();

    // ── Pending transfers keyed by token ──
    let mut pending: HashMap<usize, Pending> = HashMap::new();
    let mut next_token: usize = 0;

    // ── Rate‑limit for ensure_fresh ──
    let mut last_dns_check = Instant::now();

    // ── Kickstart ──
    multi.perform().map_err(|e| crate::error::Error::curl_multi(e))?;

    // ── Main event loop using curl_multi_timeout() ──
    loop {
        // 0. Ensure bootstrap DNS is fresh (rate‑limited).
        if last_dns_check.elapsed() > Duration::from_secs(DNS_REFRESH_INTERVAL_SECS) {
            ensure_fresh(&state, &dns_rt, host, port);
            last_dns_check = Instant::now();
        }

        // 1. Drain new tasks from the channel (non-blocking, bounded).
        let mut drained = 0;
        while drained < MAX_DRAIN_PER_LOOP {
            match rx.try_recv() {
                Ok(task) => {
                    let resolve_state = state.read().unwrap();
                    let easy = build_easy2_request(
                        task.query,
                        upstream,
                        token.as_deref(),
                        &resolve_state,
                        timeout_secs,
                        verbose,
                        pad,
                    );
                    drop(resolve_state);
                    add_transfer(&mut multi, &mut pending, &mut next_token, easy, task.resp_tx);
                    drained += 1;
                }
                Err(_) => break,
            }
        }

        // 2. Drive transfers.
        let running = multi.perform()
            .map_err(|e| crate::error::Error::curl_multi(e))?;

        // 3. Exit if everything is done.
        if running == 0 && rx.is_closed() && pending.is_empty() {
            break;
        }

        // 4. Ask libcurl how long to wait — the core fix.
        let mut timeout_ms: c_long = -1;
        unsafe {
            curl_sys::curl_multi_timeout(multi.raw(), &mut timeout_ms);
        }

        if running == 0 {
            // No running transfers — block until a new query arrives.
            match rx.blocking_recv() {
                Some(task) => {
                    let resolve_state = state.read().unwrap();
                    let easy = build_easy2_request(
                        task.query,
                        upstream,
                        token.as_deref(),
                        &resolve_state,
                        timeout_secs,
                        verbose,
                        pad,
                    );
                    drop(resolve_state);
                    add_transfer(&mut multi, &mut pending, &mut next_token, easy, task.resp_tx);
                }
                None => {
                    // Channel closed — finish remaining work.
                }
            }
        } else {
            // libcurl says work NOW (0), has no timers (-1), or wait N ms (>0).
            // Use curl_multi_poll via FFI instead of curl_multi_wait:
            // poll() waits even when there are no fds to watch, preventing
            // busy‑spin.  multi_wait() returns immediately with no fds.
            let wait_ms: std::os::raw::c_int = if timeout_ms <= 0 {
                100  // poll will return early if there IS activity
            } else {
                ((timeout_ms as u64).min(1000)) as std::os::raw::c_int
            };
            let mut numfds: std::os::raw::c_int = 0;
            let rc = unsafe {
                curl_sys::curl_multi_poll(
                    multi.raw(),
                    std::ptr::null_mut(),
                    0,
                    wait_ms,
                    &mut numfds,
                )
            };
            if rc != curl_sys::CURLM_OK as i32 {
                log::warn!("curl_multi_poll failed: rc={rc}");
            }
        }

        // 5. Drive again after wait returns.
        multi.perform()
            .map_err(|e| crate::error::Error::curl_multi(e))?;

        // 6. Collect completed transfers with diagnostics.
        collect_completed(&multi, &mut pending, timeout_secs);

        // 7. Check if done.
        if rx.is_closed() && pending.is_empty() {
            break;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Add a new easy handle to the multi, tracking it in `pending`.
fn add_transfer(
    multi: &Multi,
    pending: &mut HashMap<usize, Pending>,
    next_token: &mut usize,
    easy: Easy2<DohHandler>,
    resp_tx: oneshot::Sender<Vec<u8>>,
) {
    let tok = *next_token;
    *next_token += 1;

    match multi.add2(easy) {
        Ok(mut handle) => {
            if let Err(e) = handle.set_token(tok) {
                log::warn!("set_token: {e}");
            }
            pending.insert(tok, Pending {
                handle,
                resp_tx,
                added_at: Instant::now(),
            });
        }
        Err(e) => {
            log::warn!("multi.add2: {e}");
        }
    }
}

/// Walk completed messages, extract responses, send them back, and remove
/// the easy handle from the multi and from `pending`.
///
/// Also detects transfers that have exceeded their timeout and force‑removes
/// them to break stall spirals.
fn collect_completed(
    multi: &Multi,
    pending: &mut HashMap<usize, Pending>,
    timeout_secs: u64,
) {
    // ── Collect completion messages from libcurl ──
    let mut completed: Vec<(usize, Option<i32>)> = Vec::new();

    multi.messages(|msg| {
        let token = match msg.token() {
            Ok(t) => t,
            Err(e) => {
                log::warn!("msg.token(): {e}");
                return;
            }
        };
        // msg.result() returns Option<Result<(), curl::Error>>
        let err_code = match msg.result() {
            Some(Ok(())) => Some(0),  // CURLE_OK
            Some(Err(e)) => Some(e.code() as i32),
            None => None,
        };
        completed.push((token, err_code));
    });

    for (tok, err_code) in completed {
        if let Some(mut p) = pending.remove(&tok) {
            match err_code {
                Some(0) => {
                    // CURLE_OK — transfer succeeded.
                    let response = std::mem::take(&mut p.handle.get_mut().response);
                    let len = response.len();
                    let _ = p.resp_tx.send(response);
                    log::debug!("transfer {tok}: OK ({len} bytes)");
                }
                Some(code) => {
                    // Transfer failed with a CURLcode.
                    log::warn!(
                        "transfer {tok}: FAILED code={code} ({})",
                        curl_code_name(code),
                    );
                    // Drop resp_tx → oneshot returns Canceled → UDP client retries.
                    drop(p.resp_tx);
                }
                None => {
                    // msg.result() was None — shouldn't happen for a completed msg.
                    log::warn!("transfer {tok}: completed with no result");
                    drop(p.resp_tx);
                }
            }

            // Remove from multi (explicit, not just via Drop).
            if let Err(e) = multi.remove2(p.handle) {
                log::warn!("multi.remove2({tok}): {e}");
            }
        }
    }

    // ── Force‑remove transfers that have exceeded their timeout ──
    let grace = Duration::from_secs(timeout_secs + 5);
    let expired_tokens: Vec<usize> = pending
        .iter()
        .filter(|(_, p)| p.added_at.elapsed() > grace)
        .map(|(&tok, _)| tok)
        .collect();

    for tok in expired_tokens {
        if let Some(p) = pending.remove(&tok) {
            log::warn!(
                "transfer {tok}: force‑removed after {:.1}s (exceeded timeout+grace)",
                p.added_at.elapsed().as_secs_f64(),
            );
            if let Err(e) = multi.remove2(p.handle) {
                log::warn!("multi.remove2({tok}): {e}");
            }
            drop(p.resp_tx);
        }
    }
}

/// Map a CURLcode integer to a human‑readable name.
fn curl_code_name(code: i32) -> &'static str {
    match code {
        0 => "CURLE_OK",
        1 => "CURLE_UNSUPPORTED_PROTOCOL",
        2 => "CURLE_FAILED_INIT",
        3 => "CURLE_URL_MALFORMAT",
        5 => "CURLE_COULDNT_RESOLVE_PROXY",
        6 => "CURLE_COULDNT_RESOLVE_HOST",
        7 => "CURLE_COULDNT_CONNECT",
        9 => "CURLE_REMOTE_ACCESS_DENIED",
        22 => "CURLE_HTTP_RETURNED_ERROR",
        23 => "CURLE_WRITE_ERROR",
        26 => "CURLE_READ_ERROR",
        28 => "CURLE_OPERATION_TIMEDOUT",
        35 => "CURLE_SSL_CONNECT_ERROR",
        47 => "CURLE_TOO_MANY_REDIRECTS",
        52 => "CURLE_GOT_NOTHING",
        55 => "CURLE_SEND_ERROR",
        56 => "CURLE_RECV_ERROR",
        58 => "CURLE_SSL_CERTPROBLEM",
        60 => "CURLE_SSL_CACERT",
        77 => "CURLE_SSL_CACERT_BADFILE",
        83 => "CURLE_SSL_ISSUER_ERROR",
        91 => "CURLE_SSL_CRL_BADFILE",
        92 => "CURLE_SSL_SHUTDOWN_FAILED",
        97 => "CURLE_AUTH_ERROR",
        _ => "CURL_UNKNOWN",
    }
}
