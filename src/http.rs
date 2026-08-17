//! MCP over loopback HTTP, so a host can reach a product at a URL instead of
//! spawning a private child it also owns the lifetime of.
//!
//! Every host launches an MCP server as its own stdio child. That makes the host
//! the owner of the server's process lifetime: when it tears the child down — or
//! cancels it — the server dies with it and there is nothing left to reconnect
//! to. A child cannot repair a binding the host closed on its own side.
//!
//! This serves any product's dispatcher over HTTP from a process the host did
//! not start and cannot kill. The framing is deliberately the smallest thing
//! that satisfies MCP's Streamable HTTP shape for a local, single-user endpoint:
//! `POST /mcp` carrying one JSON-RPC message, one JSON message back. There is no
//! new dispatcher and no new state — [`respond`] is a pure function of the
//! request and the dispatch closure, which is what makes it testable without a
//! socket or a store.
//!
//! Scope is loopback plus a bearer token, by construction: [`Hub::bind`] refuses
//! any address that is not loopback rather than trusting a caller to pass one.
//!
//! Everything here is product-neutral. A product supplies only its name and its
//! default port through [`Hub`]; the wire behavior is identical across products
//! so a transport fix lands once for all of them.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};

/// The only path that dispatches. Anything else is a 404 — a stray probe must
/// never reach the tool surface.
const MCP_PATH: &str = "/mcp";

/// Cap on a single request so a malformed or hostile `Content-Length` cannot
/// make the server allocate without bound. Generous next to real tool payloads.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// JSON-RPC parse-error code, returned for a body that is not JSON.
const PARSE_ERROR: i32 = -32700;

/// One product's hub identity: everything about this transport that differs
/// between products, and nothing else.
///
/// Deliberately two fields. The transport's security and framing rules must not
/// vary by product — a per-product knob on auth ordering or loopback binding is
/// exactly the divergence this module exists to prevent — so the only things a
/// product chooses are what it is called and where it listens by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hub {
    name: &'static str,
    default_port: u16,
}

impl Hub {
    /// Declare a product's hub. `name` is used for the token directory and for
    /// operator-facing prose; `default_port` is the stable port its host configs
    /// pin.
    pub const fn new(name: &'static str, default_port: u16) -> Self {
        Self { name, default_port }
    }

    /// The product name, as it appears in operator-facing errors.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The fixed loopback port this product listens on unless told otherwise. A
    /// stable default is the point: a host config pins a URL once and keeps
    /// working across restarts, which an ephemeral port cannot offer.
    pub fn default_port(&self) -> u16 {
        self.default_port
    }

    /// Bind a loopback listener, refusing any non-loopback address.
    ///
    /// The refusal is structural rather than documented: there is no argument a
    /// caller can pass that exposes this endpoint off-machine, so a future
    /// caller cannot widen the blast radius by accident.
    pub fn bind(&self, addr: SocketAddr) -> Result<TcpListener, String> {
        if !addr.ip().is_loopback() {
            return Err(format!(
                "refusing to bind {addr}: `{} serve` is loopback-only",
                self.name
            ));
        }
        TcpListener::bind(addr).map_err(|e| format!("bind {addr}: {e}"))
    }

    /// The URL a host connects to for a given port.
    pub fn url_for(&self, port: u16) -> String {
        format!("http://{}/mcp", loopback(port))
    }

    /// The one URL both sides agree on — what `<product> serve` listens on by
    /// default and what `<product> enable` writes into host configs. Deriving
    /// both from this single function is what makes a default-port change
    /// impossible to apply to only one of them.
    pub fn default_url(&self) -> String {
        self.url_for(self.default_port)
    }

    /// The default URL with a repository pinned onto it, as `<product> enable`
    /// writes it into a host config. A connection opened at this URL defaults to
    /// that repository, so a call omitting `root` again means the repo the agent
    /// stands in, while an explicit `root` still reaches any other managed repo.
    pub fn pinned_url(&self, root: &Path) -> String {
        format!(
            "{}?root={}",
            self.default_url(),
            percent_encode(&root.display().to_string())
        )
    }

