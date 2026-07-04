use super::*;

pub struct TestHttpServer {
    pub addr: String,
    pub request_count: Arc<AtomicUsize>,
    pub stop: Arc<AtomicBool>,
    pub handle: Option<JoinHandle<()>>,
}

impl TestHttpServer {
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    pub fn request_count(&self) -> usize {
        self.request_count.load(Ordering::SeqCst)
    }
}

impl Drop for TestHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        404 => "Not Found",
        _ => "OK",
    }
}

pub fn spawn_http_server(status: u16, headers: &[(&str, &str)], body: &[u8]) -> TestHttpServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
    listener
        .set_nonblocking(true)
        .expect("test server should become nonblocking");
    let addr = listener
        .local_addr()
        .expect("test server should expose an address")
        .to_string();
    let request_count = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let request_count_for_thread = Arc::clone(&request_count);
    let stop_for_thread = Arc::clone(&stop);
    let header_lines: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    let response_body = body.to_vec();

    let handle = thread::spawn(move || {
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _peer)) => {
                    request_count_for_thread.fetch_add(1, Ordering::SeqCst);

                    let mut buffer = [0_u8; 4096];
                    let _ = stream.read(&mut buffer);

                    let mut response = format!(
                        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                        status,
                        reason_phrase(status),
                        response_body.len()
                    );
                    for (name, value) in &header_lines {
                        response.push_str(name);
                        response.push_str(": ");
                        response.push_str(value);
                        response.push_str("\r\n");
                    }
                    response.push_str("\r\n");

                    stream
                        .write_all(response.as_bytes())
                        .expect("test server should write response headers");
                    stream
                        .write_all(&response_body)
                        .expect("test server should write response body");
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });

    TestHttpServer {
        addr,
        request_count,
        stop,
        handle: Some(handle),
    }
}
