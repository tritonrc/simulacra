#[allow(dead_code)]
struct PassiveTcpListenerProbe {
    addr: String,
    connection_count: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl PassiveTcpListenerProbe {
    #[allow(dead_code)]
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn connection_count(&self) -> usize {
        self.connection_count.load(Ordering::SeqCst)
    }
}

impl Drop for PassiveTcpListenerProbe {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn spawn_passive_tcp_listener_probe() -> PassiveTcpListenerProbe {
    let listener = TcpListener::bind("127.0.0.1:0").expect("probe listener should bind");
    listener
        .set_nonblocking(true)
        .expect("probe listener should become nonblocking");

    let addr = listener
        .local_addr()
        .expect("probe listener should have a local address")
        .to_string();
    let connection_count = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let connection_count_for_thread = Arc::clone(&connection_count);
    let stop_for_thread = Arc::clone(&stop);

    let handle = thread::spawn(move || {
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((_stream, _peer)) => {
                    connection_count_for_thread.fetch_add(1, Ordering::SeqCst);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    PassiveTcpListenerProbe {
        addr,
        connection_count,
        stop,
        handle: Some(handle),
    }
}

struct RecordingHttpServer {
    addr: String,
    request_count: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl RecordingHttpServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn request_count(&self) -> usize {
        self.request_count.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .expect("request log mutex should not be poisoned")
            .clone()
    }
}

impl Drop for RecordingHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn spawn_recording_http_server(response_body: &str) -> RecordingHttpServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test HTTP server should bind");
    listener
        .set_nonblocking(true)
        .expect("test HTTP server should become nonblocking");

    let addr = listener
        .local_addr()
        .expect("test HTTP server should have a local address")
        .to_string();
    let request_count = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));

    let response_body = response_body.to_string();
    let request_count_for_thread = Arc::clone(&request_count);
    let requests_for_thread = Arc::clone(&requests);
    let stop_for_thread = Arc::clone(&stop);

    let handle = thread::spawn(move || {
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _peer)) => {
                    request_count_for_thread.fetch_add(1, Ordering::SeqCst);
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));

                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response_body.len(),
                        response_body
                    );

                    if let Some(request) = read_http_request(&mut stream) {
                        requests_for_thread
                            .lock()
                            .expect("request log mutex should not be poisoned")
                            .push(request);
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    RecordingHttpServer {
        addr,
        request_count,
        requests,
        stop,
        handle: Some(handle),
    }
}

struct JsonRpcTestServer {
    addr: String,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl JsonRpcTestServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn server_name(&self) -> &str {
        self.addr
            .split(':')
            .next()
            .expect("test server address should include a host")
    }
}

impl Drop for JsonRpcTestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn json_http_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

fn spawn_json_rpc_test_server(tools_list_body: &str, tool_call_body: &str) -> JsonRpcTestServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("JSON-RPC test server should bind");
    listener
        .set_nonblocking(true)
        .expect("JSON-RPC test server should become nonblocking");

    let addr = listener
        .local_addr()
        .expect("JSON-RPC test server should have a local address")
        .to_string();
    let request_count = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));

    let request_count_for_thread = Arc::clone(&request_count);
    let requests_for_thread = Arc::clone(&requests);
    let stop_for_thread = Arc::clone(&stop);
    let tools_list_body = tools_list_body.to_string();
    let tool_call_body = tool_call_body.to_string();

    let handle = thread::spawn(move || {
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _peer)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));

                    if let Some(request) = read_http_request(&mut stream) {
                        request_count_for_thread.fetch_add(1, Ordering::SeqCst);
                        requests_for_thread
                            .lock()
                            .expect("request log mutex should not be poisoned")
                            .push(request.clone());

                        let body = if request.contains("\"method\":\"initialize\"") {
                            json!({
                                "jsonrpc": "2.0",
                                "result": {
                                    "protocolVersion": "2024-11-05",
                                    "serverInfo": { "name": "fake-mcp", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string()
                        } else if request.contains("\"method\":\"tools/list\"") {
                            tools_list_body.clone()
                        } else if request.contains("\"method\":\"tools/call\"") {
                            tool_call_body.clone()
                        } else {
                            json!({ "jsonrpc": "2.0", "result": {} }).to_string()
                        };

                        let response = json_http_response(&body);
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    JsonRpcTestServer {
        addr,
        stop,
        handle: Some(handle),
    }
}