    /// Where this product's bearer token is persisted so a host config and the
    /// server agree on it across restarts.
    pub fn token_path(&self) -> Result<PathBuf, String> {
        let dirs = directories::BaseDirs::new().ok_or("cannot resolve a home directory")?;
        Ok(dirs.config_dir().join(self.name).join("serve-token"))
    }
}

/// One parsed HTTP request: only the parts this endpoint acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub bearer: Option<String>,
    pub body: String,
    /// The repository this connection defaults to, from `?root=` on the endpoint
    /// URL. `None` is the neutral hub — a repository-bound call that omits
    /// `root` stays a typed missing-target error.
    pub pinned_root: Option<String>,
}

/// What to write back: an HTTP status and a body (empty for 202).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

impl Response {
    fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }
}

/// Decide the response for one request.
///
/// Pure on purpose: `dispatch` stands in for `McpServer::handle_line`, so the
/// auth, routing, and framing rules are testable without a server, a store, or a
/// socket — and a test can prove a rejected request never dispatched at all.
///
/// A bad request is answered, never met by a closed transport. A body that is
/// not JSON comes back as a JSON-RPC parse error with HTTP 200, because the HTTP
/// layer delivered it fine — the fault is in the payload, and reporting it as a
/// transport failure would send the client hunting the wrong problem.
pub fn respond<F>(dispatch: F, token: &str, request: &Request) -> Response
where
    F: FnOnce(&str) -> Option<String>,
{
    if request.path != MCP_PATH {
        return Response::new(404, r#"{"error":"not found"}"#);
    }
    // Auth precedes method checks: an unauthenticated caller learns only that it
    // is unauthenticated, never which methods the endpoint would have accepted.
    if request.bearer.as_deref() != Some(token) {
        return Response::new(401, r#"{"error":"unauthorized"}"#);
    }
    if request.method != "POST" {
        return Response::new(405, r#"{"error":"method not allowed"}"#);
    }
    if serde_json::from_str::<serde_json::Value>(&request.body).is_err() {
        return Response::new(200, parse_error_frame());
    }
    let body = apply_pinned_root(&request.body, request.pinned_root.as_deref());
    match dispatch(&body) {
        // A JSON-RPC notification has no reply. 202 says "accepted, nothing to
        // return" without inventing a response body the client would try to parse.
        None => Response::new(202, String::new()),
        Some(reply) => Response::new(200, reply),
    }
}

fn parse_error_frame() -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": serde_json::Value::Null,
        "error": { "code": PARSE_ERROR, "message": "parse error: body is not valid JSON" }
    })
    .to_string()
}

/// Read one HTTP request. Returns `Ok(None)` when the peer closed without
/// sending anything, which is an ordinary probe, not an error.
pub fn read_request(stream: &mut impl Read) -> Result<Option<Request>, String> {
    let mut reader = BufReader::new(stream);
    let mut start = String::new();
    if reader
        .read_line(&mut start)
        .map_err(|e| format!("read request line: {e}"))?
        == 0
    {
        return Ok(None);
    }
    let mut parts = start.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    // Routing is still on the path alone; the query carries only the connection
    // default, never a second way to reach a different endpoint.
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), Some(query.to_string())),
        None => (target, None),
    };
    let pinned_root = query.as_deref().and_then(parse_pinned_root);

    let mut content_length = 0usize;
    let mut bearer = None;
    loop {
        let mut line = String::new();
        if reader
            .read_line(&mut line)
            .map_err(|e| format!("read header: {e}"))?
            == 0
        {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => {
                content_length = value
                    .parse::<usize>()
                    .map_err(|_| format!("invalid Content-Length: {value}"))?;
                if content_length > MAX_BODY_BYTES {
                    return Err(format!("body too large: {content_length} bytes"));
                }
            }
            "authorization" => {
                bearer = value
                    .strip_prefix("Bearer ")
                    .or_else(|| value.strip_prefix("bearer "))
                    .map(|t| t.trim().to_string());
            }
            _ => {}
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader
            .read_exact(&mut body)
            .map_err(|e| format!("read body: {e}"))?;
    }
    let body = String::from_utf8(body).map_err(|_| "body is not valid UTF-8".to_string())?;

    Ok(Some(Request {
        method,
        path,
        bearer,
        body,
        pinned_root,
    }))
}

