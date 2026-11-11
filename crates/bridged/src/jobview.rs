//! The job view's HTTP side (2.11b's `web/jobview/WIRING.md`, served by the daemon;
//! 20 Oct).
//!
//! * **Binds.** 127.0.0.1 by default. In hub mode also the laptop's tailnet address
//!   (100.64.0.0/10 or `fd7a:115c:a1e0::/48`). Never `0.0.0.0` or `::`, never a LAN address:
//!   [`check_view_addr`] refuses such a config before anything is bound. A connection from any
//!   other source address gets 403 ([`peer_allowed`]), and a `Host` that is not an IP literal or
//!   `localhost` gets 403 (a DNS-rebinding page cannot read the jobs).
//! * **Static files.** The built `web/jobview/dist/` at `/`, with the per-start token inserted as
//!   `<meta name="agentroid-token" content="…">` straight after `<head>`. Only plain relative
//!   paths are served (no `..`, no `%`, no backslash); `/assets/*` may be cached for a year,
//!   everything else is `no-store`.
//! * **Every response** carries the page's CSP, `X-Content-Type-Options: nosniff`, a `Date`
//!   (the page's clock skew comes from it) and `Connection: close` (one request per connection).
//! * **Endpoints.** `GET /jobs`, `GET /jobs/:id`, `GET /jobs/:id/events` (NDJSON backlog after
//!   `?after=`, or SSE with `id:` = seq, one NDJSON line per `data:`, `Last-Event-ID` honoured,
//!   `event: job` frames on change, 204 once an ended job has nothing more), `GET
//!   /jobs/:id/frame.jpg` (the flight recorder's newest JPEG, at most 61,440 bytes), and `POST
//!   /jobs/:id/control` (token in `X-Agentroid-Token`, compared in constant time; `Origin`, when
//!   present, must be `http://<Host>`). `frame.json`, `recorder.json` and `recorder/crop.jpg`
//!   answer 404: their schemas are only proposed (WIRING §6.5) and the screen size and crop they
//!   need are not recorded yet.
//! * The port is written to `<state_dir>/jobview.json` (next to the pipe name's state).
//!
//! This module holds the daemon's only TCP listener of its own (the link hub's is in
//! `crates/link-hub`).

use crate::runner::journal::write_atomic;
use crate::server::Daemon;
use serde_json::{json, Value};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

