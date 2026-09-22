use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use futures::StreamExt;
use tokio::sync::Semaphore;

#[derive(Parser)]
#[command(about = "Bounded local SSE concurrency benchmark")]
struct Args {
    #[arg(long, default_value = "http://127.0.0.1:8080/v1/chat/completions")]
    endpoint: String,
    #[arg(long, env = "HAARNESS_ROUTER_KEY")]
    router_key: String,
    #[arg(long, default_value = "mock/test")]
    model: String,
    #[arg(long, default_value_t = 10_000)]
    concurrency: usize,
    #[arg(long, default_value_t = 10_000)]
    requests: usize,
    #[arg(long, default_value_t = 10.0)]
    p99_target_ms: f64,
    #[arg(long, default_value_t = 0)]
    hold_seconds: u64,
    #[arg(long, default_value_t = 0)]
    ramp_seconds: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if !args.endpoint.starts_with("http://127.0.0.1:")
        && !args.endpoint.starts_with("http://localhost:")
        && !args.endpoint.starts_with("http://[::1]:")
    {
        bail!("load-test only accepts loopback router endpoints");
    }
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(args.concurrency)
        .tcp_nodelay(true)
        .build()?;
    let semaphore = Arc::new(Semaphore::new(args.concurrency));
    let endpoint: Arc<str> = Arc::from(args.endpoint);
    let key: Arc<str> = Arc::from(args.router_key);
    let model: Arc<str> = Arc::from(args.model);
    let launch_started = Instant::now();
    let mut tasks = Vec::with_capacity(args.requests);
    for request_index in 0..args.requests {
        if args.ramp_seconds > 0 {
            let nanos = u128::from(args.ramp_seconds)
                .saturating_mul(1_000_000_000)
                .saturating_mul(request_index as u128)
                / args.requests.max(1) as u128;
            let target = Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX));
            tokio::time::sleep_until((launch_started + target).into()).await;
        }
        let permit = Arc::clone(&semaphore).acquire_owned().await?;
        let client = client.clone();
        let endpoint = Arc::clone(&endpoint);
        let key = Arc::clone(&key);
        let model = Arc::clone(&model);
        let hold_seconds = args.hold_seconds;
        tasks.push(tokio::spawn(async move {
            let _permit = permit;
            let started = Instant::now();
            let response = client
                .post(endpoint.as_ref())
                .bearer_auth(key.as_ref())
                .json(&serde_json::json!({
                    "model": model.as_ref(),
                    "messages": [{"role":"user","content":"ping"}],
                    "stream": true,
                    "max_completion_tokens": 8
                }))
                .send()
                .await
                .context("request failed")?
                .error_for_status()
                .context("router returned an error")?;
            let mut body = response.bytes_stream();
            let first = tokio::time::timeout(Duration::from_secs(30), body.next())
                .await
                .context("first chunk timed out")?
                .ok_or_else(|| anyhow::anyhow!("stream ended before first chunk"))??;
            if first.is_empty() {
                bail!("received an empty first chunk");
            }
            let first_chunk = emission_latency(&first).unwrap_or_else(|| started.elapsed());
            if hold_seconds > 0 {
                tokio::time::sleep(Duration::from_secs(hold_seconds)).await;
            }
            Ok::<Duration, anyhow::Error>(first_chunk)
        }));
    }
    let mut samples = Vec::with_capacity(tasks.len());
    for task in tasks {
        samples.push(task.await??);
    }
    samples.sort_unstable();
    let rank = samples
        .len()
        .saturating_mul(99)
        .div_ceil(100)
        .saturating_sub(1)
        .min(samples.len().saturating_sub(1));
    let p99 = samples[rank].as_secs_f64() * 1_000.0;
    println!(
        "requests={} concurrency={} p99_first_chunk_ms={p99:.3}",
        args.requests, args.concurrency
    );
    if p99 >= args.p99_target_ms {
        bail!(
            "p99 {p99:.3} ms did not meet target {:.3} ms",
            args.p99_target_ms
        );
    }
    Ok(())
}

fn emission_latency(first_chunk: &[u8]) -> Option<Duration> {
    let text = std::str::from_utf8(first_chunk).ok()?;
    let marker = "__haarness_emitted_ns:";
    let start = text.find(marker)? + marker.len();
    let digits = text[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    let emitted = digits.parse::<u128>().ok()?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let elapsed = now.checked_sub(emitted)?;
    Some(Duration::from_nanos(
        u64::try_from(elapsed).unwrap_or(u64::MAX),
    ))
}
