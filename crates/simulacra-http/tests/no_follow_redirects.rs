//! No-follow request mode: a request carrying `max_redirects: Some(0)` must
//! surface a 3xx response (with its `Location`) instead of erroring, and the
//! default client must still follow redirects as before. These run against a
//! localhost TCP fixture — the concrete ureq transport is the behavior under
//! test; localhost servers keep it offline and deterministic.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

use simulacra_http::{HttpClient, HttpRequest, UreqHttpClient};

fn localhost_server() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").expect("bind to localhost")
}

fn read_request_head(stream: &std::net::TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
            break;
        }
    }
}

fn get(url: String, max_redirects: Option<u32>) -> HttpRequest {
    HttpRequest {
        url,
        method: "GET".into(),
        headers: vec![],
        body: None,
        timeout_ms: None,
        max_redirects,
    }
}

#[test]
fn no_follow_request_surfaces_the_redirect_response() {
    let listener = localhost_server();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        read_request_head(&stream);
        stream
            .write_all(b"HTTP/1.1 302 Found\r\nLocation: /elsewhere\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
    });

    let response = UreqHttpClient::default()
        .execute(&get(format!("http://{addr}/start"), Some(0)))
        .expect("a no-follow request must surface the 3xx, not error");

    assert_eq!(response.status, 302);
    assert!(
        response
            .headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("location") && value == "/elsewhere"),
        "the Location header must be readable for per-hop authorization: {:?}",
        response.headers
    );
}

#[test]
fn default_client_still_follows_redirects() {
    // Cross-origin redirect so the followed hop opens its own connection —
    // a same-origin Location could be served on the reused first connection.
    let first_listener = localhost_server();
    let first_addr = first_listener.local_addr().unwrap();
    let final_listener = localhost_server();
    let final_addr = final_listener.local_addr().unwrap();

    std::thread::spawn(move || {
        let Ok((mut stream, _)) = first_listener.accept() else {
            return;
        };
        read_request_head(&stream);
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{final_addr}/final\r\nContent-Length: 0\r\n\r\n"
        );
        stream.write_all(redirect.as_bytes()).unwrap();
    });
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = final_listener.accept() else {
            return;
        };
        read_request_head(&stream);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nbody")
            .unwrap();
    });

    let response = UreqHttpClient::default()
        .execute(&get(format!("http://{first_addr}/start"), None))
        .expect("the default client keeps following redirects");

    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"body");
}
