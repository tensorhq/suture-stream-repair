use bytes::Bytes;
use criterion::{criterion_group, criterion_main, Criterion};
use futures_util::StreamExt;
use suture::encoding::{decode_stream, encode_stream, Encoding};

/// A representative ~120-byte SSE tool-call delta event.
const EVENT: &str = "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\\\"Paris\\\"}\"}}]}}]}\n\n";

fn one_event_stream() -> impl futures_util::Stream<Item = std::io::Result<Bytes>> + Send + 'static {
    futures_util::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(EVENT))])
}

async fn drain(mut s: suture::encoding::ByteStream) -> usize {
    let mut n = 0;
    while let Some(item) = s.next().await {
        n += item.unwrap().len();
    }
    n
}

async fn encode_one(enc: Encoding) -> usize {
    drain(encode_stream(one_event_stream(), enc)).await
}

async fn roundtrip_one(enc: Encoding) -> usize {
    let encoded = encode_stream(one_event_stream(), enc);
    drain(decode_stream(encoded, enc)).await
}

fn bench(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    c.bench_function("encode_gzip_chunk", |b| {
        b.to_async(&rt).iter(|| encode_one(Encoding::Gzip))
    });
    c.bench_function("encode_brotli_fast_chunk", |b| {
        b.to_async(&rt).iter(|| encode_one(Encoding::Brotli))
    });
    c.bench_function("roundtrip_gzip_chunk", |b| {
        b.to_async(&rt).iter(|| roundtrip_one(Encoding::Gzip))
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
