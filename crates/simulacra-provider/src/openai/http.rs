//! Minimal HTTP abstraction so the OpenAI client can be driven by a fake in
//! tests, plus the reqwest-backed implementation used in production.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use simulacra_types::ProviderError;

use crate::transport::{TransportStage, transport_error};

pub(super) trait HttpClient: Send + Sync {
    fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, ProviderError>> + Send + '_>>;

    fn post_stream<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        body: &'a [u8],
        sink: &'a mut dyn HttpStreamSink,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, ProviderError>> + Send + 'a>> {
        Box::pin(async move {
            let response = self.post(url, headers, body).await?;
            sink.begin(response.status, &response.headers)?;
            sink.chunk(&response.body)?;
            Ok(response)
        })
    }
}

pub(super) trait HttpStreamSink: Send {
    fn begin(
        &mut self,
        _status: u16,
        _headers: &HashMap<String, String>,
    ) -> Result<(), ProviderError> {
        Ok(())
    }

    fn chunk(&mut self, _chunk: &[u8]) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// Raw HTTP response.
pub(super) struct HttpResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

pub(super) struct ReqwestClient {
    pub(super) client: reqwest::Client,
}

impl ReqwestClient {
    pub(super) fn new() -> Self {
        Self {
            client: crate::transport::provider_http_client(crate::transport::READ_IDLE_TIMEOUT),
        }
    }
}

impl HttpClient for ReqwestClient {
    fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, ProviderError>> + Send + '_>> {
        let url = url.to_owned();
        let headers = headers.to_vec();
        let body = body.to_vec();
        Box::pin(async move {
            let mut builder = self.client.post(&url);
            for (key, value) in &headers {
                builder = builder.header(key.as_str(), value.as_str());
            }
            let resp = builder
                .body(body)
                .send()
                .await
                .map_err(|e| transport_error(TransportStage::SendRequest, &e))?;

            let status = resp.status().as_u16();
            let resp_headers: HashMap<String, String> = resp
                .headers()
                .iter()
                .filter_map(|(k, v)| {
                    v.to_str()
                        .ok()
                        .map(|val| (k.as_str().to_lowercase(), val.to_owned()))
                })
                .collect();
            let resp_body = resp
                .bytes()
                .await
                .map_err(crate::transport::read_error("response body"))?;

            Ok(HttpResponse {
                status,
                headers: resp_headers,
                body: resp_body.to_vec(),
            })
        })
    }

    fn post_stream<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        body: &'a [u8],
        sink: &'a mut dyn HttpStreamSink,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, ProviderError>> + Send + 'a>> {
        let url = url.to_owned();
        let headers = headers.to_vec();
        let body = body.to_vec();
        Box::pin(async move {
            let mut builder = self.client.post(&url);
            for (key, value) in &headers {
                builder = builder.header(key.as_str(), value.as_str());
            }
            let mut resp = builder
                .body(body)
                .send()
                .await
                .map_err(|e| transport_error(TransportStage::SendRequest, &e))?;

            let status = resp.status().as_u16();
            let resp_headers: HashMap<String, String> = resp
                .headers()
                .iter()
                .filter_map(|(k, v)| {
                    v.to_str()
                        .ok()
                        .map(|val| (k.as_str().to_lowercase(), val.to_owned()))
                })
                .collect();
            sink.begin(status, &resp_headers)?;

            let mut resp_body = Vec::new();
            while let Some(chunk) = resp
                .chunk()
                .await
                .map_err(crate::transport::read_error("response chunk"))?
            {
                resp_body.extend_from_slice(&chunk);
                sink.chunk(&chunk)?;
            }

            Ok(HttpResponse {
                status,
                headers: resp_headers,
                body: resp_body,
            })
        })
    }
}
