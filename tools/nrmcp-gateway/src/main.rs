//! Host bridge between standard MCP Streamable HTTP and NightRun's bounded
//! UDP transport. HTTP exposure is loopback-only by default and requires a
//! bearer token.

use std::collections::HashMap;
use std::env;
use std::io::Read;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use nr_mcp::{encode_fragment, Kind, Reassembler, MAX_DATAGRAM};
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_MESSAGE: usize = 64 * 1024;

type Pending = Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>>;
type DevicePeer = Arc<Mutex<Option<SocketAddr>>>;
type AnyError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug)]
struct Options {
    udp_bind: String,
    http_bind: String,
    token: String,
    upstream: Option<String>,
    upstream_token: Option<String>,
}

fn main() -> Result<(), AnyError> {
    let options = parse_options()?;
    let udp = Arc::new(UdpSocket::bind(&options.udp_bind)?);
    udp.set_read_timeout(Some(Duration::from_millis(250)))?;
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let device_peer: DevicePeer = Arc::new(Mutex::new(None));
    let ids = Arc::new(AtomicU32::new(1));

    let http_udp = Arc::clone(&udp);
    let http_pending = Arc::clone(&pending);
    let http_peer = Arc::clone(&device_peer);
    let http_ids = Arc::clone(&ids);
    let http_token = options.token.clone();
    let http_bind = options.http_bind.clone();
    thread::spawn(move || {
        if let Err(error) = serve_http(
            &http_bind,
            http_udp,
            http_pending,
            http_peer,
            http_ids,
            http_token,
        ) {
            eprintln!("HTTP server stopped: {error}");
        }
    });

    println!("MCP endpoint: http://{}/mcp", options.http_bind);
    println!("UDP bridge:   {}", options.udp_bind);
    println!("Waiting for NightRun hello...");

    let mut reassembler = Reassembler::<MAX_MESSAGE>::default();
    let mut datagram = [0u8; MAX_DATAGRAM];
    let mut upstream = options
        .upstream
        .map(|url| Upstream::new(url, options.upstream_token));
    loop {
        let (len, peer) = match udp.recv_from(&mut datagram) {
            Ok(value) => value,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let complete = match reassembler.push(&datagram[..len]) {
            Ok(Some(message)) => message,
            Ok(None) => continue,
            Err(error) => {
                eprintln!("Dropped bridge datagram from {peer}: {error:?}");
                continue;
            }
        };
        match complete.kind {
            Kind::Hello => {
                *device_peer.lock().unwrap() = Some(peer);
                println!("NightRun connected from {peer}");
            }
            Kind::DeviceResponse => {
                *device_peer.lock().unwrap() = Some(peer);
                if let Some(sender) = pending.lock().unwrap().remove(&complete.message_id) {
                    let _ = sender.send(complete.payload.to_vec());
                }
            }
            Kind::DeviceRequest => {
                *device_peer.lock().unwrap() = Some(peer);
                let response = match upstream.as_mut() {
                    Some(client) => client
                        .call(complete.payload)
                        .unwrap_or_else(|error| json_rpc_error(complete.payload, -32000, &error)),
                    None => json_rpc_error(
                        complete.payload,
                        -32001,
                        "gateway has no --upstream MCP endpoint",
                    ),
                };
                send_message(
                    &udp,
                    peer,
                    Kind::HostResponse,
                    complete.message_id,
                    &response,
                )?;
            }
            Kind::HostRequest | Kind::HostResponse => {
                eprintln!("Ignored invalid direction {:?} from {peer}", complete.kind);
            }
        }
    }
}

fn parse_options() -> Result<Options, AnyError> {
    let mut udp_bind = String::from("127.0.0.1:9000");
    let mut http_bind = String::from("127.0.0.1:9001");
    let mut upstream = None;
    let mut token_env = String::from("NRMCP_TOKEN");
    let mut upstream_token_env = None;
    let mut args = env::args().skip(1);
    while let Some(argument) = args.next() {
        let value = |args: &mut std::iter::Skip<env::Args>| {
            args.next()
                .ok_or_else(|| format!("missing value after {argument}"))
        };
        match argument.as_str() {
            "--udp-bind" => udp_bind = value(&mut args)?,
            "--http-bind" => http_bind = value(&mut args)?,
            "--upstream" => upstream = Some(value(&mut args)?),
            "--token-env" => token_env = value(&mut args)?,
            "--upstream-token-env" => upstream_token_env = Some(value(&mut args)?),
            "--help" | "-h" => {
                println!(
                    "nrmcp-gateway [--udp-bind 127.0.0.1:9000] \
                     [--http-bind 127.0.0.1:9001] [--upstream URL] \
                     [--token-env NRMCP_TOKEN] [--upstream-token-env ENV]"
                );
                std::process::exit(0);
            }
            _ => return Err(format!("unknown argument: {argument}").into()),
        }
    }
    let token = env::var(&token_env)
        .map_err(|_| format!("{token_env} must contain the inbound bearer token"))?;
    if token.len() < 24 {
        return Err(format!("{token_env} must be at least 24 characters").into());
    }
    let upstream_token = upstream_token_env
        .map(|name| env::var(&name).map_err(|_| format!("{name} is not set")))
        .transpose()?;
    Ok(Options {
        udp_bind,
        http_bind,
        token,
        upstream,
        upstream_token,
    })
}

fn serve_http(
    bind: &str,
    udp: Arc<UdpSocket>,
    pending: Pending,
    device_peer: DevicePeer,
    ids: Arc<AtomicU32>,
    token: String,
) -> Result<(), AnyError> {
    let server = Server::http(bind)?;
    for request in server.incoming_requests() {
        handle_http(request, &udp, &pending, &device_peer, &ids, &token);
    }
    Ok(())
}

fn handle_http(
    mut request: Request,
    udp: &UdpSocket,
    pending: &Pending,
    device_peer: &DevicePeer,
    ids: &AtomicU32,
    token: &str,
) {
    if request.url() != "/mcp" || request.method() != &Method::Post {
        respond_text(request, 405, "MCP endpoint accepts POST /mcp");
        return;
    }
    if !origin_allowed(&request) {
        respond_text(request, 403, "Origin is not allowed");
        return;
    }
    let expected = format!("Bearer {token}");
    if header_value(&request, "Authorization") != Some(expected.as_str()) {
        respond_text(request, 401, "Missing or invalid bearer token");
        return;
    }
    let Some(peer) = *device_peer.lock().unwrap() else {
        respond_text(request, 503, "NightRun has not connected to the gateway");
        return;
    };
    let mut body = Vec::new();
    if request
        .as_reader()
        .take((MAX_MESSAGE + 1) as u64)
        .read_to_end(&mut body)
        .is_err()
        || body.len() > MAX_MESSAGE
    {
        respond_text(request, 413, "MCP message exceeds 64 KiB");
        return;
    }
    if !valid_json_rpc(&body) {
        respond_json(
            request,
            400,
            br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid JSON-RPC request"}}"#,
        );
        return;
    }
    let message_id = ids.fetch_add(1, Ordering::Relaxed);
    if !json_rpc_has_id(&body) {
        if let Err(error) = send_message(udp, peer, Kind::HostRequest, message_id, &body) {
            respond_text(request, 502, &format!("UDP bridge failed: {error}"));
        } else {
            let _ = request.respond(Response::empty(StatusCode(202)));
        }
        return;
    }
    let (sender, receiver) = mpsc::channel();
    pending.lock().unwrap().insert(message_id, sender);
    if let Err(error) = send_message(udp, peer, Kind::HostRequest, message_id, &body) {
        pending.lock().unwrap().remove(&message_id);
        respond_text(request, 502, &format!("UDP bridge failed: {error}"));
        return;
    }
    match receiver.recv_timeout(Duration::from_secs(600)) {
        Ok(response) => respond_json(request, 200, &response),
        Err(_) => {
            pending.lock().unwrap().remove(&message_id);
            respond_text(request, 504, "NightRun MCP request timed out");
        }
    }
}

fn valid_json_rpc(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("jsonrpc")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .as_deref()
        == Some("2.0")
}

fn json_rpc_has_id(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .is_some_and(|value| value.get("id").is_some())
}

fn origin_allowed(request: &Request) -> bool {
    let Some(origin) = header_value(request, "Origin") else {
        return true;
    };
    origin.starts_with("http://127.0.0.1:")
        || origin.starts_with("http://localhost:")
        || origin == "http://127.0.0.1"
        || origin == "http://localhost"
}

fn header_value<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|header| header.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|header| header.value.as_str())
}

