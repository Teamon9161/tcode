//! The HTTP client every provider streams over.
//!
//! Connection hygiene is not a detail here. Between two model requests the
//! agent runs tools — often for minutes — and the pooled connection sits idle
//! the whole time. A NAT, proxy or load balancer that quietly drops it in the
//! meantime leaves the pool holding a corpse: the next request is written onto
//! it and no reply ever comes. That surfaces as `ConnectTimeout` ("no response
//! from the API"), reliably right after a long tool call, which makes it look
//! as if the tool's own runtime were being counted against the request. It is
//! not — the connection was simply already dead when we reused it.
//!
//! So: ping HTTP/2 connections while they are idle (a dead one is discovered
//! before we build a request on it), keep TCP alive underneath, and retire
//! pooled connections that idled long enough to be doubtful.

use std::time::Duration;

/// TCP/TLS setup only. The wait for *response headers* is a separate budget
/// (`watchdog.connect_timeout`), applied per attempt by the agent loop.
const TCP_CONNECT: Duration = Duration::from_secs(10);
/// Drop pooled connections idle longer than this instead of gambling that the
/// peer still has them.
const POOL_IDLE: Duration = Duration::from_secs(45);
const TCP_KEEPALIVE: Duration = Duration::from_secs(30);
const H2_PING_INTERVAL: Duration = Duration::from_secs(20);
const H2_PING_TIMEOUT: Duration = Duration::from_secs(10);

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(TCP_CONNECT)
        .pool_idle_timeout(POOL_IDLE)
        .tcp_keepalive(TCP_KEEPALIVE)
        .http2_keep_alive_interval(H2_PING_INTERVAL)
        .http2_keep_alive_timeout(H2_PING_TIMEOUT)
        .http2_keep_alive_while_idle(true)
        .build()
        .expect("reqwest client")
}
