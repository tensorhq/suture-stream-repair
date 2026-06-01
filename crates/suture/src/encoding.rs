//! HTTP content-encoding handling for the proxy: a small `Encoding` model plus
//! streaming decode (and, in a later task, encode) so repair operates on plaintext.

use bytes::Bytes;
use futures_util::Stream;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

/// A boxed byte stream with io errors — the common currency of the codec layer.
pub type ByteStream = Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>>;

/// An HTTP content coding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Identity,
    Gzip,
    Brotli,
    Deflate,
    /// A coding we do not handle — caller must NOT attempt to decode/repair.
    Unknown,
}

impl Encoding {
    /// Parse a single `Content-Encoding` / `Accept-Encoding` token (case-insensitive).
    pub fn from_token(token: &str) -> Self {
        match token.trim().to_ascii_lowercase().as_str() {
            "" | "identity" => Encoding::Identity,
            "gzip" | "x-gzip" => Encoding::Gzip,
            "br" => Encoding::Brotli,
            "deflate" => Encoding::Deflate,
            _ => Encoding::Unknown,
        }
    }

    /// The header value to advertise for this coding, if any.
    pub fn header_value(self) -> Option<&'static str> {
        match self {
            Encoding::Gzip => Some("gzip"),
            Encoding::Brotli => Some("br"),
            Encoding::Deflate => Some("deflate"),
            Encoding::Identity | Encoding::Unknown => None,
        }
    }
}

/// Wrap a content-encoded byte stream so it yields the DECODED plaintext bytes.
/// `Identity`/`Unknown` pass through unchanged (the caller decides not to repair
/// an `Unknown`-coded body).
pub fn decode_stream<S>(s: S, enc: Encoding) -> ByteStream
where
    S: Stream<Item = io::Result<Bytes>> + Send + 'static,
{
    use async_compression::tokio::bufread;
    use tokio_util::io::{ReaderStream, StreamReader};
    match enc {
        Encoding::Identity | Encoding::Unknown => Box::pin(s),
        Encoding::Gzip => {
            Box::pin(ReaderStream::new(bufread::GzipDecoder::new(StreamReader::new(s))))
        }
        Encoding::Brotli => {
            Box::pin(ReaderStream::new(bufread::BrotliDecoder::new(StreamReader::new(s))))
        }
        Encoding::Deflate => {
            Box::pin(ReaderStream::new(bufread::ZlibDecoder::new(StreamReader::new(s))))
        }
    }
}

/// An in-memory `AsyncWrite` sink whose buffer we drain after each flush. The
/// buffer is shared (Arc) so the encode loop can take its contents independent of
/// the encoder's concrete type.
struct SharedSink(Arc<Mutex<Vec<u8>>>);