/// The page's Content-Security-Policy (WIRING §2), on every response.
pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; style-src-attr 'unsafe-inline'; img-src 'self'; font-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";
/// Longest request head accepted.
pub const MAX_HEAD: usize = 16 * 1024;
/// Longest request body accepted (a control is a few bytes).
pub const MAX_BODY: usize = 4096;
/// Largest frame served (WIRING §3).
pub const MAX_FRAME: usize = 61_440;
/// How often an SSE stream looks for new events.
pub const SSE_POLL_MS: u64 = 250;
/// An SSE comment this often keeps proxies and the page's 15 s staleness check honest.
pub const SSE_PING_MS: u64 = 10_000;
/// Where the port is written, under the state directory.
pub const VIEW_FILE: &str = "jobview.json";
/// Seconds a client has to send its request head.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// 100.64.0.0/10 (Tailscale's IPv4 range) or `fd7a:115c:a1e0::/48` (its IPv6 range).
pub fn is_tailnet(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v) => {
            let o = v.octets();
            o[0] == 100 && (o[1] & 0xc0) == 64
        }
        IpAddr::V6(v) => {
            let s = v.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

/// May the view bind `ip`? Loopback always; the tailnet only in hub mode; nothing else.
pub fn check_view_addr(ip: IpAddr, hub_mode: bool) -> Result<(), String> {
    if ip.is_unspecified() {
        return Err(format!(
            "refusing to bind the job view to {ip}: that is every interface, the LAN included"
        ));
    }
    if ip.is_loopback() {
        return Ok(());
    }
    if is_tailnet(ip) {
        return if hub_mode {
            Ok(())
        } else {
            Err(format!(
                "{ip} is a tailnet address: the view binds it only in hub mode"
            ))
        };
    }
    Err(format!("refusing to bind the job view to {ip}: only 127.0.0.1, or the tailnet address in hub mode (never the LAN)"))
}

/// May a client at `ip` use the view?
pub fn peer_allowed(ip: IpAddr, hub_mode: bool) -> bool {
    let ip = ip.to_canonical();
    ip.is_loopback() || (hub_mode && is_tailnet(ip))
}

/// How to start the view.
#[derive(Debug, Clone)]
pub struct JobViewConfig {
    /// The built page (`web/jobview/dist/`).
    pub dist: PathBuf,
    /// The addresses to bind: 127.0.0.1, plus the tailnet address in hub mode.
    pub bind: Vec<IpAddr>,
    /// 0: any free port (the first bind's port is reused for the others).
    pub port: u16,
    pub hub_mode: bool,
    /// `jobview.json` is written here.
    pub state_dir: Option<PathBuf>,
}

impl JobViewConfig {
    /// 127.0.0.1 on any free port.
    pub fn localhost(dist: impl Into<PathBuf>) -> JobViewConfig {
        JobViewConfig {
            dist: dist.into(),
            bind: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            port: 0,
            hub_mode: false,
            state_dir: None,
        }
    }
}

/// What a connection handler needs.
pub struct ViewCtx {
    pub daemon: Arc<Daemon>,
    pub dist: PathBuf,
    pub token: String,
    pub hub_mode: bool,
}

/// The running view.
pub struct JobView {
    addrs: Vec<SocketAddr>,
    ctx: Arc<ViewCtx>,
    file: Option<PathBuf>,
    rt: Mutex<Option<tokio::runtime::Runtime>>,
}

fn new_token() -> Result<String, String> {
    let b = link_hub::crypto::random_bytes::<32>()
        .map_err(|e| format!("no randomness for the view token: {e}"))?;
    Ok(link_hub::crypto::hex(&b))
}

impl JobView {
    /// Check every address, bind them, and serve until [`JobView::shutdown`] (or drop).
    pub fn start(daemon: Arc<Daemon>, cfg: JobViewConfig) -> Result<JobView, String> {
        if cfg.bind.is_empty() {
            return Err("the job view needs an address (127.0.0.1)".into());
        }
        for ip in &cfg.bind {
            check_view_addr(*ip, cfg.hub_mode)?;
        }
        if !cfg.dist.join("index.html").is_file() {
            return Err(format!(
                "{} has no index.html: build web/jobview first (npm run build)",
                cfg.dist.display()
            ));
        }
        let ctx = Arc::new(ViewCtx {
            daemon,
            dist: cfg.dist.clone(),
            token: new_token()?,
            hub_mode: cfg.hub_mode,
        });
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("jobview")
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let binds = cfg.bind.clone();
        let port = cfg.port;
        // bind from a plain thread: the caller may itself be inside a runtime
        let listeners = std::thread::scope(|s| {
            s.spawn(|| {
                rt.block_on(async {
                    let mut out: Vec<TcpListener> = vec![];
                    let mut port = port;
                    for ip in binds {
                        let l = TcpListener::bind(SocketAddr::new(ip, port))
                            .await
                            .map_err(|e| format!("job view not bound on {ip}:{port}: {e}"))?;
                        port = l.local_addr().map_err(|e| e.to_string())?.port();
                        out.push(l);
                    }
                    Ok::<_, String>(out)
                })
            })
            .join()
        });
        let listeners = match listeners {
            Ok(Ok(l)) => l,
            // a runtime may not be dropped inside async code: shut it down without blocking
            Ok(Err(e)) => {
                rt.shutdown_background();
                return Err(e);
            }
            Err(_) => {
                rt.shutdown_background();
                return Err("the job view's bind panicked".into());
            }
        };
        let mut addrs = vec![];
        for l in &listeners {
            match l.local_addr() {
                Ok(a) => addrs.push(a),
                Err(e) => {
                    rt.shutdown_background();
                    return Err(e.to_string());
                }
            }
        }
        for l in listeners {
            rt.spawn(accept_loop(ctx.clone(), l));
        }
        let file = match &cfg.state_dir {
            Some(dir) => {
                let p = dir.join(VIEW_FILE);
                let urls: Vec<String> = addrs.iter().map(|a| format!("http://{a}/")).collect();
                let v = json!({"v": 1, "port": addrs[0].port(), "addrs": addrs.iter().map(ToString::to_string).collect::<Vec<_>>(), "urls": urls, "hub_mode": cfg.hub_mode, "pid": std::process::id()});
                if let Err(e) = write_atomic(&p, (v.to_string() + "\n").as_bytes()) {
                    rt.shutdown_background();
                    return Err(e);
                }
                Some(p)
            }
            None => None,
        };
        Ok(JobView {
            addrs,
            ctx,
            file,
            rt: Mutex::new(Some(rt)),
        })
    }

    /// The addresses actually bound.
    pub fn local_addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    /// `http://127.0.0.1:<port>/`.
    pub fn url(&self) -> String {
        format!("http://{}/", self.addrs[0])
    }

    /// This start's control token (also in the served `index.html`).
    pub fn token(&self) -> &str {
        &self.ctx.token
    }

    /// Stop serving (open SSE streams end with the runtime).
    pub fn shutdown(&self) {
        if let Some(rt) = self
            .rt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            rt.shutdown_background();
        }
        if let Some(f) = &self.file {
            let _ = std::fs::remove_file(f);
        }
    }
}

