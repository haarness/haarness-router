use std::{
    convert::Infallible,
    net::SocketAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_stream::stream;
use axum::{
    Router,
    body::{Body, Bytes},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use clap::Parser;

#[derive(Parser)]
#[command(about = "Deterministic local OpenAI-compatible SSE provider")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:9090")]
    bind: SocketAddr,
    #[arg(long, default_value_t = 0)]
    hold_ms: u64,
    #[arg(long, default_value_t = 15_000)]
    heartbeat_ms: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let hold = args.hold_ms;
    let heartbeat = args.heartbeat_ms;
    let application = Router::new().route(
        "/chat/completions",
        post(move || async move { mock_completion(hold, heartbeat) }),
    );
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    println!("mock provider listening on {}", listener.local_addr()?);
    axum::serve(listener, application).await?;
    Ok(())
}

fn mock_completion(hold_ms: u64, heartbeat_ms: u64) -> Response {
    let output = stream! {
        let emitted = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let first = format!("data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"__haarness_emitted_ns:{emitted}\"}},\"finish_reason\":null}}]}}\n\n");
        yield Ok::<Bytes, Infallible>(Bytes::from(first));
        let mut remaining = hold_ms;
        while remaining > 0 {
            let interval = remaining.min(heartbeat_ms.max(1));
            tokio::time::sleep(Duration::from_millis(interval)).await;
            remaining -= interval;
            if remaining > 0 {
                // Exercise long-lived streams without generating response
                // payload. The router consumes this standard SSE heartbeat.
                yield Ok::<Bytes, Infallible>(Bytes::from_static(b": keep-alive\n\n"));
            }
        }
        yield Ok::<Bytes, Infallible>(Bytes::from_static(b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"));
        yield Ok::<Bytes, Infallible>(Bytes::from_static(b"data: [DONE]\n\n"));
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(output),
    )
        .into_response()
}
