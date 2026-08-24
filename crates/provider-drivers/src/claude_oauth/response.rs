use std::{io, pin::Pin};

use async_compression::tokio::bufread::{
    BrotliDecoder, DeflateDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder,
};
use futures_util::{StreamExt, TryStreamExt};
use provider_core::{ProviderError, ProviderErrorKind, ProviderStream};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio_util::io::{ReaderStream, StreamReader};

type BoxReader = Pin<Box<dyn AsyncRead + Send>>;

pub(crate) async fn response_stream(
    response: reqwest::Response,
) -> Result<ProviderStream, ProviderError> {
    let encoding = response
        .headers()
        .get_all(reqwest::header::CONTENT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<_>>()
        .join(",");
    let stream = response
        .bytes_stream()
        .map_err(|error| io::Error::other(error.to_string()));
    let mut reader: BoxReader = Box::pin(StreamReader::new(stream));
    let encodings = encoding
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("identity"))
        .collect::<Vec<_>>();
    if encodings.is_empty() {
        reader = detect_magic(reader).await?;
    } else {
        for encoding in encodings.into_iter().rev() {
            reader = decode(reader, encoding).await?;
        }
    }
    Ok(Box::pin(
        ReaderStream::new(reader).map(|result| result.map_err(|_| upstream_stream_error())),
    ))
}

async fn decode(reader: BoxReader, encoding: &str) -> Result<BoxReader, ProviderError> {
    match encoding.to_ascii_lowercase().as_str() {
        "gzip" => Ok(Box::pin(GzipDecoder::new(BufReader::new(reader)))),
        "deflate" => decode_deflate(reader).await,
        "br" => Ok(Box::pin(BrotliDecoder::new(BufReader::new(reader)))),
        "zstd" => Ok(Box::pin(ZstdDecoder::new(BufReader::new(reader)))),
        _ => Err(ProviderError::new(
            ProviderErrorKind::Upstream,
            "Claude OAuth upstream returned unsupported content encoding",
        )),
    }
}

async fn decode_deflate(reader: BoxReader) -> Result<BoxReader, ProviderError> {
    let mut reader = BufReader::new(reader);
    let header = reader
        .fill_buf()
        .await
        .map_err(|_| upstream_stream_error())?;
    if is_zlib_header(header) {
        Ok(Box::pin(ZlibDecoder::new(reader)))
    } else {
        Ok(Box::pin(DeflateDecoder::new(reader)))
    }
}

async fn detect_magic(reader: BoxReader) -> Result<BoxReader, ProviderError> {
    let mut reader = BufReader::new(reader);
    let magic = reader
        .fill_buf()
        .await
        .map_err(|_| upstream_stream_error())?;
    if magic.starts_with(&[0x1f, 0x8b]) {
        Ok(Box::pin(GzipDecoder::new(reader)))
    } else if magic.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Ok(Box::pin(ZstdDecoder::new(reader)))
    } else {
        Ok(Box::pin(reader))
    }
}

fn is_zlib_header(header: &[u8]) -> bool {
    if header.len() < 2 {
        return false;
    }
    let cmf = header[0];
    let flg = header[1];
    cmf & 0x0f == 8 && cmf >> 4 <= 7 && (u16::from(cmf) << 8 | u16::from(flg)) % 31 == 0
}

fn upstream_stream_error() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Upstream,
        "Claude OAuth upstream stream failed",
    )
}

#[cfg(test)]
mod tests {
    use axum::{Router, body::Body, http::Response, routing::get};
    use provider_core::collect_bounded_body;

    use super::*;

    #[tokio::test]
    async fn decodes_gzip_and_rejects_unknown_encoding() {
        let app = Router::new()
            .route(
                "/gzip",
                get(|| async {
                    Response::builder()
                        .header("content-encoding", "gzip")
                        .body(Body::from(bytes::Bytes::from_static(
                            b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\xff\xcb\x48\xcd\xc9\xc9\x07\x00\x86\xa6\x10\x36\x05\x00\x00\x00",
                        )))
                        .expect("gzip response")
                }),
            )
            .route(
                "/unknown",
                get(|| async {
                    Response::builder()
                        .header("content-encoding", "snappy")
                        .body(Body::from("encoded"))
                        .expect("unknown response")
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("response listener");
        let address = listener.local_addr().expect("response address");
        let server = tokio::spawn(axum::serve(listener, app).into_future());
        let gzip = reqwest::get(format!("http://{address}/gzip"))
            .await
            .expect("gzip request");
        let body = collect_bounded_body(response_stream(gzip).await.expect("gzip stream"), 64)
            .await
            .expect("gzip body");
        assert_eq!(body, "hello");
        let unknown = reqwest::get(format!("http://{address}/unknown"))
            .await
            .expect("unknown request");
        assert!(response_stream(unknown).await.is_err());
        server.abort();
    }
}
