use std::time::Duration;

use anyhow::{Context, anyhow};
use iroh::Endpoint;
use iroh::endpoint::presets;
use tokio::signal;
use tracing::{debug, info, warn};

use crate::agents::AgentManager;
use crate::config::HostConfig;
use crate::framing::{read_json_frame, write_json_frame};
use crate::protocol::{ALLEYCAT_ALPN, PROTOCOL_VERSION, Request, Response};
use crate::state;
use crate::stream::IrohStream;

pub async fn serve() -> anyhow::Result<()> {
    let mut lock = state::acquire_lock().await?;
    let _guard = lock
        .try_write()
        .context("another alleycat process is already running")?;

    let config = crate::config::load_or_init().await?;
    let secret_key = state::load_or_create_secret_key().await?;
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret_key)
        .alpns(vec![ALLEYCAT_ALPN.to_vec()])
        .bind()
        .await
        .context("binding iroh endpoint")?;

    info!(node_id = %endpoint.id(), "alleycat endpoint bound");
    let endpoint_for_online = endpoint.clone();
    tokio::spawn(async move {
        if tokio::time::timeout(Duration::from_secs(8), endpoint_for_online.online())
            .await
            .is_ok()
        {
            info!(addr = ?endpoint_for_online.addr(), "alleycat endpoint online");
        } else {
            warn!("alleycat endpoint did not report relay connectivity within timeout");
        }
    });

    let agents = AgentManager::new(config).await?;

    loop {
        tokio::select! {
            biased;
            result = signal::ctrl_c() => {
                result.context("waiting for ctrl-c")?;
                info!("received shutdown signal");
                endpoint.close().await;
                break;
            }
            incoming = endpoint.accept() => {
                let Some(connecting) = incoming else {
                    break;
                };
                let agents = agents.clone();
                tokio::spawn(async move {
                    match connecting.await {
                        Ok(conn) => {
                            while let Ok((send, recv)) = conn.accept_bi().await {
                                let agents = agents.clone();
                                tokio::spawn(async move {
                                    if let Err(error) = handle_stream(send, recv, agents).await {
                                        debug!("alleycat stream ended: {error:#}");
                                    }
                                });
                            }
                        }
                        Err(error) => debug!("alleycat incoming connection failed: {error:#}"),
                    }
                });
            }
        }
    }
    Ok(())
}

async fn handle_stream(
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    agents: AgentManager,
) -> anyhow::Result<()> {
    let request: Request = read_json_frame(&mut recv).await?;
    if let Err(error) = validate_version(&request) {
        write_json_frame(&mut send, &Response::error(&error)).await?;
        return Err(anyhow!(error));
    }

    let config = crate::config::load_or_init().await?;
    if let Err(error) = validate_token(&request, &config.token) {
        write_json_frame(&mut send, &Response::error(&error)).await?;
        return Err(anyhow!(error));
    }

    match request {
        Request::ListAgents { .. } => {
            let list = agents.list_agents().await;
            write_json_frame(&mut send, &Response::agents(list)).await?;
            Ok(())
        }
        Request::Connect { agent, .. } => {
            if !agents.agent_enabled(&agent) {
                write_json_frame(
                    &mut send,
                    &Response::error(format!("agent `{agent}` is disabled or unknown")),
                )
                .await?;
                return Err(anyhow!("agent disabled or unknown: {agent}"));
            }
            write_json_frame(&mut send, &Response::ok()).await?;
            agents
                .serve_agent(&agent, IrohStream::new(send, recv))
                .await
                .with_context(|| format!("serving agent `{agent}`"))
        }
    }
}

pub fn pair_payload(
    secret_key: &iroh::SecretKey,
    config: &HostConfig,
) -> crate::protocol::PairPayload {
    crate::protocol::PairPayload {
        v: PROTOCOL_VERSION,
        node_id: secret_key.public().to_string(),
        token: config.token.clone(),
        relay: config.relay.clone(),
    }
}

fn validate_version(request: &Request) -> Result<(), String> {
    if request.version() == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(format!(
            "protocol mismatch: client={} host={}",
            request.version(),
            PROTOCOL_VERSION
        ))
    }
}

fn validate_token(request: &Request, expected_token: &str) -> Result<(), String> {
    if request.token() == expected_token {
        Ok(())
    } else {
        Err("invalid token".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentsConfig, HostConfig};

    #[test]
    fn pair_payload_uses_stable_node_id_and_token() {
        let secret_key = iroh::SecretKey::generate();
        let config = HostConfig {
            token: "token-1".to_string(),
            relay: Some("https://relay.example".to_string()),
            agents: AgentsConfig::default(),
        };

        let payload = pair_payload(&secret_key, &config);

        assert_eq!(payload.v, PROTOCOL_VERSION);
        assert_eq!(payload.node_id, secret_key.public().to_string());
        assert_eq!(payload.token, "token-1");
        assert_eq!(payload.relay.as_deref(), Some("https://relay.example"));
    }

    #[test]
    fn first_frame_auth_accepts_matching_token() {
        let request = Request::Connect {
            v: PROTOCOL_VERSION,
            token: "secret".to_string(),
            agent: "codex".to_string(),
        };

        validate_version(&request).unwrap();
        validate_token(&request, "secret").unwrap();
    }

    #[test]
    fn first_frame_auth_rejects_protocol_mismatch() {
        let request = Request::ListAgents {
            v: PROTOCOL_VERSION + 1,
            token: "secret".to_string(),
        };

        let err = validate_version(&request).unwrap_err();
        assert!(err.contains("protocol mismatch"));
    }

    #[test]
    fn first_frame_auth_rejects_invalid_token() {
        let request = Request::ListAgents {
            v: PROTOCOL_VERSION,
            token: "wrong".to_string(),
        };

        validate_version(&request).unwrap();
        assert_eq!(
            validate_token(&request, "secret").unwrap_err(),
            "invalid token"
        );
    }
}
