//! An optional MCP server (streamable HTTP) inside the TUI. Each tool call is
//! the same request the Unix socket takes, answered by the TUI's own scan, so
//! suggestions land in the same list and are saved the same way.

use crate::agent::{self, Json, Obj, Request};
use axum::extract::Request as HttpRequest;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use std::io::Read;
use std::net::{Ipv4Addr, SocketAddr};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::sync::mpsc::Sender;

pub const DEFAULT_PORT: u16 = 7377;

#[derive(Clone)]
struct Apdu {
    tx: Sender<Request>,
    root: PathBuf,
}

#[derive(Deserialize, JsonSchema)]
struct SizesArgs {
    /// Absolute path, or relative to the folder apdu is browsing. Defaults to that folder.
    path: Option<String>,
    /// How many children to list (default 20).
    children: Option<u32>,
    /// "size" (biggest first, the default), "modified" or "created" (oldest first).
    sort: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct FindArgs {
    /// The name to match, e.g. "node_modules" or "*.log": case is ignored and `*` matches anything.
    name: String,
    /// Where to search: absolute, or relative to the folder apdu is browsing. Defaults to that folder.
    path: Option<String>,
    /// How many matches to list, biggest first (default 100). `total` and `private_sum` count them all.
    limit: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
struct SuggestArgs {
    /// The file or folder to suggest deleting: absolute, or relative to the folder apdu is browsing.
    path: String,
    /// Why it is safe or worth deleting, shown to the user next to it.
    reason: String,
}

#[derive(Deserialize, JsonSchema)]
struct PathArgs {
    /// Absolute, or relative to the folder apdu is browsing.
    path: String,
}

impl Apdu {
    fn abs(&self, p: &str) -> String {
        let p = if p.is_empty() { self.root.clone() } else { self.root.join(p) };
        p.to_string_lossy().into_owned()
    }

    async fn ask(&self, cmd: &str, fields: Vec<(&str, Json)>) -> Result<String, String> {
        let mut body = Obj::new();
        body.insert("cmd".into(), cmd.into());
        body.extend(fields.into_iter().map(|(k, v)| (k.to_string(), v)));
        let tx = self.tx.clone();
        let answer = tokio::task::spawn_blocking(move || agent::ask(&tx, body)).await.map_err(|e| e.to_string())?;
        match &answer {
            Json::Obj(o) if o.get("ok") == Some(&Json::Bool(true)) => Ok(answer.to_string()),
            Json::Obj(o) => Err(o.get("error").and_then(Json::str).unwrap_or("request failed").to_string()),
            _ => Err("unexpected answer from the TUI".into()),
        }
    }
}

#[tool_router]
impl Apdu {
    #[tool(
        description = "Sizes of a file or folder and its biggest (or oldest) children, from apdu's scan. `private` is the bytes deleting it would free (APFS clones counted once); `allocated` is what du reports. Times are Unix seconds plus ISO strings; a folder's `modified` is the newest change anywhere inside it."
    )]
    async fn sizes(&self, Parameters(a): Parameters<SizesArgs>) -> Result<String, String> {
        let mut f = vec![("path", self.abs(a.path.as_deref().unwrap_or("")).into())];
        if let Some(n) = a.children {
            f.push(("children", (n as u64).into()));
        }
        if let Some(s) = a.sort {
            f.push(("sort", s.into()));
        }
        self.ask("sizes", f).await
    }