/// Read `root=` out of a query string, percent-decoded. An empty value is
/// `None`: a client that sends `?root=` has pinned nothing, and treating that as
/// a repository would invent a target out of a blank.
fn parse_pinned_root(query: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == "root")
        .map(|(_, value)| percent_decode(value))
        .filter(|root| !root.trim().is_empty())
}

/// Percent-decode one query value. A repository path routinely contains a space
/// or other character a host will encode, and a mangled path must not reach
/// routing as if it were a real one.
///
/// `+` is left as a literal `+`, not turned into a space: this value is a
/// filesystem path, where `+` is an ordinary character, and the form-encoding
/// convention would corrupt every directory legitimately named with one.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    // A percent-escape that decodes to invalid UTF-8 cannot name a path we could
    // canonicalize; keeping it lossy hands routing a value that fails closed
    // rather than panicking the connection thread.
    String::from_utf8_lossy(&out).into_owned()
}

/// Apply the connection's pinned root to one JSON-RPC message.
///
/// This restores a repo adapter's launch-root default for an adapter that now
/// reaches a hub over HTTP instead of spawning its own repo-local child. The
/// default is fixed per connection by client configuration and read per request;
/// it is never mutated by a call, so this is not a process-global current repo.
///
/// Only `tools/call` is touched, and only when the caller supplied no `root`: an
/// explicit argument always wins, which is what keeps cross-repo work reachable
/// from any connection. The pinned value is not validated here — it flows into
/// the same per-request canonicalization every explicit `root` gets, so a
/// nonexistent or unmanaged pin fails closed there instead of falling back.
fn apply_pinned_root(body: &str, pinned: Option<&str>) -> String {
    let Some(pinned) = pinned else {
        return body.to_string();
    };
    let Ok(mut message) = serde_json::from_str::<serde_json::Value>(body) else {
        return body.to_string();
    };
    if message.get("method").and_then(|m| m.as_str()) != Some("tools/call") {
        return body.to_string();
    }
    let Some(params) = message.get_mut("params").and_then(|p| p.as_object_mut()) else {
        return body.to_string();
    };
    let arguments = params
        .entry("arguments")
        .or_insert_with(|| serde_json::json!({}));
    let Some(arguments) = arguments.as_object_mut() else {
        return body.to_string();
    };
    // Only an absent key takes the default. A `root` that is present but empty or
    // not a string is the caller's error, and it must reach the routing kernel to
    // earn its typed rejection — silently replacing it with the pin would answer a
    // malformed call as if it had been a valid one.
    if arguments.contains_key("root") {
        return body.to_string();
    }
    arguments.insert(
        "root".to_string(),
        serde_json::Value::String(pinned.to_string()),
    );
    message.to_string()
}

/// Serialize a response as HTTP/1.1. `Connection: close` keeps the framing
/// honest: one request per connection, so a half-read body can never be
/// mistaken for the next request's start.
pub fn write_response(stream: &mut impl Write, response: &Response) -> Result<(), String> {
    let reason = match response.status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        reason,
        response.body.len()
    );
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(response.body.as_bytes()))
        .and_then(|()| stream.flush())
        .map_err(|e| format!("write response: {e}"))
}

/// The default listen address for a given port.
pub fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

/// Percent-encode one query value, the inverse of [`percent_decode`].
///
/// Everything outside an unreserved set is escaped, so a path containing a space,
/// `&`, `=`, `#`, or `%` survives the round trip rather than truncating the value
/// or inventing a second query parameter. `+` is escaped too: the decoder
/// deliberately treats a literal `+` as itself, and encoding it keeps the pair
/// honest for any other reader that applies form-decoding.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Serve one already-accepted connection.
pub fn serve_connection<F>(stream: &mut TcpStream, token: &str, dispatch: F)
where
    F: FnOnce(&str) -> Option<String>,
{
    let response = match read_request(stream) {
        Ok(None) => return,
        Ok(Some(request)) => respond(dispatch, token, &request),
        // A request we could not even parse still gets an answer. Closing here
        // would be a closed-transport failure in a new place.
        Err(error) => Response::new(400, serde_json::json!({ "error": error }).to_string()),
    };
    let _ = write_response(stream, &response);
}