impl tokio::io::AsyncWrite for SharedSink {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Re-encode a plaintext byte stream with `enc`, flushing after every input chunk so
/// output is emitted promptly (never buffered until end of stream). `Identity`/`Unknown`
/// pass through unchanged. Uses fast compression levels (high quality costs ms per call
/// and buys almost nothing on tiny flushed payloads).
pub fn encode_stream<S>(s: S, enc: Encoding) -> ByteStream
where
    S: futures_util::Stream<Item = io::Result<Bytes>> + Send + 'static,
{
    use async_compression::tokio::write::{BrotliEncoder, GzipEncoder, ZlibEncoder};
    use async_compression::Level;
    match enc {
        Encoding::Identity | Encoding::Unknown => Box::pin(s),
        Encoding::Gzip => {
            let buf = Arc::new(Mutex::new(Vec::new()));
            encode_with(GzipEncoder::with_quality(SharedSink(buf.clone()), Level::Default), s, buf)
        }
        Encoding::Brotli => {
            let buf = Arc::new(Mutex::new(Vec::new()));
            encode_with(BrotliEncoder::with_quality(SharedSink(buf.clone()), Level::Fastest), s, buf)
        }
        Encoding::Deflate => {
            let buf = Arc::new(Mutex::new(Vec::new()));
            encode_with(ZlibEncoder::with_quality(SharedSink(buf.clone()), Level::Default), s, buf)
        }
    }
}

fn encode_with<E, S>(encoder: E, input: S, buf: Arc<Mutex<Vec<u8>>>) -> ByteStream
where
    E: tokio::io::AsyncWrite + Unpin + Send + 'static,
    S: futures_util::Stream<Item = io::Result<Bytes>> + Send + 'static,
{
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    let input = Box::pin(input);
    Box::pin(futures_util::stream::unfold(
        (input, encoder, buf, false),
        |(mut input, mut encoder, buf, done)| async move {
            if done {
                return None;
            }
            match input.next().await {
                Some(Ok(chunk)) => {
                    if let Err(e) = encoder.write_all(&chunk).await {
                        return Some((Err(e), (input, encoder, buf, true)));
                    }
                    if let Err(e) = encoder.flush().await {
                        return Some((Err(e), (input, encoder, buf, true)));
                    }
                    let out = std::mem::take(&mut *buf.lock().unwrap());
                    Some((Ok(Bytes::from(out)), (input, encoder, buf, false)))
                }
                Some(Err(e)) => Some((Err(e), (input, encoder, buf, true))),
                None => {
                    if let Err(e) = encoder.shutdown().await {
                        return Some((Err(e), (input, encoder, buf, true)));
                    }
                    let out = std::mem::take(&mut *buf.lock().unwrap());
                    if out.is_empty() {
                        None
                    } else {
                        Some((Ok(Bytes::from(out)), (input, encoder, buf, true)))
                    }
                }
            }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use futures::StreamExt;
    use std::io::Write;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut e = GzEncoder::new(Vec::new(), Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    async fn collect(s: impl futures::Stream<Item = std::io::Result<Bytes>>) -> Vec<u8> {
        futures::pin_mut!(s);
        let mut out = Vec::new();
        while let Some(item) = s.next().await {
            out.extend_from_slice(&item.unwrap());
        }
        out
    }

    #[test]
    fn parses_encoding_tokens() {
        assert_eq!(Encoding::from_token(""), Encoding::Identity);
        assert_eq!(Encoding::from_token("identity"), Encoding::Identity);
        assert_eq!(Encoding::from_token("gzip"), Encoding::Gzip);
        assert_eq!(Encoding::from_token("GZIP"), Encoding::Gzip);
        assert_eq!(Encoding::from_token("br"), Encoding::Brotli);
        assert_eq!(Encoding::from_token("deflate"), Encoding::Deflate);
        assert_eq!(Encoding::from_token("weird"), Encoding::Unknown);
    }

    #[tokio::test]
    async fn decodes_gzip_stream() {
        let plain = b"data: {\"a\":1}\n\ndata: [DONE]\n\n";
        let comp = gzip(plain);
        // feed the compressed bytes in two chunks to exercise streaming
        let mid = comp.len() / 2;
        let chunks = vec![
            Ok(Bytes::copy_from_slice(&comp[..mid])),
            Ok(Bytes::copy_from_slice(&comp[mid..])),
        ];
        let input = futures::stream::iter(chunks);
        let decoded = collect(decode_stream(Box::pin(input), Encoding::Gzip)).await;
        assert_eq!(decoded, plain);
    }

    #[tokio::test]
    async fn identity_and_unknown_passthrough() {
        let bytes = b"raw bytes not compressed";
        for enc in [Encoding::Identity, Encoding::Unknown] {
            let input = futures::stream::iter(vec![Ok(Bytes::copy_from_slice(bytes))]);
            let out = collect(decode_stream(Box::pin(input), enc)).await;
            assert_eq!(out, bytes);
        }
    }

    #[tokio::test]
    async fn gzip_round_trips() {
        let plain = b"data: {\"city\":\"Paris\"}\n\ndata: [DONE]\n\n";
        let input = futures::stream::iter(vec![Ok(Bytes::copy_from_slice(plain))]);
        let encoded = collect(encode_stream(Box::pin(input), Encoding::Gzip)).await;
        // decode it back with the existing decoder
        let dec_in = futures::stream::iter(vec![Ok(Bytes::from(encoded))]);
        let decoded = collect(decode_stream(Box::pin(dec_in), Encoding::Gzip)).await;
        assert_eq!(decoded, plain);
    }

    #[tokio::test]
    async fn brotli_round_trips() {
        let plain = b"hello brotli streaming world";
        let input = futures::stream::iter(vec![Ok(Bytes::copy_from_slice(plain))]);
        let encoded = collect(encode_stream(Box::pin(input), Encoding::Brotli)).await;
        let dec_in = futures::stream::iter(vec![Ok(Bytes::from(encoded))]);
        let decoded = collect(decode_stream(Box::pin(dec_in), Encoding::Brotli)).await;
        assert_eq!(decoded, plain);
    }

    #[tokio::test]
    async fn identity_encode_passthrough() {
        let plain = b"unchanged";
        let input = futures::stream::iter(vec![Ok(Bytes::copy_from_slice(plain))]);
        let out = collect(encode_stream(Box::pin(input), Encoding::Identity)).await;
        assert_eq!(out, plain);
    }

    /// PROMPTNESS: per-chunk flush must emit decodable output per input chunk, not
    /// buffer everything until stream end. We feed N distinct chunks one at a time and
    /// assert the encoder yields at least N NON-EMPTY output chunks (one flushed block
    /// per input). A buffer-until-end encoder would yield ~1 non-empty chunk (only at
    /// shutdown), failing this.
    #[tokio::test]
    async fn encoder_flushes_per_chunk() {
        let parts: Vec<&[u8]> = vec![b"first ", b"second ", b"third"];
        let input = futures::stream::iter(
            parts.iter().map(|p| Ok(Bytes::copy_from_slice(p))).collect::<Vec<_>>(),
        );
        let encoded = encode_stream(Box::pin(input), Encoding::Gzip);
        futures::pin_mut!(encoded);

        let mut nonempty = 0usize;
        let mut all = Vec::new();
        while let Some(item) = encoded.next().await {
            let b = item.unwrap();
            if !b.is_empty() {
                nonempty += 1;
            }
            all.extend_from_slice(&b);
        }
        assert!(
            nonempty >= parts.len(),
            "expected >= {} non-empty flushed output chunks (per-chunk flush), got {}",
            parts.len(),
            nonempty
        );
        // and it still round-trips
        let dec_in = futures::stream::iter(vec![Ok(Bytes::from(all))]);
        let decoded = collect(decode_stream(Box::pin(dec_in), Encoding::Gzip)).await;
        assert_eq!(decoded, b"first second third");
    }
}
