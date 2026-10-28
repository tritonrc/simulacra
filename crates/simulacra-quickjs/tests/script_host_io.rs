//! A host's futures may do socket I/O, whichever eval path drives them.

use std::sync::Arc;

use simulacra_quickjs::{BoxFuture, JsHost, JsRuntime, JsRuntimeOptions};
use simulacra_vfs::MemoryFs;

struct Loopback;

impl JsHost for Loopback {
    fn names(&self) -> Vec<String> {
        vec!["roundtrip".into()]
    }

    fn call(&self, _: String, _: String) -> BoxFuture<'static, Result<String, String>> {
        Box::pin(async {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .map_err(|e| e.to_string())?;
            let addr = listener.local_addr().map_err(|e| e.to_string())?;
            let (mut client, (mut server, _)) =
                tokio::try_join!(tokio::net::TcpStream::connect(addr), listener.accept())
                    .map_err(|e| e.to_string())?;
            client
                .write_all(b"\"pong\"")
                .await
                .map_err(|e| e.to_string())?;
            drop(client);
            let mut reply = String::new();
            server
                .read_to_string(&mut reply)
                .await
                .map_err(|e| e.to_string())?;
            Ok(reply)
        })
    }
}

#[test]
fn a_host_future_doing_socket_io_resolves_under_the_sync_eval() {
    let rt = JsRuntime::new(Arc::new(MemoryFs::new()))
        .unwrap()
        .with_runtime_options(JsRuntimeOptions {
            host: Some(Arc::new(Loopback)),
            ..Default::default()
        });
    let out = rt.eval("tools.roundtrip({})").unwrap();
    assert_eq!(out.result.as_deref(), Some("pong"));
}