    #[tool(
        description = "Find every file or folder with a given name anywhere in the scan, in one call, with each match's path and `private` size, biggest first. Use it instead of walking folders with `sizes` to find things like every `node_modules`, `target` or `*.log`. It does not look inside a match, so nothing is counted twice, and it can miss what is inside `.git` and `node_modules` (the saved scan keeps only their totals)."
    )]
    async fn find(&self, Parameters(a): Parameters<FindArgs>) -> Result<String, String> {
        let mut f = vec![("name", a.name.into()), ("path", self.abs(a.path.as_deref().unwrap_or("")).into())];
        if let Some(n) = a.limit {
            f.push(("limit", (n as u64).into()));
        }
        self.ask("find", f).await
    }

    #[tool(
        description = "Suggest that the user delete a file or folder, with a reason. It shows up in the user's apdu TUI (and is kept between runs); only the user can mark or delete it. Suggesting the same path again replaces the reason."
    )]
    async fn suggest(&self, Parameters(a): Parameters<SuggestArgs>) -> Result<String, String> {
        self.ask("suggest", vec![("path", self.abs(&a.path).into()), ("reason", a.reason.into())]).await
    }

    #[tool(description = "List the current deletion suggestions, with their reasons and whether the user has marked each for deletion. `deleted` lists suggestions the user has since deleted in apdu, with when: they are gone, so don't suggest them again or treat them as still on disk.")]
    async fn suggestions(&self) -> Result<String, String> {
        self.ask("suggestions", vec![]).await
    }

    #[tool(description = "Withdraw the suggestion on one path.")]
    async fn clear_suggestion(&self, Parameters(a): Parameters<PathArgs>) -> Result<String, String> {
        self.ask("clear", vec![("path", self.abs(&a.path).into())]).await
    }

    #[tool(description = "Show the user a file or folder: the TUI opens the folder holding it and selects it.")]
    async fn goto(&self, Parameters(a): Parameters<PathArgs>) -> Result<String, String> {
        self.ask("goto", vec![("path", self.abs(&a.path).into())]).await
    }
}

#[tool_handler(
    name = "apdu",
    instructions = "apdu is a disk usage browser for macOS that understands APFS copy-on-write clones. These tools read the scan of the folder the user has open in apdu and let you suggest things to delete. You cannot delete or mark anything: the user reviews your suggestions in apdu and decides. Use `private` sizes when judging what deleting would free."
)]
impl ServerHandler for Apdu {}

pub struct Running {
    pub lan: bool,
    pub host: String,
    pub ip: Option<String>,
    pub port: u16,
    pairing: Arc<Mutex<Option<Pairing>>>,
}

/// A one-time code that lets another computer fetch the setup command
/// (token included) without typing the token.
struct Pairing {
    code: String,
    until: Instant,
    misses: u32,
    paired_by: Option<String>,
}

pub enum PairState {
    Open(Duration),
    Paired(String),
    Closed,
}

const PAIRING_LIFE: Duration = Duration::from_secs(180);
const PAIRING_MISSES: u32 = 5;

impl Running {
    pub fn url(&self) -> String {
        format!("http://{}:{}/mcp", self.host, self.port)
    }

    /// Makes a fresh pairing code, replacing any earlier one. It works once,
    /// for three minutes, and is dropped after a few wrong guesses.
    pub fn new_pairing(&self) -> String {
        let mut b = [0u8; 4];
        let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
        let code = format!("{:06}", u32::from_le_bytes(b) % 1_000_000);
        *self.pairing.lock().unwrap() = Some(Pairing { code: code.clone(), until: Instant::now() + PAIRING_LIFE, misses: 0, paired_by: None });
        code
    }

    pub fn pair_state(&self) -> PairState {
        match self.pairing.lock().unwrap().as_ref() {
            Some(Pairing { paired_by: Some(from), .. }) => PairState::Paired(from.clone()),
            Some(p) => p.until.checked_duration_since(Instant::now()).map_or(PairState::Closed, PairState::Open),
            None => PairState::Closed,
        }
    }

    pub fn cancel_pairing(&self) {
        *self.pairing.lock().unwrap() = None;
    }

    pub fn pair_command(&self, host: &str, code: &str) -> String {
        format!("curl -fsS http://{host}:{}/pair/{code} | sh", self.port)
    }
}

