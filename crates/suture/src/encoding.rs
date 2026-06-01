//! HTTP content-encoding handling for the proxy: a small `Encoding` model plus
//! streaming decode (and, in a later task, encode) so repair operates on plaintext.

use bytes::Bytes;
use futures_util::Stream;
use std::io;
use std::pin::Pin;

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
}
