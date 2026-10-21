//! Local allocation hot-path benchmark; no network, credentials or model calls.
use piko::provider::ChatStream;
use serde_json::json;
use std::{hint::black_box, time::Instant};

fn main() {
    for count in [1_000, 8_000] {
        let delta = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"x".repeat(32)}}]}}]});
        let mut samples = Vec::new();
        for _ in 0..9 {
            let mut stream = ChatStream::default();
            stream.push(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"one","function":{"name":"write_file","arguments":""}}]}}]})).unwrap();
            let start = Instant::now();
            for _ in 0..count {
                stream.push(black_box(&delta)).unwrap();
            }
            black_box(stream);
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        samples.sort_by(f64::total_cmp);
        println!(
            "{}",
            json!({"deltas":count,"argument_bytes":count*32,"median_ms":samples[4],"samples_ms":samples})
        );
    }
}
