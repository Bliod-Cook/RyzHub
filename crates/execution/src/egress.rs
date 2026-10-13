//! Sandbox egress through an allowlisting HTTP proxy.
//!
//! A sandbox has no network interface besides loopback. A sandbox whose spec allows egress
//! gets its own proxy socket, `control/egress.sock`, so the host knows which sandbox and
//! organization each connection comes from. The proxy accepts `CONNECT host:port` (HTTPS) and
//! absolute-form `http://` requests, only to allowlisted hostnames on allowed ports that
//! resolve to public addresses. It connects to the address it resolved, so a later DNS answer
//! cannot redirect the connection to an internal one. Every connection, allowed or denied, is
//! logged for the control plane to collect.
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpStream, UnixListener, UnixStream},
    task::AbortHandle,
};
use uuid::Uuid;

/// Destination ports a sandbox may reach.
pub const PORTS: [u16; 2] = [80, 443];
/// Largest request head the proxy reads.
const MAX_HEAD: usize = 16 * 1024;
/// Log records kept; older ones are dropped first. Every control plane that uses the node
/// reads the log with its own cursor, so nothing is dropped on reading.
const LOG_LIMIT: usize = 10_000;
/// Most rules an allowlist has.
pub const MAX_RULES: usize = 256;
/// Where the guest forwards local proxy connections, inside the sandbox.
pub const GUEST_SOCKET: &str = "egress.sock";
/// The proxy port and address sandbox processes use.
pub const LOCAL_PORT: u16 = 8091;
pub const LOCAL_PROXY: &str = "http://127.0.0.1:8091";
/// Set in the guest's environment when the sandbox may reach the network.
pub const PROXY_ENV: &str = "RYZHUB_EGRESS_PROXY";

/// One proxied connection or refusal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub seq: u64,
    pub at_ms: i64,
    pub org_id: Uuid,
    pub sandbox_id: Uuid,
    pub host: String,
    pub port: u16,
    pub allowed: bool,
    /// Why a connection was refused or failed; empty when it completed.
    pub reason: String,
    pub sent: u64,
    pub received: u64,
}

/// Whether `host` (lowercase, without a trailing dot) matches a rule: an exact hostname, or
/// `*.suffix` for any name under `suffix` (not `suffix` itself).
pub fn allowed(rules: &[String], host: &str) -> bool {
    rules.iter().any(|rule| match rule.strip_prefix("*.") {
        Some(suffix) => host
            .strip_suffix(suffix)
            .is_some_and(|rest| rest.len() > 1 && rest.ends_with('.')),
        None => host == rule,
    })
}

/// A rule the allowlist may hold: a hostname, optionally led by `*.`.
pub fn valid_rule(rule: &str) -> bool {
    let name = rule.strip_prefix("*.").unwrap_or(rule);
    (1..=253).contains(&name.len())
        && name.contains('.')
        && name.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// Whether `ip` is a public unicast address. Loopback, private, link-local, shared (CGNAT),
/// documentation, benchmarking, reserved and multicast ranges are not, nor are IPv6 unique
/// local and link-local addresses; an IPv4-mapped address is judged as IPv4.
pub fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => public_v4(v4),
            None => public_v6(v6),
        },
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || a == 0
        || (a == 100 && (64..128).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (18..20).contains(&b))
        || a >= 240)
}

fn public_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (first & 0xfe00) == 0xfc00
        || (first & 0xffc0) == 0xfe80
        || (first == 0x2001 && ip.segments()[1] == 0x0db8)
        || first == 0x0064)
}

struct Log {
    next: u64,
    records: VecDeque<Record>,
}

/// The host's egress proxies, one per sandbox that may reach the network.
pub struct Egress {
    /// Identifies this process's log, whose sequence numbers restart with it.
    boot: Uuid,
    /// Connections each organization may open per minute.
    per_minute: u32,
    log: Mutex<Log>,
    windows: Mutex<HashMap<Uuid, (i64, u32)>>,
    listeners: Mutex<BTreeMap<Uuid, AbortHandle>>,
}

struct Grant {
    org: Uuid,
    sandbox: Uuid,
    rules: Vec<String>,
}