fn respond_text(request: Request, status: u16, text: &str) {
    let response = Response::from_string(text)
        .with_status_code(StatusCode(status))
        .with_header(Header::from_bytes("Content-Type", "text/plain; charset=utf-8").unwrap());
    let _ = request.respond(response);
}

fn respond_json(request: Request, status: u16, body: &[u8]) {
    let response = Response::from_data(body.to_vec())
        .with_status_code(StatusCode(status))
        .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
        .with_header(Header::from_bytes("MCP-Protocol-Version", PROTOCOL_VERSION).unwrap());
    let _ = request.respond(response);
}

fn send_message(
    socket: &UdpSocket,
    peer: SocketAddr,
    kind: Kind,
    message_id: u32,
    message: &[u8],
) -> Result<(), AnyError> {
    let count = nr_mcp::fragment_count(message.len())?;
    let mut datagram = [0u8; MAX_DATAGRAM];
    for index in 0..count {
        let len = encode_fragment(kind, message_id, message, index, &mut datagram)?;
        socket.send_to(&datagram[..len], peer)?;
    }
    Ok(())
}

struct Upstream {
    url: String,
    token: Option<String>,
    session: Option<String>,
    initialized: bool,
}

impl Upstream {
    fn new(url: String, token: Option<String>) -> Self {
        Self {
            url,
            token,
            session: None,
            initialized: false,
        }
    }

