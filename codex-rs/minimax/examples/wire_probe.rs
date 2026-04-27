//! Local-only probe to identify which top-level field causes MiniMax's
//! "invalid chat setting (2013)" rejection.
//!
//! Reads the API key from ~/.codex/auth.json (or auth_file_path env)
//! and sends a series of targeted requests that toggle one field at a
//! time. Prints status + first ~80 chars of the response body for each.
//!
//! Run with:  cargo run -p codex-minimax --example wire_probe
//!
//! Not part of the shipped binary — examples/ is excluded from
//! library consumers and the example never embeds secrets.

use std::path::PathBuf;

use serde_json::json;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let home = std::env::var("HOME").expect("HOME unset");
    let path = std::env::var("CODREX_AUTH_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("{home}/.codex/auth.json")));
    let raw = std::fs::read_to_string(&path)?;
    let v: serde_json::Value = serde_json::from_str(&raw)?;
    let api_key = v
        .pointer("/providers/minimax/api_key")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow::anyhow!("minimax key not found in {}", path.display()))?
        .to_string();

    let url = "https://api.minimax.io/v1/chat/completions";
    let client = reqwest::Client::new();
    let model = "MiniMax-M2.7";

    // Probe matrix: each entry is (label, body_extra) merged onto baseline.
    // Baseline = minimal valid (mirrors --test-connection).
    let baseline = || {
        json!({
            "model": model,
            "messages": [{"role":"user","content":"reply with the word ok"}],
            "stream": false,
            "max_tokens": 8
        })
    };

    let probes: Vec<(&str, Box<dyn Fn() -> serde_json::Value>)> = vec![
        ("01-baseline (control)", Box::new(baseline)),
        (
            "02-baseline + reasoning_split:true",
            Box::new(|| {
                let mut b = baseline();
                b["reasoning_split"] = json!(true);
                b
            }),
        ),
        (
            "03-baseline + stream:true",
            Box::new(|| {
                let mut b = baseline();
                b["stream"] = json!(true);
                b
            }),
        ),
        (
            "04-baseline + 1 function tool",
            Box::new(|| {
                let mut b = baseline();
                b["tools"] = json!([{
                    "type":"function",
                    "function":{
                        "name":"shell",
                        "description":"run a shell command",
                        "parameters":{
                            "type":"object",
                            "properties":{"cmd":{"type":"string"}},
                            "required":["cmd"]
                        }
                    }
                }]);
                b
            }),
        ),
        (
            "05-baseline + 2 system messages back-to-back",
            Box::new(|| {
                json!({
                    "model": model,
                    "messages": [
                        {"role":"system","content":"You are helpful."},
                        {"role":"system","content":"Be concise."},
                        {"role":"user","content":"reply ok"}
                    ],
                    "stream": false,
                    "max_tokens": 8
                })
            }),
        ),
        (
            "05b-baseline + 2 user messages back-to-back",
            Box::new(|| {
                json!({
                    "model": model,
                    "messages": [
                        {"role":"user","content":"first message"},
                        {"role":"user","content":"reply ok"}
                    ],
                    "stream": false,
                    "max_tokens": 8
                })
            }),
        ),
        (
            "06-full mirror of failing run (stream+reasoning_split+tools)",
            Box::new(|| {
                json!({
                    "model": model,
                    "messages": [
                        {"role":"system","content":"You are helpful."},
                        {"role":"user","content":"reply ok"}
                    ],
                    "stream": true,
                    "reasoning_split": true,
                    "tools": [{
                        "type":"function",
                        "function":{
                            "name":"shell",
                            "description":"run a shell command",
                            "parameters":{
                                "type":"object",
                                "properties":{"cmd":{"type":"string"}},
                                "required":["cmd"]
                            }
                        }
                    }]
                })
            }),
        ),
    ];

    for (label, build) in &probes {
        let body = build();
        let resp = client
            .post(url)
            .bearer_auth(&api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let snippet: String = text.chars().take(180).collect();
        println!("[{label}] HTTP {} -> {snippet}", status.as_u16());
    }

    Ok(())
}
