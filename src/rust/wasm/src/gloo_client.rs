use async_trait::async_trait;
use bytes::{BufMut, Bytes, BytesMut};
use http_range_client::{AsyncBufferedHttpRangeClient, AsyncHttpRangeClient, HttpError, Result};
use std::cmp::{max, min};

#[cfg(target_arch = "wasm32")]
use gloo_net::http::RequestBuilder as GlooRequest;

pub struct WasmHttpClient {}

pub struct WasmBufferedHttpRangeClient64 {
    url: String,
    buffer: BytesMut,
    head: u64,
    min_req_size: usize,
}

#[cfg(target_arch = "wasm32")]
impl WasmHttpClient {
    pub fn new(url: &str) -> AsyncBufferedHttpRangeClient<WasmHttpClient> {
        AsyncBufferedHttpRangeClient::with(WasmHttpClient {}, url)
    }
}

#[cfg(target_arch = "wasm32")]
impl WasmBufferedHttpRangeClient64 {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            buffer: BytesMut::new(),
            head: 0,
            min_req_size: 1024,
        }
    }

    pub fn set_min_req_size(&mut self, size: usize) {
        self.min_req_size = size;
    }

    pub fn min_req_size(&mut self, size: usize) -> &mut Self {
        self.set_min_req_size(size);
        self
    }

    pub async fn get_range(&mut self, begin: u64, length: usize) -> Result<Bytes> {
        let length_u64 = length as u64;
        let requested_end = begin
            .checked_add(length_u64)
            .ok_or_else(|| HttpError::HttpError("range end overflow".to_string()))?;
        let tail = self.tail();

        if begin < self.head || requested_end > tail {
            if begin > self.head && begin < tail {
                let drop_len = usize::try_from(begin - self.head)
                    .map_err(|_| HttpError::HttpError("buffer trim length overflow".to_string()))?;
                let _ = self.buffer.split_to(drop_len);
                self.head = begin;
            } else {
                self.buffer.clear();
                self.head = begin;
            }

            let range_begin = max(begin, self.tail());
            let missing_len = requested_end
                .checked_sub(range_begin)
                .ok_or_else(|| HttpError::HttpError("range length underflow".to_string()))?;
            let fetch_len_u64 = max(missing_len, self.min_req_size as u64);
            usize::try_from(fetch_len_u64)
                .map_err(|_| HttpError::HttpError("range request too large".to_string()))?;
            let range_end = range_begin
                .checked_add(fetch_len_u64)
                .and_then(|end| end.checked_sub(1))
                .ok_or_else(|| HttpError::HttpError("range end overflow".to_string()))?;
            let range = format!("bytes={range_begin}-{range_end}");
            let bytes = WasmHttpClient {}.get_range(&self.url, &range).await?;
            self.buffer.put(bytes);
        }

        let available = min(requested_end, self.tail())
            .checked_sub(begin)
            .ok_or_else(|| HttpError::HttpError("buffer slice underflow".to_string()))?;
        let available = usize::try_from(available)
            .map_err(|_| HttpError::HttpError("buffer slice length overflow".to_string()))?;
        if available < length {
            return Err(HttpError::HttpError(format!(
                "short range response: requested {length} bytes, got {available}"
            )));
        }

        let lower = usize::try_from(begin - self.head)
            .map_err(|_| HttpError::HttpError("buffer slice offset overflow".to_string()))?;
        Ok(Bytes::copy_from_slice(&self.buffer[lower..lower + length]))
    }

    fn tail(&self) -> u64 {
        self.head + self.buffer.len() as u64
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl WasmHttpClient {
    pub fn new(url: &str) -> AsyncBufferedHttpRangeClient<WasmHttpClient> {
        // This is a mock implementation for non-wasm targets
        // It will never be called in production, but enables compilation
        AsyncBufferedHttpRangeClient::with(WasmHttpClient {}, url)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl WasmBufferedHttpRangeClient64 {
    pub fn new(_url: &str) -> Self {
        Self {
            url: String::new(),
            buffer: BytesMut::new(),
            head: 0,
            min_req_size: 1024,
        }
    }

    pub fn set_min_req_size(&mut self, size: usize) {
        self.min_req_size = size;
    }

    pub fn min_req_size(&mut self, size: usize) -> &mut Self {
        self.set_min_req_size(size);
        self
    }

    pub async fn get_range(&mut self, _begin: u64, _length: usize) -> Result<Bytes> {
        Err(HttpError::HttpError(
            "Not implemented for non-wasm targets".to_string(),
        ))
    }
}

#[cfg(target_arch = "wasm32")]
#[async_trait(?Send)]
impl AsyncHttpRangeClient for WasmHttpClient {
    async fn get_range(&self, url: &str, range: &str) -> Result<Bytes> {
        let response = GlooRequest::new(url)
            .header("Range", range)
            .send()
            .await
            .map_err(|e| HttpError::HttpError(e.to_string()))?;

        if !response.ok() {
            return Err(HttpError::HttpStatus(response.status()));
        }
        response
            .binary()
            .await
            .map(Bytes::from)
            .map_err(|e| HttpError::HttpError(e.to_string()))
    }

    async fn head_response_header(&self, url: &str, header: &str) -> Result<Option<String>> {
        let response = GlooRequest::new(url)
            .send()
            .await
            .map_err(|e| HttpError::HttpError(format!("failed to send request: {e}")))?;
        if let Some(val) = response.headers().get(header) {
            Ok(Some(val.to_string()))
        } else {
            Ok(None)
        }
    }
}

// Mock implementation for non-wasm targets
#[cfg(not(target_arch = "wasm32"))]
#[async_trait]
impl AsyncHttpRangeClient for WasmHttpClient {
    async fn get_range(&self, _url: &str, _range: &str) -> Result<Bytes> {
        // Mock implementation that will never be called
        Err(HttpError::HttpError(
            "Not implemented for non-wasm targets".to_string(),
        ))
    }

    async fn head_response_header(&self, _url: &str, _header: &str) -> Result<Option<String>> {
        // Mock implementation that will never be called
        Err(HttpError::HttpError(
            "Not implemented for non-wasm targets".to_string(),
        ))
    }
}