/// Load the persisted bearer token, creating one on first run.
///
/// Written 0600 on unix. The token is the only thing standing between any local
/// process and a tool surface that can mutate a repository, so it is generated
/// from OS entropy, never from time or pid.
pub fn ensure_token(path: &Path) -> Result<String, String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return Ok(existing);
        }
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| format!("cannot generate a token: {e}"))?;
    let token = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, format!("{token}\n"))
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(token)
}

impl Hub {
    /// Bind `port`, print the operator banner, and serve until killed.
    ///
    /// This is the entire `<product> serve` command body. A product supplies only
    /// its dispatcher, so the token handling, the loopback guard, the banner's
    /// shape, and the accept loop's failure policy are one implementation across
    /// every product rather than one per product drifting apart.
    ///
    /// Returns a process exit code: non-zero only when the hub could not start.
    pub fn serve<F>(&self, port: u16, dispatch: F) -> i32
    where
        F: Fn(&str) -> Option<String> + Clone + Send + 'static,
    {
        let name = self.name;
        // Bind before touching the token. A second `serve` losing the port race
        // must fail without having written anything: the token is shared state a
        // running server and its pinned host configs already agree on, and a
        // process that will never serve has no business creating or rewriting it.
        let addr = loopback(port);
        let listener = match self.bind(addr) {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("{name} serve: {error}");
                return 1;
            }
        };
        let token_path = match self.token_path() {
            Ok(path) => path,
            Err(error) => {
                eprintln!("{name} serve: {error}");
                return 1;
            }
        };
        let token = match ensure_token(&token_path) {
            Ok(token) => token,
            Err(error) => {
                eprintln!("{name} serve: {error}");
                return 1;
            }
        };
        let bound = listener.local_addr().unwrap_or(addr);

        println!("{name} serve — MCP over HTTP at http://{bound}/mcp");
        println!("  token: {}", token_path.display());
        println!("  host config: url = \"http://{bound}/mcp\"");
        println!("  bearer:      {token}");