    fn call(&mut self, body: &[u8]) -> Result<Vec<u8>, String> {
        self.ensure_initialized()?;
        self.post(body)
    }

    fn ensure_initialized(&mut self) -> Result<(), String> {
        if self.initialized {
            return Ok(());
        }
        let initialize = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": "nrmcp-gateway-init",
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "nrmcp-gateway",
                    "title": "NightRun MCP Gateway",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        }))
        .unwrap();
        let response = self.post(&initialize)?;
        let value: Value = serde_json::from_slice(&response)
            .map_err(|error| format!("invalid initialize response: {error}"))?;
        if value.get("result").is_none() {
            return Err(String::from("upstream rejected MCP initialization"));
        }
        let initialized = br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let _ = self.post(initialized)?;
        self.initialized = true;
        Ok(())
    }

    fn post(&mut self, body: &[u8]) -> Result<Vec<u8>, String> {
        let request_id = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|value| value.get("id").cloned());
        let mut request = ureq::post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PROTOCOL_VERSION);
        if let Some(token) = self.token.as_deref() {
            request = request.header("Authorization", &format!("Bearer {token}"));
        }
        if let Some(session) = self.session.as_deref() {
            request = request.header("Mcp-Session-Id", session);
        }
        let mut response = request
            .send(body)
            .map_err(|error| format!("upstream request failed: {error}"))?;
        if self.session.is_none() {
            self.session = response
                .headers()
                .get("mcp-session-id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
        }
        let is_sse = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"));
        let response = response
            .body_mut()
            .read_to_vec()
            .map_err(|error| format!("upstream response failed: {error}"))?;
        if response.len() > MAX_MESSAGE {
            return Err(String::from("upstream response exceeds 64 KiB"));
        }
        if is_sse {
            extract_sse_json(&response, request_id.as_ref())
        } else {
            Ok(response)
        }
    }
}

fn extract_sse_json(body: &[u8], expected_id: Option<&Value>) -> Result<Vec<u8>, String> {
    let text = core::str::from_utf8(body).map_err(|_| String::from("upstream SSE is not UTF-8"))?;
    for event in text.split("\n\n") {
        let mut data = String::new();
        for line in event.lines() {
            if let Some(value) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value.trim_start());
            }
        }
        if !data.is_empty() {
            if let Ok(value) = serde_json::from_str::<Value>(&data) {
                if expected_id.is_none() || value.get("id") == expected_id {
                    return Ok(data.into_bytes());
                }
            }
        }
    }
    Err(String::from("upstream SSE contained no JSON-RPC message"))
}

