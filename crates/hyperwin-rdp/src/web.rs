//! The keelOS-host face of hyperWIN, as hyperMAC has one (its src/web.rs): just enough of
//! keel's API that kRDPclient adds this machine like a keelOS host (address + token) and shows
//! its VMs as guests. Same TLS certificate as the RDP side, so the fingerprint kRDPclient pins
//! from here matches the RDP connection.
//!
//! Served (HTTP/1.1 over TLS, one request per connection):
//!   GET /v1/version                 no token
//!   GET /v1/vms                     Bearer token -> the VMs
//!   GET /v1/vms/{uuid}              one of them
//!   GET /v1/vms/{uuid}/consoles     {"running": ..., "vnc": false}
//!   POST /v1/vms/{uuid}/{action}    start, stop (the guest shuts down), kill (power off),
//!                                   reset (power off and start). suspend and resume: 409.
//!   GET /v1/vms/{uuid}/clipboard    the VM's clipboard, for tests: how many ends it has (the
//!                                   guest's helper, each RDP client), what is offered on it,
//!                                   and the text if text is offered
//!   POST /v1/vms/{uuid}/clipboard   the body (UTF-8 text) "copied": the guest and the clients
//!                                   can paste it
//!   GET /session                    what the token may do (kRDPclient asks before it offers
//!                                   the power buttons)
//! Everything else is 404 (kRDPclient treats /v1/host as optional).
//!
//!   POST /login                     {"password", "user"?} = hyperWIN's account (account.rs)
//!                                   -> a short web session (cookie + csrf)
//!   POST /v1/tokens                 with that session -> {"secret": a new token}
//!   POST /logout                    ends the session
//!
//! The three POSTs are keel's own login flow, as far as kRDPclient's join uses it (a password
//! typed where the token goes in Add keelOS Host): the password is traded for a token, the
//! password itself is not kept by the client. keel's login has no user name and kRDPclient
//! sends none; hyperWIN has one account, so the password alone names it. (hyperMAC's web.rs
//! does the same.)

use anyhow::Context as _;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, info, warn};

pub struct State {
    /// hyperwin's state folder: the VMs are read from it at every request
    pub state: PathBuf,
    pub rdp_port: u16,
    pub tokens: PathBuf,
    /// web sessions made by POST /login: access cookie -> (csrf, made at)
    pub sessions: Mutex<std::collections::HashMap<String, (String, Instant)>>,
}

/// A web session lives this long; kRDPclient's join ends it itself within a second.
const SESSION_TTL: Duration = Duration::from_secs(600);
/// keel's names (keel/internal/web/server.go), which kRDPclient's join uses.
const ACCESS_COOKIE: &str = "keel_access";
const CSRF_HEADER: &str = "x-keel-csrf";

/// One request, as far as the answers need it.
#[derive(Default)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub bearer: String,
    /// the keel_access cookie's value and the X-Keel-CSRF header
    pub access: String,
    pub csrf: String,
    pub body: Vec<u8>,
}

/// status, JSON body, a Set-Cookie value
type Answer = (u16, serde_json::Value, Option<String>);

fn plain(code: u16, body: serde_json::Value) -> Answer {
    (code, body, None)
}

