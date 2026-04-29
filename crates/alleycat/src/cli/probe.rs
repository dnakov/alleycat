//! `alleycat probe` — local debug client that connects to the daemon over iroh
//! exactly the way the phone does, runs the JSON-RPC initialize handshake
//! against an agent, and invokes a method (default `thread/list`).
//!
//! Two modes:
//! - No `--agent`: round-trip a `list_agents` over the alleycat protocol and
//!   print the agent table.
//! - With `--agent <name>`: open a `connect`-style stream, send `initialize`
//!   + `initialized` + the user-supplied method, and dump every JSON-RPC frame
//!   in/out.
//!
//! Identity: reads the daemon's local `host.toml` + `host.key` so the probe
//! authenticates with the same node id and token a phone holding the QR
//! payload would. Generates a fresh client iroh identity each run.

use std::time::Duration;

use anyhow::{Context, anyhow};
use clap::Args;
use iroh::endpoint::presets;
use iroh::{Endpoint, PublicKey, SecretKey};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::framing::{read_json_frame, write_json_frame};
use crate::host;
use crate::protocol::{ALLEYCAT_ALPN, PROTOCOL_VERSION, Request, Response};

#[derive(Args, Debug)]
pub struct ProbeArgs {
    /// Agent to connect to (`pi`, `opencode`, `codex`). Omit to round-trip a
    /// `list_agents` call instead.
    #[arg(long)]
    pub agent: Option<String>,
    /// JSON-RPC method to invoke after `initialize` succeeds. Ignored when
    /// `--agent` is omitted. Defaults to `thread/list`.
    #[arg(long)]
    pub method: Option<String>,
    /// JSON params for the method. Defaults to `{}`.
    #[arg(long, default_value = "{}")]
    pub params: String,
    /// Override the node id to dial. Defaults to the local daemon's node id
    /// (read from `host.key`). Useful for probing a remote alleycat.
    #[arg(long)]
    pub node_id: Option<String>,
    /// Override the auth token. Defaults to the local daemon's token (read
    /// from `host.toml`). Pair this with `--node-id` to probe a remote.
    #[arg(long)]
    pub token: Option<String>,
    /// How long to wait for additional JSON-RPC frames after the method
    /// response before exiting, in seconds. Streaming methods may push
    /// notifications; raise this to capture them.
    #[arg(long, default_value_t = 5)]
    pub linger_secs: u64,
    /// Timeout for the JSON-RPC method response, in seconds.
    #[arg(long, default_value_t = 30)]
    pub timeout_secs: u64,
}

pub async fn run(args: ProbeArgs) -> anyhow::Result<()> {
    let cfg = crate::config::load_or_init().await?;
    let server_secret = crate::state::load_or_create_secret_key().await?;

    let token = match &args.token {
        Some(t) => t.clone(),
        None => cfg.token.clone(),
    };
    let node_id: PublicKey = match &args.node_id {
        Some(s) => s
            .parse()
            .with_context(|| format!("parsing --node-id {s:?} as iroh public key"))?,
        None => server_secret.public(),
    };

    let payload = host::pair_payload(&server_secret, &cfg);
    eprintln!(
        "probe: dialing node_id={} token={} relay={}",
        node_id,
        short_token(&token),
        payload.relay.as_deref().unwrap_or("<iroh default>")
    );

    let endpoint = build_client_endpoint().await?;
    let _ = tokio::time::timeout(Duration::from_secs(8), endpoint.online()).await;

    let conn = endpoint
        .connect(node_id, ALLEYCAT_ALPN)
        .await
        .with_context(|| format!("dialing alleycat node {node_id}"))?;
    eprintln!("probe: iroh connection established");

    match args.agent.as_deref() {
        None => list_agents(&conn, &token).await,
        Some(agent) => probe_agent(&conn, &token, agent, &args).await,
    }
}

async fn list_agents(conn: &iroh::endpoint::Connection, token: &str) -> anyhow::Result<()> {
    let (mut send, mut recv) = conn.open_bi().await.context("opening list_agents stream")?;
    write_json_frame(
        &mut send,
        &Request::ListAgents {
            v: PROTOCOL_VERSION,
            token: token.to_string(),
        },
    )
    .await?;
    send.finish().ok();
    let resp: Response = read_json_frame(&mut recv).await?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}