impl Egress {
    pub fn new(per_minute: u32) -> Arc<Self> {
        Arc::new(Self {
            boot: Uuid::now_v7(),
            per_minute,
            log: Mutex::new(Log {
                next: 1,
                records: VecDeque::new(),
            }),
            windows: Mutex::new(HashMap::new()),
            listeners: Mutex::new(BTreeMap::new()),
        })
    }

    /// Serve sandbox `sandbox`'s proxy at `socket`, replacing any earlier one.
    pub fn start(
        self: &Arc<Self>,
        socket: PathBuf,
        org: Uuid,
        sandbox: Uuid,
        rules: Vec<String>,
    ) -> anyhow::Result<()> {
        self.stop(sandbox);
        if socket.exists() {
            std::fs::remove_file(&socket)?;
        }
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        let grant = Arc::new(Grant {
            org,
            sandbox,
            rules,
        });
        let this = self.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (this, grant) = (this.clone(), grant.clone());
                tokio::spawn(async move { this.connection(stream, &grant).await });
            }
        });
        self.listeners
            .lock()
            .unwrap()
            .insert(sandbox, task.abort_handle());
        Ok(())
    }

    /// Stop sandbox `sandbox`'s proxy. Connections already open run until they close.
    pub fn stop(&self, sandbox: Uuid) {
        if let Some(task) = self.listeners.lock().unwrap().remove(&sandbox) {
            task.abort();
        }
    }

    /// Records after `after`, at most `limit`, and this log's boot id. A cursor from an
    /// earlier process (another `boot`) reads from the start.
    pub fn collect(&self, boot: Option<Uuid>, after: u64, limit: usize) -> (Uuid, Vec<Record>) {
        let after = if boot == Some(self.boot) { after } else { 0 };
        let log = self.log.lock().unwrap();
        let records = log
            .records
            .iter()
            .filter(|r| r.seq > after)
            .take(limit)
            .cloned()
            .collect();
        (self.boot, records)
    }

    fn record(&self, mut record: Record) {
        let mut log = self.log.lock().unwrap();
        record.seq = log.next;
        log.next += 1;
        if log.records.len() == LOG_LIMIT {
            log.records.pop_front();
        }
        log.records.push_back(record);
    }

    /// Whether `org` may open another connection this minute.
    fn admit(&self, org: Uuid) -> bool {
        let minute = now_ms() / 60_000;
        let mut windows = self.windows.lock().unwrap();
        windows.retain(|_, (m, _)| *m == minute);
        let (_, count) = windows.entry(org).or_insert((minute, 0));
        *count += 1;
        *count <= self.per_minute
    }

    async fn connection(&self, mut client: UnixStream, grant: &Grant) {
        let mut record = Record {
            seq: 0,
            at_ms: now_ms(),
            org_id: grant.org,
            sandbox_id: grant.sandbox,
            host: String::new(),
            port: 0,
            allowed: false,
            reason: String::new(),
            sent: 0,
            received: 0,
        };
        let outcome = self.proxy(&mut client, grant, &mut record).await;
        if let Err(reason) = outcome {
            if !record.allowed {
                let body = format!("RyzHub egress policy: {reason}\n");
                let refusal = format!(
                    "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = client.write_all(refusal.as_bytes()).await;
            }
            record.reason = reason.into();
        }
        // A malformed request names nothing worth recording.
        if !record.host.is_empty() {
            self.record(record);
        }
    }

    async fn proxy(
        &self,
        client: &mut UnixStream,
        grant: &Grant,
        record: &mut Record,
    ) -> Result<(), &'static str> {
        let (head, rest) = tokio::time::timeout(Duration::from_secs(10), read_head(client))
            .await
            .map_err(|_| "request_timeout")??;
        let request = Request::parse(&head)?;
        record.host = request.host.clone();
        record.port = request.port;
        if request.host.parse::<IpAddr>().is_ok() {
            return Err("ip_literal");
        }
        if !PORTS.contains(&request.port) {
            return Err("port");
        }
        if !allowed(&grant.rules, &request.host) {
            return Err("host_not_allowed");
        }
        if !self.admit(grant.org) {
            return Err("rate_limited");
        }
        let addresses: Vec<SocketAddr> = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::lookup_host((request.host.as_str(), request.port)),
        )
        .await
        .map_err(|_| "dns_timeout")?
        .map_err(|_| "dns_failed")?
        .collect();
        // Any internal answer refuses the name: it may be pointed at internal services.
        if addresses.is_empty() || addresses.iter().any(|a| !public(a.ip())) {
            return Err("private_address");
        }
        let mut upstream = None;
        for address in &addresses {
            if let Ok(Ok(stream)) =
                tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(address)).await
            {
                upstream = Some(stream);
                break;
            }
        }
        let mut upstream = upstream.ok_or("connect_failed")?;
        record.allowed = true;
        if request.connect {
            client
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .map_err(|_| "client_closed")?;
        } else {
            upstream
                .write_all(&request.forwarded_head())
                .await
                .map_err(|_| "upstream_closed")?;
        }
        upstream
            .write_all(&rest)
            .await
            .map_err(|_| "upstream_closed")?;
        let (sent, received) = tokio::io::copy_bidirectional(client, &mut upstream)
            .await
            .unwrap_or((0, 0));
        record.sent = sent + rest.len() as u64;
        record.received = received;
        Ok(())
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The request head up to its blank line, and whatever the client sent after it.
async fn read_head(
    client: &mut (impl AsyncRead + Unpin),
) -> Result<(String, Vec<u8>), &'static str> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buffer.split_off(end + 4);
            let head = String::from_utf8(buffer).map_err(|_| "bad_request")?;
            return Ok((head, rest));
        }
        if buffer.len() > MAX_HEAD {
            return Err("bad_request");
        }
        let read = client.read(&mut chunk).await.map_err(|_| "bad_request")?;
        if read == 0 {
            return Err("bad_request");
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// A proxy request: `CONNECT host:port` or an absolute-form `http://` request.
#[derive(Debug, PartialEq, Eq)]
struct Request {
    connect: bool,
    method: String,
    host: String,
    port: u16,
    /// Origin-form target of a plain HTTP request.
    path: String,
    version: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn parse(head: &str) -> Result<Self, &'static str> {
        let mut lines = head.split("\r\n");
        let mut line = lines.next().ok_or("bad_request")?.split(' ');
        let (method, target, version) = match (line.next(), line.next(), line.next(), line.next()) {
            (Some(m), Some(t), Some(v), None) if v.starts_with("HTTP/1.") => (m, t, v),
            _ => return Err("bad_request"),
        };
        let headers = lines
            .filter(|l| !l.is_empty())
            .map(|l| {
                l.split_once(':')
                    .map(|(n, v)| (n.trim().to_owned(), v.trim().to_owned()))
                    .ok_or("bad_request")
            })
            .collect::<Result<Vec<_>, _>>()?;
        let connect = method.eq_ignore_ascii_case("CONNECT");
        let (authority, path) = if connect {
            (target, String::new())
        } else {
            let rest = target.strip_prefix("http://").ok_or("unsupported_scheme")?;
            match rest.find('/') {
                Some(slash) => (&rest[..slash], rest[slash..].to_owned()),
                None => (rest, "/".to_owned()),
            }
        };
        let (host, port) = split_authority(authority, if connect { 443 } else { 80 })?;
        Ok(Self {
            connect,
            method: method.to_owned(),
            host,
            port,
            path,
            version: version.to_owned(),
            headers,
        })
    }

    /// The head sent upstream for a plain HTTP request: origin-form, without proxy headers,
    /// one request per connection.
    fn forwarded_head(&self) -> Vec<u8> {
        let mut head = format!("{} {} {}\r\n", self.method, self.path, self.version);
        let mut host = false;
        for (name, value) in &self.headers {
            let lower = name.to_ascii_lowercase();
            if lower.starts_with("proxy-") || lower == "connection" || lower == "keep-alive" {
                continue;
            }
            host |= lower == "host";
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        if !host {
            head.push_str(&format!("Host: {}\r\n", self.host));
        }
        head.push_str("Connection: close\r\n\r\n");
        head.into_bytes()
    }
}

/// Lowercase host without a trailing dot, and port, of `host[:port]` or `[v6][:port]`.
fn split_authority(authority: &str, default: u16) -> Result<(String, u16), &'static str> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or("bad_request")?;
        (host, after.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    let port = match port {
        Some(port) => port.parse().map_err(|_| "bad_request")?,
        None => default,
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || host.contains(['/', '@', ' ']) {
        return Err("bad_request");
    }
    Ok((host, port))
}

/// Environment for processes the guest starts: the local proxy when the sandbox may reach the
/// network, nothing otherwise. Local services stay direct.
pub fn child_env() -> Vec<(String, String)> {
    let Ok(proxy) = std::env::var(PROXY_ENV) else {
        return vec![];
    };
    let direct = "127.0.0.1,localhost,::1".to_owned();
    ["http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY"]
        .into_iter()
        .map(|name| (name.to_owned(), proxy.clone()))
        .chain([
            ("no_proxy".to_owned(), direct.clone()),
            ("NO_PROXY".to_owned(), direct),
        ])
        .collect()
}

/// The guest side: pass each connection to the sandbox's local proxy port on to the host's
/// proxy socket. Requests for the sandbox's own loopback services are served here instead:
/// some clients (busybox wget among them) ignore `no_proxy` and send those to the proxy too.
pub async fn forward(listener: tokio::net::TcpListener, socket: PathBuf) {
    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(forward_one(stream, socket.clone()));
    }
}

fn loopback(host: &str) -> bool {
    host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

async fn forward_one(mut client: TcpStream, socket: PathBuf) {
    let Ok(Ok((head, rest))) =
        tokio::time::timeout(Duration::from_secs(10), read_head(&mut client)).await
    else {
        return;
    };
    match Request::parse(&head) {
        Ok(request) if loopback(&request.host) => {
            if request.port == LOCAL_PORT {
                let _ = client
                    .write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n")
                    .await;
                return;
            }
            let Ok(mut local) = TcpStream::connect(("127.0.0.1", request.port)).await else {
                let _ = client
                    .write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n")
                    .await;
                return;
            };
            let opened = if request.connect {
                client
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await
            } else {
                local.write_all(&request.forwarded_head()).await
            };
            if opened.is_ok() {
                splice(&mut client, &mut local, &rest).await;
            }
        }
        _ => {
            if let Ok(mut host) = UnixStream::connect(&socket).await
                && host.write_all(head.as_bytes()).await.is_ok()
            {
                splice(&mut client, &mut host, &rest).await;
            }
        }
    }
}

/// Send `rest` on to `upstream`, then copy both ways until either side closes.
async fn splice(
    client: &mut (impl AsyncRead + AsyncWrite + Unpin),
    upstream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    rest: &[u8],
) {
    if upstream.write_all(rest).await.is_ok() {
        let _ = tokio::io::copy_bidirectional(client, upstream).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn ask(socket: &std::path::Path, request: &str) -> String {
        let mut stream = UnixStream::connect(socket).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        reply
    }

    #[tokio::test]
    async fn refusals_are_answered_logged_and_collected_once() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join(GUEST_SOCKET);
        let (org, sandbox) = (Uuid::now_v7(), Uuid::now_v7());
        let egress = Egress::new(1);
        let rules = vec!["allowed.invalid".to_owned()];
        egress.start(socket.clone(), org, sandbox, rules).unwrap();
        for (request, reason) in [
            ("CONNECT 10.0.0.1:443 HTTP/1.1\r\n\r\n", "ip_literal"),
            ("CONNECT allowed.invalid:22 HTTP/1.1\r\n\r\n", "port"),
            (
                "CONNECT other.invalid:443 HTTP/1.1\r\n\r\n",
                "host_not_allowed",
            ),
            // The first admitted connection uses the minute's allowance (and finds no address).
            ("CONNECT allowed.invalid:443 HTTP/1.1\r\n\r\n", "dns_failed"),
            (
                "GET http://allowed.invalid/ HTTP/1.1\r\n\r\n",
                "rate_limited",
            ),
        ] {
            let reply = ask(&socket, request).await;
            assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
            assert!(reply.contains(reason), "{reason}: {reply}");
        }
        let (boot, records) = egress.collect(None, 0, 100);
        let reasons: Vec<&str> = records.iter().map(|r| r.reason.as_str()).collect();
        assert_eq!(
            reasons,
            [
                "ip_literal",
                "port",
                "host_not_allowed",
                "dns_failed",
                "rate_limited"
            ]
        );
        assert!(
            records
                .iter()
                .all(|r| !r.allowed && r.org_id == org && r.sandbox_id == sandbox)
        );
        let last = records.last().unwrap().seq;
        assert!(egress.collect(Some(boot), last, 100).1.is_empty());
        assert_eq!(egress.collect(Some(boot), last - 2, 100).1.len(), 2);
        // A cursor from another process, or another reader, starts over.
        assert_eq!(egress.collect(Some(Uuid::now_v7()), last, 100).1.len(), 5);
        assert_eq!(egress.collect(None, 0, 100).1.len(), 5);
        egress.stop(sandbox);
    }

    #[tokio::test]
    async fn the_guest_serves_loopback_requests_locally_and_forwards_the_rest() {
        // A local service that answers with the request line it got.
        let service = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = service.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = service.accept().await.unwrap();
            let (head, _) = read_head(&mut stream).await.unwrap();
            let line = head.lines().next().unwrap().to_owned();
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{line}",
                line.len()
            );
            stream.write_all(reply.as_bytes()).await.unwrap();
        });
        // The host's proxy socket, which records what reaches it.
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join(GUEST_SOCKET);
        let host = UnixListener::bind(&socket).unwrap();
        let reached = tokio::spawn(async move {
            let (mut stream, _) = host.accept().await.unwrap();
            read_head(&mut stream).await.unwrap().0
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = listener.local_addr().unwrap();
        tokio::spawn(forward(listener, socket));
        let mut client = TcpStream::connect(proxy).await.unwrap();
        let local =
            format!("GET http://127.0.0.1:{port}/v1/messages HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        client.write_all(local.as_bytes()).await.unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).await.unwrap();
        assert!(reply.ends_with("GET /v1/messages HTTP/1.1"), "{reply}");
        let mut client = TcpStream::connect(proxy).await.unwrap();
        client
            .write_all(b"CONNECT pypi.org:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        assert!(reached.await.unwrap().starts_with("CONNECT pypi.org:443"));
    }

    #[test]
    fn rules_match_exact_names_and_strict_subdomains() {
        let rules = vec!["pypi.org".to_owned(), "*.githubusercontent.com".to_owned()];
        assert!(allowed(&rules, "pypi.org"));
        assert!(!allowed(&rules, "evilpypi.org"));
        assert!(!allowed(&rules, "pypi.org.evil.test"));
        assert!(allowed(&rules, "raw.githubusercontent.com"));
        assert!(!allowed(&rules, "githubusercontent.com"));
        assert!(!allowed(&rules, "xgithubusercontent.com"));
        assert!(valid_rule("*.npmjs.org") && valid_rule("deb.debian.org"));
        for bad in [
            "*",
            "localhost",
            "Upper.org",
            "a..b",
            "-a.org",
            "*.*.org",
            "a.org:443",
        ] {
            assert!(!valid_rule(bad), "{bad}");
        }
    }

    #[test]
    fn only_public_unicast_addresses_are_reachable() {
        for internal in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "198.18.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
        ] {
            assert!(!public(internal.parse().unwrap()), "{internal}");
        }
        for external in ["151.101.0.223", "2606:4700::1111", "::ffff:8.8.8.8"] {
            assert!(public(external.parse().unwrap()), "{external}");
        }
    }

    #[test]
    fn requests_parse_into_targets_and_plain_http_is_rewritten() {
        let connect =
            Request::parse("CONNECT pypi.org:443 HTTP/1.1\r\nHost: pypi.org:443").unwrap();
        assert!(connect.connect && connect.host == "pypi.org" && connect.port == 443);
        let plain = Request::parse(
            "GET http://Deb.Debian.org./debian/dists HTTP/1.1\r\nHost: deb.debian.org\r\nProxy-Connection: keep-alive\r\nConnection: keep-alive",
        )
        .unwrap();
        assert_eq!((plain.host.as_str(), plain.port), ("deb.debian.org", 80));
        let head = String::from_utf8(plain.forwarded_head()).unwrap();
        assert!(head.starts_with("GET /debian/dists HTTP/1.1\r\n"));
        assert!(
            head.contains("Host: deb.debian.org\r\n")
                && head.ends_with("Connection: close\r\n\r\n")
        );
        assert!(!head.to_ascii_lowercase().contains("keep-alive"));
        assert_eq!(
            Request::parse("GET https://pypi.org/ HTTP/1.1").unwrap_err(),
            "unsupported_scheme"
        );
        assert_eq!(
            Request::parse("GET / HTTP/1.1").unwrap_err(),
            "unsupported_scheme"
        );
        let v6 = Request::parse("CONNECT [::1]:443 HTTP/1.1").unwrap();
        assert_eq!(v6.host, "::1");
    }
}