/// POST /login: the account's password (and, when sent, its name) for a web session. A wrong
/// name and a wrong password are refused alike; too many refusals lock the door for a while
/// (account.rs).
fn login(st: &State, body: &[u8]) -> Answer {
    if crate::account::locked() {
        return plain(429, serde_json::json!({"code": "too_many_logins", "error": "too many failed logins: wait a few minutes"}));
    }
    let sent = serde_json::from_slice::<serde_json::Value>(body).unwrap_or_default();
    let field = |k: &str| sent.get(k).and_then(|x| x.as_str()).filter(|x| !x.is_empty());
    if !crate::account::check(&st.state, field("user"), field("password").unwrap_or_default()) {
        warn!("web login refused");
        return plain(401, serde_json::json!({"code": "bad_password", "error": "wrong password"}));
    }
    let (mut access, mut csrf) = ([0u8; 32], [0u8; 16]);
    if getrandom::fill(&mut access).is_err() || getrandom::fill(&mut csrf).is_err() {
        return plain(500, serde_json::json!({"error": "no random numbers"}));
    }
    let (access, csrf) = (hex(&access), hex(&csrf));
    let mut sessions = st.sessions.lock().unwrap_or_else(|e| e.into_inner());
    sessions.retain(|_, (_, made)| made.elapsed() < SESSION_TTL);
    sessions.insert(access.clone(), (csrf.clone(), Instant::now()));
    info!("web login");
    (200, serde_json::json!({"csrf": csrf}), Some(format!("{ACCESS_COOKIE}={access}; Path=/; Secure; HttpOnly; SameSite=Strict")))
}