async fn probe_agent(
    conn: &iroh::endpoint::Connection,
    token: &str,
    agent: &str,
    args: &ProbeArgs,
) -> anyhow::Result<()> {
    let method = args
        .method
        .clone()
        .unwrap_or_else(|| "thread/list".to_string());
    let params: Value = serde_json::from_str(&args.params)
        .with_context(|| format!("parsing --params {:?} as JSON", args.params))?;

    let (mut send, recv) = conn
        .open_bi()
        .await
        .with_context(|| format!("opening connect stream for agent `{agent}`"))?;
    write_json_frame(
        &mut send,
        &Request::Connect {
            v: PROTOCOL_VERSION,
            token: token.to_string(),
            agent: agent.to_string(),
            resume: None,
        },
    )
    .await?;

    // First read the length-prefixed connect ack on the same recv handle,
    // then keep recv (BufReader-wrapped) for the JSONL phase.
    let mut recv = recv;
    let resp: Response = read_json_frame(&mut recv).await?;
    if !resp.ok {
        anyhow::bail!(
            "connect rejected: {}",
            resp.error.unwrap_or_else(|| "<no error>".to_string())
        );
    }
    eprintln!("probe: connect ok agent={agent}; switching to JSONL");

    let mut reader = BufReader::new(recv);

    // initialize
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "clientInfo": {
                "name": "alleycat-probe",
                "version": env!("CARGO_PKG_VERSION"),
                "title": "alleycat-probe"
            },
            "capabilities": {}
        }
    });
    print_outbound(&init);
    write_jsonl(&mut send, &init).await?;

    // Read until we see a response with id=1.
    loop {
        let frame = read_jsonl_with_timeout(&mut reader, Duration::from_secs(args.timeout_secs))
            .await
            .context("reading initialize response")?;
        print_inbound(&frame);
        if frame.get("id").is_some() {
            break;
        }
    }

    // initialized notification
    let initialized = json!({
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {}
    });
    print_outbound(&initialized);
    write_jsonl(&mut send, &initialized).await?;

    // user method
    let method_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": method,
        "params": params,
    });
    print_outbound(&method_req);
    write_jsonl(&mut send, &method_req).await?;

    // Drain frames until we see id=2 response, then linger for late
    // notifications.
    let mut got_response = false;
    let response_deadline = tokio::time::Instant::now() + Duration::from_secs(args.timeout_secs);
    while !got_response && tokio::time::Instant::now() < response_deadline {
        match read_jsonl_with_timeout(
            &mut reader,
            response_deadline.saturating_duration_since(tokio::time::Instant::now()),
        )
        .await
        {
            Ok(frame) => {
                print_inbound(&frame);
                if frame.get("id") == Some(&json!(2)) {
                    got_response = true;
                }
            }
            Err(error) => {
                eprintln!("probe: read error: {error:#}");
                break;
            }
        }
    }
    if !got_response {
        eprintln!(
            "probe: did not receive response to id=2 ({method}) within {}s",
            args.timeout_secs
        );
    }

    // Linger window — capture any trailing notifications the bridge pushes.
    if args.linger_secs > 0 {
        eprintln!("probe: lingering {}s for trailing frames", args.linger_secs);
        let linger_deadline = tokio::time::Instant::now() + Duration::from_secs(args.linger_secs);
        while tokio::time::Instant::now() < linger_deadline {
            match read_jsonl_with_timeout(
                &mut reader,
                linger_deadline.saturating_duration_since(tokio::time::Instant::now()),
            )
            .await
            {
                Ok(frame) => print_inbound(&frame),
                Err(_) => break,
            }
        }
    }

    let _ = send.finish();
    Ok(())
}

async fn write_jsonl(stream: &mut iroh::endpoint::SendStream, value: &Value) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_jsonl_with_timeout<R>(
    reader: &mut BufReader<R>,
    timeout: Duration,
) -> anyhow::Result<Value>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut line = String::new();
    let n = tokio::time::timeout(timeout, reader.read_line(&mut line))
        .await
        .map_err(|_| anyhow!("timed out waiting for JSON line"))??;
    if n == 0 {
        return Err(anyhow!("stream closed by peer"));
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("empty JSON line"));
    }
    serde_json::from_str(trimmed).with_context(|| format!("decoding JSON-RPC line: {trimmed}"))
}

fn print_outbound(value: &Value) {
    let pretty = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    eprintln!("→ {pretty}");
}

fn print_inbound(value: &Value) {
    let pretty = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    println!("← {pretty}");
}

async fn build_client_endpoint() -> anyhow::Result<Endpoint> {
    let secret = SecretKey::generate();
    Endpoint::builder(presets::N0)
        .secret_key(secret)
        .alpns(vec![ALLEYCAT_ALPN.to_vec()])
        .bind()
        .await
        .context("binding probe client endpoint")
}

fn short_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(&Sha256::digest(token.as_bytes())[..4])
}
