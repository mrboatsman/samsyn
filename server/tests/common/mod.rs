//! Helpers for the sharing scenarios: spawn `samsyn serve` and `samsyn sync`
//! processes from the test binary's own build, talk to the HTTP API, and poll the filesystem.
//! Everything lives in a temp dir and is killed on drop, so a failing test leaves nothing behind.
#![allow(dead_code)]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_samsyn");

pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

pub struct Proc {
    child: Child,
    pub log: PathBuf,
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Proc {
    pub fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
    pub fn log_contains(&self, s: &str) -> bool {
        self.log_text().contains(s)
    }
    pub fn log_count(&self, s: &str) -> usize {
        self.log_text().matches(s).count()
    }
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
    pub fn has_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

fn spawn(args: &[&str], log: &Path) -> Proc {
    let out = std::fs::File::create(log).unwrap();
    let err = out.try_clone().unwrap();
    let child = Command::new(BIN)
        .args(args)
        .env("RUST_LOG", "info")
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .expect("spawn samsyn");
    Proc { child, log: log.to_path_buf() }
}

pub struct Server {
    pub proc: Proc,
    pub port: u16,
    pub data: PathBuf,
    pub owner_token: String,
}

impl Server {
    /// Start a server (no p2p unless `extra` says otherwise) and wait for its owner token.
    pub fn start(dir: &Path, name: &str, extra: &[&str]) -> Server {
        let port = free_port();
        let data = dir.join(format!("{name}-data"));
        let log = dir.join(format!("{name}-server.log"));
        Self::start_at(data, log, port, extra)
    }

    pub fn start_at(data: PathBuf, log: PathBuf, port: u16, extra: &[&str]) -> Server {
        let bind = format!("127.0.0.1:{port}");
        let mut args = vec!["--data-dir", data.to_str().unwrap(), "serve", "--bind", &bind];
        if !extra.iter().any(|a| a.starts_with("--p2p-port")) && !extra.contains(&"--no-p2p") {
            args.push("--no-p2p");
        }
        args.extend_from_slice(extra);
        let proc = spawn(&args, &log);
        let t0 = Instant::now();
        loop {
            if ureq_ok(&format!("http://127.0.0.1:{port}/health")) {
                break;
            }
            assert!(t0.elapsed() < Duration::from_secs(20), "server did not come up:\n{}", proc.log_text());
            std::thread::sleep(Duration::from_millis(100));
        }
        let text = proc.log_text();
        let owner_token = text
            .split("token: ")
            .nth(1)
            .map(|s| s.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect::<String>())
            .filter(|t| t.starts_with("rp_"))
            .unwrap_or_else(|| {
                // Restarted on an existing data dir: no token printed; caller must keep it.
                String::new()
            });
        Server { proc, port, data, owner_token }
    }

    /// Stop and start again on the same data dir and port.
    pub fn restart(&mut self, extra: &[&str]) {
        self.proc.kill();
        let log = self.proc.log.clone();
        let token = self.owner_token.clone();
        let fresh = Server::start_at(self.data.clone(), log, self.port, extra);
        self.proc = fresh.proc;
        self.owner_token = token;
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn add_user(&self, name: &str) -> String {
        let out = Command::new(BIN)
            .args(["--data-dir", self.data.to_str().unwrap(), "user", "add", name])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        text.split("rp_")
            .nth(1)
            .map(|s| format!("rp_{}", s.chars().take_while(|c| c.is_ascii_hexdigit()).collect::<String>()))
            .expect("token in user add output")
    }

    pub fn node_id(&self) -> String {
        let v: serde_json::Value = api_json(self, "", "GET", "/health", None);
        v["node_id"].as_str().unwrap().to_string()
    }

    pub fn errors(&self) -> Vec<String> {
        self.proc
            .log_text()
            .lines()
            .filter(|l| l.contains(" ERROR") || l.contains("panicked"))
            .map(str::to_string)
            .collect()
    }
}

fn ureq_ok(url: &str) -> bool {
    std::net::TcpStream::connect_timeout(
        &url.trim_start_matches("http://").split('/').next().unwrap().parse().unwrap(),
        Duration::from_millis(300),
    )
    .is_ok()
}

/// Minimal HTTP client on top of reqwest's blocking API (dev-dependency free: reqwest is
/// already a dependency of the binary).
pub fn api_json(server: &Server, token: &str, method: &str, path: &str, body: Option<serde_json::Value>) -> serde_json::Value {
    let (status, text) = api_raw(server, token, method, path, body);
    serde_json::from_str(&text).unwrap_or_else(|_| serde_json::json!({ "status": status, "raw": text }))
}

pub fn api_raw(server: &Server, token: &str, method: &str, path: &str, body: Option<serde_json::Value>) -> (u16, String) {
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(40)).build().unwrap();
    let url = format!("{}{}", server.url(), path);
    let mut req = match method {
        "GET" => client.get(&url),
        "POST" => client.post(&url),
        "DELETE" => client.delete(&url),
        "PUT" => client.put(&url),
        "PATCH" => client.patch(&url),
        m => panic!("method {m}"),
    };
    if !token.is_empty() {
        req = req.bearer_auth(token);
    }
    if let Some(b) = body {
        req = req.json(&b);
    }
    let res = req.send().expect("http request");
    let status = res.status().as_u16();
    (status, res.text().unwrap_or_default())
}

/// A headless client syncing `dir` with a vault (or, with `ticket`, a share as guest).
pub struct Client {
    pub proc: Proc,
    pub dir: PathBuf,
}

impl Client {
    pub fn start(server: &Server, token: &str, vault: &str, dir: &Path, device: &str) -> Client {
        std::fs::create_dir_all(dir).unwrap();
        let log = dir.parent().unwrap().join(format!("client-{device}.log"));
        let url = server.url();
        let proc = spawn(
            &["sync", "--server", &url, "--token", token, "--vault", vault, "--dir", dir.to_str().unwrap(), "--device", device],
            &log,
        );
        Client { proc, dir: dir.to_path_buf() }
    }

    pub fn start_guest(ticket: &str, dir: &Path, device: &str) -> Client {
        std::fs::create_dir_all(dir).unwrap();
        let log = dir.parent().unwrap().join(format!("client-{device}.log"));
        let proc = spawn(&["sync", "--ticket", ticket, "--dir", dir.to_str().unwrap(), "--device", device], &log);
        Client { proc, dir: dir.to_path_buf() }
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }
    pub fn write(&self, rel: &str, content: &str) {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    pub fn write_bytes(&self, rel: &str, content: &[u8]) {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    pub fn append(&self, rel: &str, line: &str) {
        use std::io::Write;
        let p = self.path(rel);
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, "{line}").unwrap();
    }
    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path(rel)).unwrap_or_default()
    }
    pub fn exists(&self, rel: &str) -> bool {
        self.path(rel).exists()
    }
    pub fn remove(&self, rel: &str) {
        let _ = std::fs::remove_file(self.path(rel));
    }
    pub fn contains(&self, rel: &str, needle: &str) -> bool {
        self.read(rel).contains(needle)
    }
    pub fn same_bytes(&self, rel: &str, other: &Path) -> bool {
        match (std::fs::read(self.path(rel)), std::fs::read(other)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
    }
}

/// Poll `cond` until true or `secs` elapsed. Returns whether it became true.
pub fn wait_for(secs: u64, mut cond: impl FnMut() -> bool) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    cond()
}

/// `assert!(wait_for(..))` with a message that shows the relevant logs on failure.
#[macro_export]
macro_rules! eventually {
    ($secs:expr, $cond:expr, $what:expr $(, $log:expr)*) => {{
        if !$crate::common::wait_for($secs, || $cond) {
            let mut msg = format!("timed out waiting for: {}", $what);
            $( msg.push_str(&format!("\n--- {} ---\n{}", stringify!($log), $log.log_text().lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"))); )*
            panic!("{msg}");
        }
    }};
}

pub fn sleep(secs: f64) {
    std::thread::sleep(Duration::from_secs_f64(secs));
}

pub fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("samsyn-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Strip the read-only bit a client put on a file inside a read-only share, so a test can try
/// to edit it anyway. Test fixture only: clippy's concern (world-writable on unix) is moot in a
/// throwaway directory.
#[allow(clippy::permissions_set_readonly_false)]
pub fn make_writable(p: &Path) {
    let mut perms = std::fs::metadata(p).unwrap().permissions();
    perms.set_readonly(false);
    std::fs::set_permissions(p, perms).unwrap();
}