impl Drop for JobView {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn accept_loop(ctx: Arc<ViewCtx>, l: TcpListener) {
    loop {
        match l.accept().await {
            Ok((stream, peer)) => {
                let c = ctx.clone();
                tokio::spawn(async move { serve_stream(&c, stream, peer.ip()).await });
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

// ---- HTTP/1.1, one request per connection ----

/// One parsed request.
#[derive(Debug, Clone, Default)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    /// Names lower-cased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
    pub fn query_param(&self, name: &str) -> Option<&str> {
        self.query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v)
    }
}

fn find_head_end(b: &[u8]) -> Option<usize> {
    b.windows(4).position(|w| w == b"\r\n\r\n")
}

async fn read_request<S: AsyncRead + Unpin>(s: &mut S) -> Result<Request, (u16, String)> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    let end = loop {
        if let Some(e) = find_head_end(&buf) {
            break e;
        }
        if buf.len() > MAX_HEAD {
            return Err((431, "request head too large".into()));
        }
        let n = match tokio::time::timeout(HEAD_TIMEOUT, s.read(&mut chunk)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err((400, e.to_string())),
            Err(_) => return Err((408, "request head not received in time".into())),
        };
        if n == 0 {
            return Err((
                400,
                "connection closed before the request head ended".into(),
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..end])
        .map_err(|_| (400, "request head is not UTF-8".to_string()))?;
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split(' ');
    let (method, target, version) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    if method.is_empty()
        || !target.starts_with('/')
        || !version.starts_with("HTTP/1.")
        || parts.next().is_some()
    {
        return Err((400, "bad request line".into()));
    }
    let mut headers = vec![];
    for l in lines {
        let Some((n, v)) = l.split_once(':') else {
            return Err((400, "bad header line".into()));
        };
        headers.push((n.trim().to_ascii_lowercase(), v.trim().to_string()));
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut req = Request {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
        headers,
        body: vec![],
    };
    let len: usize = match req.header("content-length") {
        Some(v) => v
            .parse()
            .map_err(|_| (400, "bad Content-Length".to_string()))?,
        None => 0,
    };
    if req.header("transfer-encoding").is_some() {
        return Err((411, "send a Content-Length, not a chunked body".into()));
    }
    if len > MAX_BODY {
        return Err((413, format!("body over {MAX_BODY} bytes")));
    }
    let mut body = buf[end + 4..].to_vec();
    while body.len() < len {
        let n = match tokio::time::timeout(HEAD_TIMEOUT, s.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => n,
            _ => return Err((400, "body shorter than Content-Length".into())),
        };
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    req.body = body;
    Ok(req)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        _ => "Internal Server Error",
    }
}

/// An RFC 1123 date (`Tue, 20 Oct 2026 12:00:00 GMT`) for a Unix time in seconds.
pub fn http_date(unix_s: u64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = unix_s / 86_400;
    let secs = unix_s % 86_400;
    // civil_from_days (Howard Hinnant), for days since 1970-01-01
    let z = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let wd = DAYS[usize::try_from(days % 7).unwrap_or(0)];
    let mon = MONTHS[usize::try_from(m - 1).unwrap_or(0)];
    format!(
        "{wd}, {d:02} {mon} {y} {:02}:{:02}:{:02} GMT",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

fn now_date() -> String {
    http_date(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    )
}

fn head(
    status: u16,
    ctype: &str,
    len: Option<usize>,
    long_cache: bool,
    extra: &[(&str, String)],
) -> String {
    let mut h = format!("HTTP/1.1 {status} {}\r\nDate: {}\r\nContent-Security-Policy: {CSP}\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n", reason(status), now_date());
    h.push_str(if long_cache {
        "Cache-Control: public, max-age=31536000, immutable\r\n"
    } else {
        "Cache-Control: no-store\r\n"
    });
    if !ctype.is_empty() {
        h.push_str(&format!("Content-Type: {ctype}\r\n"));
    }
    if let Some(n) = len {
        h.push_str(&format!("Content-Length: {n}\r\n"));
    }
    for (k, v) in extra {
        h.push_str(&format!("{k}: {v}\r\n"));
    }
    h.push_str("\r\n");
    h
}

async fn send<S: AsyncWrite + Unpin>(
    s: &mut S,
    status: u16,
    ctype: &str,
    body: &[u8],
    long_cache: bool,
    extra: &[(&str, String)],
) {
    let h = head(status, ctype, Some(body.len()), long_cache, extra);
    let _ = s.write_all(h.as_bytes()).await;
    let _ = s.write_all(body).await;
    let _ = s.flush().await;
    let _ = s.shutdown().await;
}

async fn send_json<S: AsyncWrite + Unpin>(s: &mut S, status: u16, v: &Value) {
    send(
        s,
        status,
        "application/json; charset=utf-8",
        (v.to_string() + "\n").as_bytes(),
        false,
        &[],
    )
    .await;
}

async fn send_error<S: AsyncWrite + Unpin>(s: &mut S, status: u16, msg: &str) {
    send_json(s, status, &json!({"error": msg})).await;
}

/// Constant-time equality (length leaks; the token's length is public).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The host part of a `Host` header (`[::1]:8080` gives `::1`).
fn host_name(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next().unwrap_or_default();
    }
    match host.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => h,
        _ => host,
    }
}

/// `localhost` or an IP literal: a page served under some other name (DNS rebinding) is refused.
fn host_ok(req: &Request) -> bool {
    let Some(h) = req.header("host") else {
        return false;
    };
    let name = host_name(h);
    name.eq_ignore_ascii_case("localhost") || name.parse::<IpAddr>().is_ok()
}

fn content_type(p: &Path) -> &'static str {
    match p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "ndjson" => "application/x-ndjson; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// A request path as a file under `dist`, or `None` when it is not a plain relative path.
pub fn static_path(dist: &Path, path: &str) -> Option<PathBuf> {
    let rel = path.strip_prefix('/')?;
    let rel = if rel.is_empty() { "index.html" } else { rel };
    let plain = rel.split('/').all(|seg| {
        !seg.is_empty()
            && seg != "."
            && seg != ".."
            && seg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    });
    plain.then(|| {
        rel.split('/')
            .fold(dist.to_path_buf(), |p, seg| p.join(seg))
    })
}

/// `index.html` with the token's meta tag straight after `<head>`.
pub fn inject_token(html: &str, token: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<head")?;
    let close = start + lower[start..].find('>')? + 1;
    Some(format!(
        "{}\n<meta name=\"agentroid-token\" content=\"{token}\">{}",
        &html[..close],
        &html[close..]
    ))
}

async fn blocking<T: Send + 'static>(
    ctx: &ViewCtx,
    f: impl FnOnce(&Daemon) -> T + Send + 'static,
) -> Option<T> {
    let d = ctx.daemon.clone();
    tokio::task::spawn_blocking(move || f(&d)).await.ok()
}

/// Serve one connection from `peer` (the accept loop's, or a test's in-memory stream).
pub async fn serve_stream<S: AsyncRead + AsyncWrite + Unpin>(
    ctx: &ViewCtx,
    mut s: S,
    peer: IpAddr,
) {
    if !peer_allowed(peer, ctx.hub_mode) {
        return send_error(&mut s, 403, &format!("the job view answers this laptop (and the tailnet in hub mode) only; {peer} is refused")).await;
    }
    let req = match read_request(&mut s).await {
        Ok(r) => r,
        Err((code, msg)) => return send_error(&mut s, code, &msg).await,
    };
    if !host_ok(&req) {
        return send_error(&mut s, 403, "Host must be localhost or an IP address").await;
    }
    if req.path == "/jobs" || req.path.starts_with("/jobs/") {
        return api(ctx, &mut s, &req).await;
    }
    if req.method != "GET" {
        return send(
            &mut s,
            405,
            "text/plain; charset=utf-8",
            b"GET only\n",
            false,
            &[("Allow", "GET".into())],
        )
        .await;
    }
    let Some(file) = static_path(&ctx.dist, &req.path) else {
        return send_error(&mut s, 404, "not found").await;
    };
    let Ok(bytes) = std::fs::read(&file) else {
        return send_error(&mut s, 404, "not found").await;
    };
    let ctype = content_type(&file);
    if file == ctx.dist.join("index.html") {
        let Some(html) = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|h| inject_token(h, &ctx.token))
        else {
            return send_error(&mut s, 500, "index.html has no <head>").await;
        };
        return send(&mut s, 200, ctype, html.as_bytes(), false, &[]).await;
    }
    let long = req.path.starts_with("/assets/");
    send(&mut s, 200, ctype, &bytes, long, &[]).await;
}

async fn api<S: AsyncRead + AsyncWrite + Unpin>(ctx: &ViewCtx, s: &mut S, req: &Request) {
    let parts: Vec<&str> = req.path.trim_start_matches('/').split('/').collect();
    let id = parts.get(1).map(|x| x.to_string()).unwrap_or_default();
    let rest: Vec<&str> = parts.iter().skip(2).copied().collect();
    let want = if rest == ["control"] { "POST" } else { "GET" };
    if req.method != want {
        return send(
            s,
            405,
            "application/json; charset=utf-8",
            format!("{{\"error\":\"{want} only\"}}\n").as_bytes(),
            false,
            &[("Allow", want.into())],
        )
        .await;
    }
    match (parts.len(), rest.as_slice()) {
        (1, _) => {
            let v = blocking(ctx, Daemon::view_jobs).await.unwrap_or(Value::Array(vec![]));
            send_json(s, 200, &v).await;
        }
        (2, []) => match blocking(ctx, move |d| d.view_job(&id)).await.flatten() {
            Some(v) => send_json(s, 200, &v).await,
            None => send_error(s, 404, "no such job").await,
        },
        (_, ["events"]) => events(ctx, s, req, id).await,
        (_, ["frame.jpg"]) => match blocking(ctx, move |d| d.view_frame(&id)).await.flatten() {
            Some(f) if f.len() <= MAX_FRAME => send(s, 200, "image/jpeg", &f, false, &[]).await,
            Some(_) => send_error(s, 404, "the newest frame is over 61,440 bytes and is not served").await,
            None => send_error(s, 404, "no frame yet").await,
        },
        (_, ["frame.json"] | ["recorder.json"] | ["recorder", "crop.jpg"]) => send_error(s, 404, "not recorded yet: frame metadata and the recorder crop need the proposed schemas (WIRING 6.5)").await,
        (_, ["control"]) => control(ctx, s, req, id).await,
        _ => send_error(s, 404, "not found").await,
    }
}

async fn control<S: AsyncRead + AsyncWrite + Unpin>(
    ctx: &ViewCtx,
    s: &mut S,
    req: &Request,
    id: String,
) {
    let token_ok = req
        .header("x-agentroid-token")
        .is_some_and(|t| ct_eq(t.as_bytes(), ctx.token.as_bytes()));
    if !token_ok {
        return send_error(s, 403, "token refused").await;
    }
    if let Some(origin) = req.header("origin") {
        let host = req.header("host").unwrap_or_default();
        if origin != format!("http://{host}") {
            return send_error(s, 403, "origin refused").await;
        }
    }
    let Some(action) = serde_json::from_slice::<Value>(&req.body)
        .ok()
        .and_then(|v| v.get("action").and_then(Value::as_str).map(str::to_string))
    else {
        return send_error(s, 400, "body must be {\"action\": ...}").await;
    };
    match blocking(ctx, move |d| d.view_control(&id, &action)).await {
        Some(Ok(())) => send_json(s, 202, &json!({"accepted": true})).await,
        Some(Err((code, msg))) => send_error(s, code, &msg).await,
        None => send_error(s, 500, "the control did not finish").await,
    }
}

async fn events<S: AsyncRead + AsyncWrite + Unpin>(
    ctx: &ViewCtx,
    s: &mut S,
    req: &Request,
    id: String,
) {
    let sse = req
        .header("accept")
        .is_some_and(|a| a.contains("text/event-stream"));
    let after_q = req.query_param("after").and_then(|a| a.parse::<u64>().ok());
    if !sse {
        let id2 = id.clone();
        return match blocking(ctx, move |d| d.view_events(&id2, after_q))
            .await
            .flatten()
        {
            Some((lines, _)) => {
                let body: String = lines.iter().map(|(_, l)| format!("{l}\n")).collect();
                send(
                    s,
                    200,
                    "application/x-ndjson; charset=utf-8",
                    body.as_bytes(),
                    false,
                    &[],
                )
                .await;
            }
            None => send_error(s, 404, "no such job").await,
        };
    }
    let mut after = req
        .header("last-event-id")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or(after_q);
    let first = {
        let id2 = id.clone();
        blocking(ctx, move |d| d.view_events(&id2, after))
            .await
            .flatten()
    };
    let Some((backlog, ended)) = first else {
        return send_error(s, 404, "no such job").await;
    };
    if ended && backlog.is_empty() {
        // 204 tells EventSource to stop reconnecting: the job has nothing more to say
        return send(s, 204, "", b"", false, &[]).await;
    }
    let h = head(200, "text/event-stream; charset=utf-8", None, false, &[]);
    if s.write_all(h.as_bytes()).await.is_err() || s.write_all(b"retry: 2000\n\n").await.is_err() {
        return;
    }
    let mut pending = Some((backlog, ended));
    let mut quiet_ms = 0u64;
    loop {
        let (lines, ended) = match pending.take() {
            Some(p) => p,
            None => {
                let id2 = id.clone();
                match blocking(ctx, move |d| d.view_events(&id2, after))
                    .await
                    .flatten()
                {
                    Some(p) => p,
                    None => return,
                }
            }
        };
        let mut out = String::new();
        for (seq, line) in &lines {
            out.push_str(&format!("id: {seq}\ndata: {line}\n\n"));
            after = Some(*seq);
        }
        if !lines.is_empty() {
            let id2 = id.clone();
            if let Some(Some(job)) = blocking(ctx, move |d| d.view_job(&id2)).await {
                out.push_str(&format!("event: job\ndata: {job}\n\n"));
            }
            quiet_ms = 0;
        } else if quiet_ms >= SSE_PING_MS {
            out.push_str(": ping\n\n");
            quiet_ms = 0;
        }
        if !out.is_empty()
            && (s.write_all(out.as_bytes()).await.is_err() || s.flush().await.is_err())
        {
            return;
        }
        if ended {
            let _ = s.shutdown().await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(SSE_POLL_MS)).await;
        quiet_ms += SSE_POLL_MS;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn the_view_binds_loopback_and_in_hub_mode_the_tailnet_only() {
        assert!(check_view_addr(ip("127.0.0.1"), false).is_ok());
        assert!(check_view_addr(ip("::1"), false).is_ok());
        for bad in [
            "0.0.0.0",
            "::",
            "192.168.1.20",
            "10.0.0.5",
            "172.16.3.4",
            "8.8.8.8",
            "fe80::1",
        ] {
            assert!(check_view_addr(ip(bad), false).is_err(), "{bad} refused");
            assert!(
                check_view_addr(ip(bad), true).is_err(),
                "{bad} refused in hub mode too"
            );
        }
        assert!(
            check_view_addr(ip("100.101.102.103"), false).is_err(),
            "tailnet only in hub mode"
        );
        assert!(check_view_addr(ip("100.101.102.103"), true).is_ok());
        assert!(check_view_addr(ip("fd7a:115c:a1e0::1"), true).is_ok());
        assert!(
            check_view_addr(ip("100.128.0.1"), true).is_err(),
            "outside 100.64.0.0/10"
        );
        assert!(
            peer_allowed(ip("127.0.0.1"), false)
                && peer_allowed(IpAddr::V6(Ipv6Addr::LOCALHOST), false)
        );
        assert!(
            peer_allowed(ip("::ffff:127.0.0.1"), false),
            "v4-mapped loopback"
        );
        assert!(
            !peer_allowed(ip("192.168.1.5"), true)
                && !peer_allowed(ip("100.64.0.9"), false)
                && peer_allowed(ip("100.64.0.9"), true)
        );
    }

    #[test]
    fn dates_paths_tokens_and_hosts() {
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(http_date(1_792_497_600), "Tue, 20 Oct 2026 12:00:00 GMT");
        assert_eq!(http_date(951_782_400), "Tue, 29 Feb 2000 00:00:00 GMT");
        let d = Path::new("dist");
        assert_eq!(static_path(d, "/"), Some(d.join("index.html")));
        assert_eq!(
            static_path(d, "/assets/index-abc.js"),
            Some(d.join("assets").join("index-abc.js"))
        );
        for bad in [
            "/../x",
            "/assets/../../x",
            "/a%2e%2e/x",
            "/a\\b",
            "/c:/x",
            "//x",
            "/a//b",
        ] {
            assert_eq!(static_path(d, bad), None, "{bad}");
        }
        let html = inject_token(
            "<!doctype html><html><head lang=\"en\"><title>x</title></head></html>",
            "tok",
        )
        .unwrap();
        assert!(html.contains(
            "<head lang=\"en\">\n<meta name=\"agentroid-token\" content=\"tok\"><title>"
        ));
        assert!(ct_eq(b"abc", b"abc") && !ct_eq(b"abc", b"abd") && !ct_eq(b"abc", b"ab"));
        assert_eq!(host_name("[::1]:80"), "::1");
        assert_eq!(host_name("127.0.0.1:8080"), "127.0.0.1");
        assert_eq!(host_name("localhost"), "localhost");
    }
}