        if let Err(error) = self.run(listener, token, dispatch) {
            eprintln!("{name} serve: {error}");
            return 1;
        }
        0
    }

    /// Serve each accepted connection on its own thread through `dispatch`.
    ///
    /// Re-checks that the listener is loopback and refuses otherwise. [`bind`]
    /// cannot be the only guard once this is public: a caller can hand over a
    /// listener it bound itself, and accepting one would expose an authenticated
    /// tool surface off-machine through the one entry point that skipped the
    /// check. The guard belongs on every path that serves, not only the one that
    /// binds.
    ///
    /// `dispatch` is cloned per connection because each connection consumes one
    /// `FnOnce`. One refused connection is never a reason to take the service
    /// down: staying up is the entire property this transport exists to provide,
    /// so a failed accept is reported and the loop continues.
    pub fn run<F>(&self, listener: TcpListener, token: String, dispatch: F) -> Result<(), String>
    where
        F: Fn(&str) -> Option<String> + Clone + Send + 'static,
    {
        match listener.local_addr() {
            Ok(addr) if addr.ip().is_loopback() => {}
            Ok(addr) => {
                return Err(format!(
                    "refusing to serve {addr}: `{} serve` is loopback-only",
                    self.name
                ))
            }
            // An address we cannot read is an address we cannot vouch for.
            Err(error) => {
                return Err(format!(
                    "refusing to serve: cannot confirm the listener is loopback: {error}"
                ))
            }
        }
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(stream) => stream,
                Err(error) => {
                    eprintln!("{} serve: accept failed: {error}", self.name);
                    continue;
                }
            };
            let token = token.clone();
            let dispatch = dispatch.clone();
            std::thread::spawn(move || {
                serve_connection(&mut stream, &token, |line| dispatch(line));
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A stand-in product, so no test depends on which product happens to ship
    /// this crate.
    const HUB: Hub = Hub::new("testprod", 7977);

    fn request(method: &str, path: &str, bearer: Option<&str>, body: &str) -> Request {
        Request {
            method: method.to_string(),
            path: path.to_string(),
            bearer: bearer.map(str::to_string),
            body: body.to_string(),
            pinned_root: None,
        }
    }

    fn pinned(method: &str, bearer: Option<&str>, body: &str, root: &str) -> Request {
        Request {
            pinned_root: Some(root.to_string()),
            ..request(method, "/mcp", bearer, body)
        }
    }

    fn tools_call(arguments: &str) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"a_tool","arguments":{arguments}}}}}"#
        )
    }

    /// What the dispatcher actually received, so a test can assert on the line
    /// the server would have handled rather than on the reply's shape.
    fn capture(seen: &Cell<Option<String>>) -> impl FnOnce(&str) -> Option<String> + '_ {
        move |line: &str| {
            seen.set(Some(line.to_string()));
            Some(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string())
        }
    }

    fn dispatched(request: &Request) -> serde_json::Value {
        let seen = Cell::new(None);
        respond(capture(&seen), "secret", request);
        serde_json::from_str(&seen.take().expect("request should have dispatched"))
            .expect("dispatched line is JSON")
    }

    fn dispatched_root(request: &Request) -> Option<String> {
        dispatched(request)
            .pointer("/params/arguments/root")
            .and_then(|root| root.as_str())
            .map(str::to_string)
    }

    fn echo(line: &str) -> Option<String> {
        Some(format!(r#"{{"jsonrpc":"2.0","id":1,"echo":{line}}}"#))
    }

    #[test]
    fn post_mcp_with_a_valid_token_dispatches_and_returns_the_reply() {
        let response = respond(
            echo,
            "secret",
            &request("POST", "/mcp", Some("secret"), r#"{"method":"initialize"}"#),
        );
        assert_eq!(response.status, 200);
        assert!(response.body.contains(r#""method":"initialize""#));
    }

    #[test]
    fn a_notification_returns_202_with_no_body() {
        let response = respond(
            |_| None,
            "secret",
            &request("POST", "/mcp", Some("secret"), r#"{"method":"notify"}"#),
        );
        assert_eq!(response.status, 202);
        assert!(response.body.is_empty(), "no body to misparse");
    }

    /// The security property, asserted as behavior rather than inspection: a
    /// rejected request must not reach the dispatcher at all.
    #[test]
    fn a_missing_or_wrong_token_is_401_and_never_dispatches() {
        for bearer in [None, Some("wrong")] {
            let called = Cell::new(false);
            let response = respond(
                |line| {
                    called.set(true);
                    echo(line)
                },
                "secret",
                &request("POST", "/mcp", bearer, r#"{"method":"initialize"}"#),
            );
            assert_eq!(response.status, 401, "bearer={bearer:?}");
            assert!(!called.get(), "dispatch must not run for bearer={bearer:?}");
        }
    }

    #[test]
    fn another_path_is_404_and_never_dispatches() {
        let called = Cell::new(false);
        let response = respond(
            |line| {
                called.set(true);
                echo(line)
            },
            "secret",
            &request("POST", "/admin", Some("secret"), "{}"),
        );
        assert_eq!(response.status, 404);
        assert!(
            !called.get(),
            "a stray path must not reach the tool surface"
        );
    }

    #[test]
    fn a_wrong_method_on_mcp_is_405() {
        let response = respond(echo, "secret", &request("GET", "/mcp", Some("secret"), ""));
        assert_eq!(response.status, 405);
    }

    /// A malformed payload is answered, never met with a closed transport, and is
    /// reported as a JSON-RPC fault rather than an HTTP one.
    #[test]
    fn a_malformed_body_returns_a_jsonrpc_parse_error_not_a_closed_transport() {
        let response = respond(
            echo,
            "secret",
            &request("POST", "/mcp", Some("secret"), "not json at all"),
        );
        assert_eq!(response.status, 200, "the HTTP layer delivered it fine");
        let parsed: serde_json::Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(parsed["error"]["code"], PARSE_ERROR);
    }

    #[test]
    fn read_request_parses_method_path_bearer_and_body() {
        let raw = "POST /mcp?x=1 HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer abc\r\nContent-Length: 9\r\n\r\n{\"a\":\"b\"}";
        let mut cursor = std::io::Cursor::new(raw.as_bytes().to_vec());
        let request = read_request(&mut cursor).unwrap().unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/mcp", "query string is not part of routing");
        assert_eq!(request.bearer.as_deref(), Some("abc"));
        assert_eq!(request.body, r#"{"a":"b"}"#);
    }

    // The connection-pinned default root.

    #[test]
    fn a_pinned_connection_supplies_the_root_a_call_omitted() {
        let request = pinned("POST", Some("secret"), &tools_call("{}"), "/repos/alpha");
        assert_eq!(
            dispatched_root(&request).as_deref(),
            Some("/repos/alpha"),
            "a repo adapter's own repository is the default again"
        );
    }

    #[test]
    fn an_unpinned_connection_leaves_the_call_exactly_as_sent() {
        let body = tools_call("{}");
        let request = request("POST", "/mcp", Some("secret"), &body);
        assert_eq!(
            dispatched_root(&request),
            None,
            "the neutral hub still has no implicit repo, so this stays a typed missing-target error downstream"
        );
        let seen = Cell::new(None);
        respond(capture(&seen), "secret", &request);
        assert_eq!(
            seen.take().as_deref(),
            Some(body.as_str()),
            "byte-identical"
        );
    }

    #[test]
    fn an_explicit_root_wins_over_the_pin_so_cross_repo_still_works() {
        let request = pinned(
            "POST",
            Some("secret"),
            &tools_call(r#"{"root":"/repos/beta"}"#),
            "/repos/alpha",
        );
        assert_eq!(
            dispatched_root(&request).as_deref(),
            Some("/repos/beta"),
            "naming another repository from a pinned connection must still reach it"
        );
    }

    #[test]
    fn a_present_but_malformed_root_is_never_replaced_by_the_pin() {
        for supplied in [r#"{"root":""}"#, r#"{"root":5}"#] {
            let request = pinned(
                "POST",
                Some("secret"),
                &tools_call(supplied),
                "/repos/alpha",
            );
            let arguments = dispatched(&request);
            let root = arguments.pointer("/params/arguments/root").unwrap();
            assert_ne!(
                root,
                &serde_json::json!("/repos/alpha"),
                "a malformed root must earn its typed rejection, not be silently fixed"
            );
        }
    }

    #[test]
    fn only_tools_call_is_rewritten() {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let seen = Cell::new(None);
        respond(
            capture(&seen),
            "secret",
            &pinned("POST", Some("secret"), body, "/repos/alpha"),
        );
        assert_eq!(
            seen.take().as_deref(),
            Some(body),
            "handshake and listing traffic is not repository-bound"
        );
    }

    #[test]
    fn a_pin_never_reaches_the_tool_surface_without_a_valid_token() {
        let seen = Cell::new(None);
        let response = respond(
            capture(&seen),
            "secret",
            &pinned("POST", Some("wrong"), &tools_call("{}"), "/repos/alpha"),
        );
        assert_eq!(response.status, 401);
        assert!(seen.take().is_none(), "auth still precedes every rewrite");
    }

    /// `enable` writes the URL and the hub reads it back. The two halves are
    /// asserted together, because a path that survives one and not the other
    /// routes an agent to the wrong repository — or to none.
    #[test]
    fn a_pinned_url_round_trips_through_the_hubs_own_decoder() {
        for path in [
            "/home/juno/ishoo",
            "/home/juno/dawn dish soap",
            "/repos/c++",
            "/repos/a&b=c",
            "/repos/100% real",
            "/repos/plus+name",
            "/repos/ünïcode",
        ] {
            let url = HUB.pinned_url(Path::new(path));
            assert!(
                !url[url.find('?').unwrap()..].contains(' '),
                "no raw space in a URL: {url}"
            );
            let query = url.split_once('?').expect("pinned URL carries a query").1;
            assert_eq!(
                parse_pinned_root(query).as_deref(),
                Some(path),
                "round trip failed for {path} via {url}"
            );
        }
    }

    #[test]
    fn read_request_parses_a_percent_encoded_pinned_root() {
        let raw =
            "POST /mcp?root=%2Fhome%2Fjuno%2Fdawn%20dish HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}";
        let mut cursor = std::io::Cursor::new(raw.as_bytes().to_vec());
        let request = read_request(&mut cursor).unwrap().unwrap();
        assert_eq!(request.path, "/mcp", "routing is still the path alone");
        assert_eq!(
            request.pinned_root.as_deref(),
            Some("/home/juno/dawn dish"),
            "a path with a space survives the query"
        );
    }

    #[test]
    fn a_plus_in_a_pinned_root_stays_a_plus() {
        assert_eq!(
            parse_pinned_root("root=/repos/c++"),
            Some("/repos/c++".to_string()),
            "this is a filesystem path, not a form field"
        );
    }

    #[test]
    fn an_empty_or_absent_pin_is_no_pin() {
        assert_eq!(parse_pinned_root("root="), None);
        assert_eq!(parse_pinned_root("other=1"), None);
        assert_eq!(parse_pinned_root(""), None);
    }

    #[test]
    fn read_request_refuses_an_oversized_content_length() {
        let raw = format!(
            "POST /mcp HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        let mut cursor = std::io::Cursor::new(raw.into_bytes());
        assert!(read_request(&mut cursor).is_err());
    }

    #[test]
    fn bind_refuses_a_non_loopback_address() {
        let addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let error = HUB.bind(addr).unwrap_err();
        assert!(error.contains("loopback-only"), "got: {error}");
        assert!(
            error.contains("testprod"),
            "the product names itself in its own operator error: {error}"
        );
    }

    /// The loopback guard must hold on every path that serves, not only the one
    /// that binds — `run` is public, so a caller can arrive with its own listener.
    #[test]
    fn run_refuses_a_listener_that_is_not_loopback() {
        let Ok(listener) = TcpListener::bind("0.0.0.0:0") else {
            return; // A sandbox that forbids the bind cannot exhibit the risk.
        };
        let error = HUB
            .run(listener, "secret".to_string(), |_| None)
            .expect_err("a non-loopback listener must never be served");
        assert!(error.contains("loopback-only"), "got: {error}");
    }

    /// Two products must not share a token file: one product's compromised token
    /// would otherwise open the other's tool surface.
    #[test]
    fn each_product_gets_its_own_token_path_and_url() {
        let other = Hub::new("otherprod", 7988);
        assert_ne!(HUB.token_path().unwrap(), other.token_path().unwrap());
        assert_ne!(HUB.default_url(), other.default_url());
        assert!(HUB.default_url().contains("7977"));
        assert!(other.default_url().contains("7988"));
    }

    #[test]
    fn ensure_token_creates_once_then_reuses_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("serve-token");
        let first = ensure_token(&path).unwrap();
        assert_eq!(first.len(), 64, "32 bytes of entropy, hex encoded");
        assert_eq!(
            ensure_token(&path).unwrap(),
            first,
            "stable across restarts"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "token must not be world-readable");
        }
    }

    /// The whole point of the transport, proven over a real socket: the server
    /// outlives a client that goes away mid-session and answers the next one.
    #[test]
    fn the_server_survives_a_client_that_disconnects_and_answers_the_next_request() {
        use std::io::Write as _;
        let listener = HUB.bind(loopback(0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut served = 0;
            for stream in listener.incoming().take(2) {
                let mut stream = stream.unwrap();
                serve_connection(&mut stream, "secret", echo);
                served += 1;
            }
            served
        });

        // A client that opens a connection, writes a partial request, and dies.
        let mut aborted = std::net::TcpStream::connect(addr).unwrap();
        aborted.write_all(b"POST /mcp HTTP/1.1\r\n").unwrap();
        drop(aborted);

        // The next client is answered normally by the same still-running server.
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        let body = r#"{"method":"initialize"}"#;
        write!(
            stream,
            "POST /mcp HTTP/1.1\r\nAuthorization: Bearer secret\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 200 OK"), "got: {reply}");
        assert!(reply.contains(r#""method":"initialize""#));
        assert_eq!(handle.join().unwrap(), 2);
    }
}