/// Uses up the pairing code if `given` matches it; any wrong guess counts
/// toward dropping it.
fn redeem(pairing: &Mutex<Option<Pairing>>, given: &str, from: &str) -> bool {
    let mut p = pairing.lock().unwrap();
    let Some(cur) = p.as_mut().filter(|c| c.paired_by.is_none()) else { return false };
    if Instant::now() > cur.until {
        *p = None;
        return false;
    }
    if cur.code == given {
        cur.paired_by = Some(from.to_string());
        return true;
    }
    cur.misses += 1;
    if cur.misses >= PAIRING_MISSES {
        *p = None;
    }
    false
}

/// The script `/pair/<code>` serves: it adds this server to Claude Code at
/// `authority` (the host:port the other computer reached us by), or prints
/// the URL and token for another client.
fn setup_script(authority: &str, token: &str) -> String {
    let url = format!("http://{authority}/mcp");
    let add = format!("claude mcp add --scope user --transport http apdu {url} --header \"Authorization: Bearer {token}\"");
    format!(
        "#!/bin/sh\n\
         if ! command -v claude >/dev/null 2>&1; then\n  \
           echo 'Claude Code is not installed here. For another MCP client use:'\n  \
           echo '  URL:    {url}'\n  \
           echo '  header: Authorization: Bearer {token}'\n  \
           exit 1\n\
         fi\n\
         claude mcp remove --scope user apdu >/dev/null 2>&1\n\
         {add} && echo 'apdu MCP added to Claude Code ({url})'\n"
    )
}

/// Starts serving MCP at `/mcp` on localhost, or on every interface with
/// `lan`. Every request needs `Authorization: Bearer <token>`.
pub fn start(lan: bool, port: u16, root: PathBuf, tx: Sender<Request>) -> std::io::Result<Running> {
    let token_file = token_file().ok_or_else(|| std::io::Error::other("no $HOME to keep the MCP token in"))?;
    let token = token(&token_file)?;
    let ip = if lan { Ipv4Addr::UNSPECIFIED } else { Ipv4Addr::LOCALHOST };
    let listener = std::net::TcpListener::bind(SocketAddr::from((ip, port)))
        .map_err(|e| std::io::Error::new(e.kind(), format!("could not listen on port {port}: {e}")))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let host = if lan { hostname() } else { "127.0.0.1".into() };
    let running = Running {
        lan,
        ip: if lan { lan_ip() } else { None },
        host: host.clone(),
        port,
        pairing: Arc::new(Mutex::new(None)),
    };
    let pairing = running.pairing.clone();
    let (fallback, script_token) = (format!("{host}:{port}"), token.clone());
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
        rt.block_on(async move {
            let Ok(listener) = tokio::net::TcpListener::from_std(listener) else { return };
            let mut config = StreamableHttpServerConfig::default();
            if lan {
                // Other machines reach us by name or LAN address; the token keeps out
                // everyone else, DNS rebinding included.
                config = config.disable_allowed_hosts();
            }
            let apdu = Apdu { tx, root };
            let service = StreamableHttpService::new(move || Ok(apdu.clone()), Arc::new(LocalSessionManager::default()), config);
            let token: Arc<str> = token.into();
            let pair = move |axum::extract::Path(code): axum::extract::Path<String>, axum::extract::ConnectInfo(from): axum::extract::ConnectInfo<SocketAddr>, headers: axum::http::HeaderMap| async move {
                let host = headers.get("host").and_then(|h| h.to_str().ok()).filter(|h| plain_authority(h)).unwrap_or(&fallback);
                if redeem(&pairing, &code, &from.ip().to_string()) { Ok(setup_script(host, &script_token)) } else { Err(StatusCode::NOT_FOUND) }
            };
            let app = axum::Router::new()
                .nest_service("/mcp", service)
                .layer(axum::middleware::from_fn(move |req, next| {
                    let token = token.clone();
                    async move { authorize(&token, req, next).await }
                }))
                .route("/pair/{code}", axum::routing::get(pair));
            let _ = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await;
        });
    });
    Ok(running)
}