/// Does this request carry a live web session (the cookie and its csrf)?
fn session(st: &State, request: &Request) -> bool {
    let sessions = st.sessions.lock().unwrap_or_else(|e| e.into_inner());
    sessions.get(&request.access).is_some_and(|(want, made)| !request.csrf.is_empty() && *want == request.csrf && made.elapsed() < SESSION_TTL)
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
pub struct Tokens {
    pub sha256: Vec<String>,
}

pub fn read_tokens(path: &Path) -> Tokens {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Is this token one of ours? (tokens.json holds only the SHA-256 of each.)
pub fn token_ok(tokens: &Path, token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let want = hex(&Sha256::digest(token.as_bytes()));
    read_tokens(tokens).sha256.iter().any(|h| h.eq_ignore_ascii_case(&want))
}

/// A new token, its hash added to <state>/tokens.json. keel's own spelling
/// (`keel_<12 hex>_<43 base64url>`), which is how kRDPclient tells a token from a password.
pub fn new_token(state: &Path) -> anyhow::Result<String> {
    let (mut id, mut secret) = ([0u8; 6], [0u8; 32]);
    getrandom::fill(&mut id).map_err(|e| anyhow::anyhow!("random numbers: {e}"))?;
    getrandom::fill(&mut secret).map_err(|e| anyhow::anyhow!("random numbers: {e}"))?;
    let token = format!("keel_{}_{}", hex(&id), b64url(&secret));
    let path = state.join("tokens.json");
    let mut t = read_tokens(&path);
    t.sha256.push(hex(&Sha256::digest(token.as_bytes())));
    std::fs::create_dir_all(state).with_context(|| format!("create {}", state.display()))?;
    std::fs::write(&path, serde_json::to_vec_pretty(&t)?).with_context(|| format!("write {}", path.display()))?;
    Ok(token)
}

/// base64url without padding (RFC 4648 §5): 32 bytes -> the 43 characters keel's token carries.
fn b64url(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        for i in 0..1 + c.len() {
            out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

fn guest_json(vm: &hyperwin_vm::Vm, rdp_port: u16) -> serde_json::Value {
    let os = vm.config.os.as_str();
    let name = match os {
        "windows" => "Windows",
        "linux" => "Linux",
        "freebsd" => "FreeBSD",
        other => other,
    };
    serde_json::json!({
        "uuid": vm.config.id,
        "alias": vm.name,
        "state": vm.state.name(),
        "os": { "family": os, "name": name, "version": "" },
        "rdp": { "port": rdp_port },
    })
}

/// keel's power actions on a VM. The work is given up to five seconds (what kRDPclient asks
/// for with ?wait=5): done by then is 200 or the refusal; still going is 202, and the list
/// shows the outcome. (A start can take minutes when the host's display driver has to be
/// copied into the guest first.)
fn power(st: &State, vm: &hyperwin_vm::Vm, action: &str) -> (u16, serde_json::Value) {
    match action {
        "start" | "stop" | "kill" | "reset" => {}
        "suspend" | "resume" => return (409, serde_json::json!({"error": "hyperWIN does not save a VM's state yet"})),
        _ => return (404, serde_json::json!({"error": format!("no such action: {action}")})),
    }
    let (state, name, action) = (st.state.clone(), vm.name.clone(), action.to_owned());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = match action.as_str() {
            "start" => hyperwin_vm::start(&state, &name),
            "stop" => hyperwin_vm::shut_down(&state, &name, 60_000),
            "kill" => hyperwin_vm::kill(&state, &name),
            _ => hyperwin_vm::kill(&state, &name).and_then(|()| hyperwin_vm::start(&state, &name)),
        };
        match &result {
            Ok(()) => info!(vm = name, action, "web: power"),
            Err(e) => warn!(vm = name, action, "web: power: {e:#}"),
        }
        let _ = tx.send(result);
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => (200, serde_json::json!({})),
        Ok(Err(e)) => (409, serde_json::json!({"error": format!("{e:#}")})),
        Err(_) => (202, serde_json::json!({})),
    }
}

/// What a request gets. Blocks (HCS calls).
fn answer(st: &State, request: &Request) -> Answer {
    let (method, path, bearer) = (request.method.as_str(), request.path.as_str(), request.bearer.as_str());
    match (method, path) {
        (_, "/v1/version") => return plain(200, serde_json::json!({"version": env!("CARGO_PKG_VERSION"), "product": "hyperWIN", "build": "dev"})),
        ("POST", "/login") => return login(st, &request.body),
        ("POST", "/login/setup") => return plain(409, serde_json::json!({"code": "setup_done", "error": "hyperWIN's account is set on the machine itself: hyperwin passwd USER"})),
        ("POST", "/v1/tokens") => {
            if !session(st, request) {
                return plain(401, serde_json::json!({"error": "log in first (POST /login), then send the cookie and X-Keel-CSRF"}));
            }
            return match new_token(&st.state) {
                Ok(token) => {
                    let name = serde_json::from_slice::<serde_json::Value>(&request.body).ok().and_then(|v| v.get("name")?.as_str().map(str::to_owned)).unwrap_or_default();
                    info!(name, "web: token made");
                    plain(200, serde_json::json!({"secret": token, "name": name}))
                }
                Err(e) => plain(500, serde_json::json!({"error": format!("{e:#}")})),
            };
        }
        ("POST", "/logout") => {
            st.sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(&request.access);
            return plain(200, serde_json::json!({}));
        }
        _ => {}
    }
    if !token_ok(&st.tokens, bearer) {
        return plain(401, serde_json::json!({"error": "a token made by `hyperwin token` goes in Authorization: Bearer"}));
    }
    let (code, body) = answer_with_token(st, method, path, &request.body);
    plain(code, body)
}

/// The part behind the token.
fn answer_with_token(st: &State, method: &str, path: &str, body: &[u8]) -> (u16, serde_json::Value) {
    if path == "/session" {
        // one kind of token: it may do all there is to do here
        return (200, serde_json::json!({"perms": {"vm:read": true, "vm:power": true, "vm:console": true}}));
    }
    let vms = match hyperwin_vm::list(&st.state) {
        Ok(vms) => vms,
        Err(e) => return (500, serde_json::json!({"error": format!("{e:#}")})),
    };
    if path == "/v1/vms" {
        return (200, serde_json::Value::Array(vms.iter().map(|vm| guest_json(vm, st.rdp_port)).collect()));
    }
    let mut parts = path.strip_prefix("/v1/vms/").unwrap_or("").split('/');
    let (uuid, what, more) = (parts.next().unwrap_or(""), parts.next(), parts.next());
    let Some(vm) = vms.iter().find(|vm| !uuid.is_empty() && vm.config.id.eq_ignore_ascii_case(uuid)) else {
        return (404, serde_json::json!({"error": "hyperWIN serves /v1/version, /v1/vms and /session only"}));
    };
    match (method, what, more) {
        ("GET", None, _) => (200, guest_json(vm, st.rdp_port)),
        ("GET", Some("consoles"), None) => (200, serde_json::json!({"running": vm.state == hyperwin_vm::State::Running, "vnc": false})),
        // the guest's clipboard, as far as it is text (for tests without an RDP client)
        ("GET", Some("clipboard"), None) => (200, crate::clip::peek(&vm.name)),
        ("POST", Some("clipboard"), None) => {
            crate::clip::copy_text(&vm.name, &String::from_utf8_lossy(body));
            (200, serde_json::json!({}))
        }
        ("POST", Some(action), None) => power(st, vm, action),
        _ => (404, serde_json::json!({"error": "no such thing on a VM"})),
    }
}

/// One connection: read one request, answer it, close.
async fn handle<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(mut stream: S, st: Arc<State>) {
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut tmp).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
        if buf.len() > 16 * 1024 {
            return;
        }
    }
    let head_len = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(0) + 4;
    let head = String::from_utf8_lossy(&buf[..head_len]).into_owned();
    let mut lines = head.lines();
    let Some((method, path)) = lines.next().and_then(|l| {
        let mut it = l.split_whitespace();
        Some((it.next()?.to_owned(), it.next()?.to_owned()))
    }) else {
        return;
    };
    let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned())).collect();
    let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    // the body, for the POSTs (a password or a token's name: small)
    let want = header("content-length").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    if want > 16 * 1024 {
        return;
    }
    while buf.len() < head_len + want {
        match stream.read(&mut tmp).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
    let request = Request {
        method,
        path: path.split('?').next().unwrap_or("").to_owned(),
        bearer: header("authorization").and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer "))).unwrap_or("").to_owned(),
        access: header("cookie").and_then(|c| c.split(';').find_map(|c| c.trim().strip_prefix(ACCESS_COOKIE)?.strip_prefix('='))).unwrap_or("").to_owned(),
        csrf: header(CSRF_HEADER).unwrap_or("").to_owned(),
        body: buf[head_len..head_len + want].to_vec(),
    };
    let (method, path) = (request.method.clone(), request.path.clone());
    let (code, body, cookie) = tokio::task::spawn_blocking(move || answer(&st, &request)).await.unwrap_or_else(|e| plain(500, serde_json::json!({"error": e.to_string()})));
    debug!(method, path, code, "web");
    let body = body.to_string();
    let reason = match code {
        200 => "OK",
        202 => "Accepted",
        401 => "Unauthorized",
        409 => "Conflict",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Not Found",
    };
    let cookie = cookie.map(|c| format!("Set-Cookie: {c}\r\n")).unwrap_or_default();
    let _ = stream
        .write_all(format!("HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\n{cookie}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())
        .await;
    let _ = stream.shutdown().await;
}

/// The web face, on the RDP certificate.
pub async fn serve(listen: std::net::SocketAddr, acceptor: tokio_rustls::TlsAcceptor, st: Arc<State>) -> anyhow::Result<()> {
    let l = tokio::net::TcpListener::bind(listen).await.with_context(|| format!("web: listen on {listen}"))?;
    info!("web API on https://{listen} (add this address in kRDPclient)");
    loop {
        let Ok((tcp, peer)) = l.accept().await else { continue };
        let acceptor = acceptor.clone();
        let st = st.clone();
        tokio::spawn(async move {
            match acceptor.accept(tcp).await {
                Ok(tls) => handle(tls, st).await,
                Err(e) => warn!(%peer, "web TLS: {e}"),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_and_answers() {
        let dir = std::env::temp_dir().join(format!("hyperwin-web-test-{}", std::process::id()));
        let token = new_token(&dir).unwrap();
        assert!(token.starts_with("keel_") && token.len() == 5 + 12 + 1 + 43, "{token}");
        // a VM that is only a file: it lists as stopped (HCS knows no such id)
        let id = hyperwin_vm::new_id().unwrap();
        let vm_dir = dir.join("vms").join("w10");
        std::fs::create_dir_all(&vm_dir).unwrap();
        let config = serde_json::json!({"id": id, "endpoint": "e", "cpus": 2, "memory_mb": 2048, "disk": "nowhere.vhdx", "width": 1280, "height": 800});
        std::fs::write(vm_dir.join("vm.json"), config.to_string()).unwrap();
        let st = State { state: dir.clone(), rdp_port: 3390, tokens: dir.join("tokens.json"), sessions: Default::default() };
        let answer = |st: &State, method: &str, path: &str, bearer: &str| {
            let (code, body, _) = super::answer(st, &Request { method: method.into(), path: path.into(), bearer: bearer.into(), ..Default::default() });
            (code, body)
        };
        // the account's password becomes a session, and the session a token
        crate::account::set(&dir, "admin", "long enough").unwrap();
        let post = |path: &str, body: &str, access: &str, csrf: &str| {
            super::answer(&st, &Request { method: "POST".into(), path: path.into(), body: body.as_bytes().to_vec(), access: access.into(), csrf: csrf.into(), ..Default::default() })
        };
        assert_eq!(post("/login", r#"{"password":"nope nope"}"#, "", "").0, 401);
        assert_eq!(post("/login", r#"{"password":"long enough","user":"someone"}"#, "", "").0, 401);
        let (code, body, cookie) = post("/login", r#"{"password":"long enough"}"#, "", "");
        assert_eq!(code, 200);
        let csrf = body["csrf"].as_str().unwrap().to_owned();
        let access = cookie.unwrap().split(';').next().unwrap().strip_prefix("keel_access=").unwrap().to_owned();
        assert_eq!(post("/v1/tokens", r#"{"name":"pc"}"#, &access, "wrong").0, 401);
        let (code, made, _) = post("/v1/tokens", r#"{"name":"pc"}"#, &access, &csrf);
        assert_eq!(code, 200);
        assert!(token_ok(&st.tokens, made["secret"].as_str().unwrap()));
        assert_eq!(post("/logout", "", &access, &csrf).0, 200);
        assert_eq!(post("/v1/tokens", "{}", &access, &csrf).0, 401);
        assert!(token_ok(&st.tokens, &token));
        assert!(!token_ok(&st.tokens, "keel_000000000000_x"));
        assert_eq!(answer(&st, "GET", "/v1/version", "").0, 200);
        assert_eq!(answer(&st, "GET", "/v1/vms", "").0, 401);
        assert_eq!(answer(&st, "GET", "/session", &token).1["perms"]["vm:power"], true);
        if let Err(e) = hyperwin_vm::list(&dir) {
            // a machine without the Virtual Machine Platform feature: HCS answers nothing
            eprintln!("the rest needs HCS: {e:#}");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let (code, vms) = answer(&st, "GET", "/v1/vms", &token);
        assert_eq!(code, 200, "{vms}");
        assert_eq!(vms[0]["alias"], "w10");
        assert_eq!(vms[0]["uuid"], id.as_str());
        assert_eq!(vms[0]["state"], "stopped");
        assert_eq!(vms[0]["rdp"]["port"], 3390);
        assert_eq!(vms[0]["os"]["family"], "windows");
        assert_eq!(answer(&st, "GET", &format!("/v1/vms/{id}"), &token).0, 200);
        assert_eq!(answer(&st, "GET", &format!("/v1/vms/{id}/consoles"), &token).1["running"], false);
        // a stopped VM cannot be shut down; one without a disk cannot start
        assert_eq!(answer(&st, "POST", &format!("/v1/vms/{id}/stop"), &token).0, 409);
        assert_eq!(answer(&st, "POST", &format!("/v1/vms/{id}/start"), &token).0, 409);
        assert_eq!(answer(&st, "POST", &format!("/v1/vms/{id}/suspend"), &token).0, 409);
        assert_eq!(answer(&st, "GET", "/v1/host", &token).0, 404);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
