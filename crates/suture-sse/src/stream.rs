use crate::extractor::DeltaExtractor;
use crate::repairer::SseRepairer;
use bytes::Bytes;
use futures_core::Stream;
use futures_util::stream::StreamExt;

/// Wrap an upstream byte stream, forwarding each chunk verbatim and appending a
/// synthesized repair tail after the upstream ends cleanly. On an upstream error,
/// the error is forwarded and the stream stops (no repair tail).
pub fn repair_stream<S, E>(
    upstream: S,
    extractor: Box<dyn DeltaExtractor>,
) -> impl Stream<Item = Result<Bytes, E>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Send + 'static,
{
    let repairer = SseRepairer::new(extractor);
    futures_util::stream::unfold(
        (upstream.boxed(), repairer, false),
        |(mut up, mut repairer, finished)| async move {
            if finished {
                return None;
            }
            match up.next().await {
                Some(Ok(chunk)) => {
                    let forwarded = repairer.push(&chunk);
                    Some((Ok(forwarded), (up, repairer, false)))
                }
                Some(Err(e)) => Some((Err(e), (up, repairer, true))),
                None => {
                    let tail = repairer.finish();
                    if tail.is_empty() {
                        None
                    } else {
                        Some((Ok(tail), (up, repairer, true)))
                    }
                }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::OpenAi;
    use bytes::Bytes;
    use futures::stream;
    use futures::StreamExt;
    use serde_json::Value;

    #[tokio::test]
    async fn stream_forwards_then_appends_repair() {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"x\\\":1\"}}]}}]}\n\n")),
        ];
        let upstream = stream::iter(chunks);
        let repaired = repair_stream(upstream, Box::new(OpenAi));
        futures::pin_mut!(repaired);

        let mut all = Vec::new();
        while let Some(item) = repaired.next().await {
            all.extend_from_slice(&item.unwrap());
        }

        let mut parser = crate::SseParser::new();
        let mut args = String::new();
        for data in parser.push(&all) {
            if let Ok(v) = serde_json::from_slice::<Value>(&data) {
                if let Some(a) =
                    v["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str()
                {
                    args.push_str(a);
                }
            }
        }
        assert_eq!(args, r#"{"x":1}"#);
        serde_json::from_str::<Value>(&args).unwrap();
    }
}