async fn authorize(token: &str, req: HttpRequest, next: Next) -> Result<Response, StatusCode> {
    let given = req.headers().get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    match given {
        Some(g) if same(g.as_bytes(), token.as_bytes()) => Ok(next.run(req).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

/// A Host header safe to put in a shell script: letters, digits, `.`, `-`, `:`.
fn plain_authority(h: &str) -> bool {
    !h.is_empty() && h.len() < 256 && h.bytes().all(|b| b.is_ascii_alphanumeric() || b".-:".contains(&b))
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn token_file() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/apdu/mcp-token"))
}

/// The saved token, or a new random one saved (owner-only) for next time, so
/// agents set up once keep working.
fn token(file: &Path) -> std::io::Result<String> {
    if let Ok(t) = std::fs::read_to_string(file)
        && !t.trim().is_empty()
    {
        return Ok(t.trim().to_string());
    }
    let mut bytes = [0u8; 24];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let t: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let dir = file.parent().unwrap();
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(file)?;
    std::io::Write::write_all(&mut f, t.as_bytes())?;
    Ok(t)
}

/// The Bonjour name other Macs on the network resolve, like `my-mac.local`.
fn hostname() -> String {
    let out = std::process::Command::new("scutil").args(["--get", "LocalHostName"]).output();
    match out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()) {
        Ok(name) if !name.is_empty() => format!("{name}.local"),
        _ => lan_ip().unwrap_or_else(|| "<this-mac>".into()),
    }
}

/// This Mac's first private IPv4 address, for when `.local` names don't resolve.
fn lan_ip() -> Option<String> {
    let mut addrs: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut addrs) } != 0 {
        return None;
    }
    let mut found = None;
    let mut cur = addrs;
    while !cur.is_null() && found.is_none() {
        let a = unsafe { &*cur };
        if !a.ifa_addr.is_null() && i32::from(unsafe { (*a.ifa_addr).sa_family }) == libc::AF_INET {
            let sin = unsafe { &*(a.ifa_addr as *const libc::sockaddr_in) };
            let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            if ip.is_private() {
                found = Some(ip.to_string());
            }
        }
        cur = a.ifa_next;
    }
    unsafe { libc::freeifaddrs(addrs) };
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_made_once_and_owner_only() {
        let d = std::env::temp_dir().join(format!("apdu-token-{}", std::process::id()));
        let f = d.join("t/mcp-token");
        let first = token(&f).unwrap();
        assert_eq!(first.len(), 48);
        assert_eq!(token(&f).unwrap(), first);
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn pairing_works_once_and_not_after_wrong_guesses() {
        let p = Mutex::new(None);
        assert!(!redeem(&p, "123456", "a"), "closed until opened");
        let open = |code: &str| Some(Pairing { code: code.into(), until: Instant::now() + PAIRING_LIFE, misses: 0, paired_by: None });
        *p.lock().unwrap() = open("123456");
        assert!(redeem(&p, "123456", "10.0.0.2"));
        assert!(!redeem(&p, "123456", "10.0.0.3"), "one use");
        assert_eq!(p.lock().unwrap().as_ref().unwrap().paired_by.as_deref(), Some("10.0.0.2"));

        *p.lock().unwrap() = open("123456");
        for _ in 0..PAIRING_MISSES {
            assert!(!redeem(&p, "000000", "x"));
        }
        assert!(!redeem(&p, "123456", "x"), "too many wrong guesses close it");

        *p.lock().unwrap() = Some(Pairing { until: Instant::now(), ..open("123456").unwrap() });
        std::thread::sleep(Duration::from_millis(5));
        assert!(!redeem(&p, "123456", "x"), "expired");
    }

    #[test]
    fn only_plain_hosts_go_into_the_script() {
        assert!(plain_authority("my-mac.local:7377"));
        assert!(plain_authority("192.168.1.5:7377"));
        assert!(!plain_authority("x;rm -rf ~"));
        assert!(!plain_authority("a$(id)"));
        assert!(!plain_authority(""));
        assert!(setup_script("h.local:1", "t").contains("apdu http://h.local:1/mcp --header \"Authorization: Bearer t\""));
    }

    #[test]
    fn compares_tokens_whole() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"ab", b"abc"));
    }
}