fn json_rpc_error(request: &[u8], code: i32, message: &str) -> Vec<u8> {
    let id = serde_json::from_slice::<Value>(request)
        .ok()
        .and_then(|value| value.get("id").cloned())
        .unwrap_or(Value::Null);
    serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    }))
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn free_tcp_address() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        drop(listener);
        address
    }

    #[test]
    fn validates_json_rpc_version() {
        assert!(valid_json_rpc(
            br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#
        ));
        assert!(!valid_json_rpc(br#"{"jsonrpc":"1.0"}"#));
        assert!(!valid_json_rpc(b"not json"));
        assert!(json_rpc_has_id(
            br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#
        ));
        assert!(!json_rpc_has_id(
            br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
        ));
    }

    #[test]
    fn error_preserves_json_rpc_id() {
        let value: Value = serde_json::from_slice(&json_rpc_error(
            br#"{"jsonrpc":"2.0","id":"call-7","method":"tools/list"}"#,
            -32000,
            "offline",
        ))
        .unwrap();
        assert_eq!(value["id"], "call-7");
        assert_eq!(value["error"]["code"], -32000);
    }

    #[test]
    fn extracts_json_rpc_from_sse() {
        let body = b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":8,\"result\":{}}\n\n";
        let json = extract_sse_json(body, Some(&json!(8))).unwrap();
        let value: Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(value["id"], 8);
    }

    #[test]
    fn inbound_http_round_trips_through_udp_device() {
        let gateway_udp = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        gateway_udp
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let gateway_address = gateway_udp.local_addr().unwrap();
        let device = UdpSocket::bind("127.0.0.1:0").unwrap();
        device
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let peer = Arc::new(Mutex::new(Some(device.local_addr().unwrap())));
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let ids = Arc::new(AtomicU32::new(1));
        let http_address = free_tcp_address();
        let server_udp = Arc::clone(&gateway_udp);
        let server_pending = Arc::clone(&pending);
        let server_peer = Arc::clone(&peer);
        let server_ids = Arc::clone(&ids);
        let server_address = http_address.clone();
        thread::spawn(move || {
            serve_http(
                &server_address,
                server_udp,
                server_pending,
                server_peer,
                server_ids,
                String::from("0123456789abcdefghijklmn"),
            )
            .unwrap();
        });

        let router_udp = Arc::clone(&gateway_udp);
        let router_pending = Arc::clone(&pending);
        let router_thread = thread::spawn(move || {
            let mut reassembler = Reassembler::<4096>::default();
            let mut datagram = [0u8; MAX_DATAGRAM];
            loop {
                let (len, _) = router_udp.recv_from(&mut datagram).unwrap();
                if let Some(response) = reassembler.push(&datagram[..len]).unwrap() {
                    assert_eq!(response.kind, Kind::DeviceResponse);
                    if let Some(sender) =
                        router_pending.lock().unwrap().remove(&response.message_id)
                    {
                        sender.send(response.payload.to_vec()).unwrap();
                    }
                    break;
                }
            }
        });

        let device_thread = thread::spawn(move || {
            let mut reassembler = Reassembler::<4096>::default();
            let mut datagram = [0u8; MAX_DATAGRAM];
            loop {
                let (len, _) = device.recv_from(&mut datagram).unwrap();
                if let Some(request) = reassembler.push(&datagram[..len]).unwrap() {
                    assert_eq!(request.kind, Kind::HostRequest);
                    assert!(valid_json_rpc(request.payload));
                    let response = br#"{"jsonrpc":"2.0","id":7,"result":{}}"#;
                    send_message(
                        &device,
                        gateway_address,
                        Kind::DeviceResponse,
                        request.message_id,
                        response,
                    )
                    .unwrap();
                    break;
                }
            }
        });

        thread::sleep(Duration::from_millis(50));
        let mut response = ureq::post(&format!("http://{http_address}/mcp"))
            .header("Authorization", "Bearer 0123456789abcdefghijklmn")
            .header("Content-Type", "application/json")
            .send(br#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#)
            .unwrap();
        let body = response.body_mut().read_to_string().unwrap();
        assert_eq!(body, r#"{"jsonrpc":"2.0","id":7,"result":{}}"#);
        device_thread.join().unwrap();
        router_thread.join().unwrap();
    }

    #[test]
    fn outbound_upstream_negotiates_session_before_tool_call() {
        let address = free_tcp_address();
        let server = Server::http(&address).unwrap();
        let server_thread = thread::spawn(move || {
            for index in 0..3 {
                let mut request = server.recv().unwrap();
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let value: Value = serde_json::from_str(&body).unwrap();
                match index {
                    0 => {
                        assert_eq!(value["method"], "initialize");
                        let response = Response::from_string(
                            r#"{"jsonrpc":"2.0","id":"nrmcp-gateway-init","result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"mock","version":"1"}}}"#,
                        )
                        .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
                        .with_header(Header::from_bytes("Mcp-Session-Id", "session-7").unwrap());
                        request.respond(response).unwrap();
                    }
                    1 => {
                        assert_eq!(value["method"], "notifications/initialized");
                        assert_eq!(header_value(&request, "Mcp-Session-Id"), Some("session-7"));
                        request.respond(Response::empty(StatusCode(202))).unwrap();
                    }
                    _ => {
                        assert_eq!(value["method"], "tools/list");
                        assert_eq!(header_value(&request, "Mcp-Session-Id"), Some("session-7"));
                        request
                            .respond(
                                Response::from_string(
                                    r#"{"jsonrpc":"2.0","id":9,"result":{"tools":[]}}"#,
                                )
                                .with_header(
                                    Header::from_bytes("Content-Type", "application/json").unwrap(),
                                ),
                            )
                            .unwrap();
                    }
                }
            }
        });
        let mut upstream = Upstream::new(format!("http://{address}/mcp"), None);
        let response = upstream
            .call(br#"{"jsonrpc":"2.0","id":9,"method":"tools/list","params":{}}"#)
            .unwrap();
        let value: Value = serde_json::from_slice(&response).unwrap();
        assert_eq!(value["result"]["tools"], json!([]));
        server_thread.join().unwrap();
    }
}
