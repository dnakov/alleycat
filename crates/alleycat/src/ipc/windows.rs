use std::time::Duration;

use anyhow::Context;
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};

use super::ControlStream;
use crate::paths;

/// `ERROR_PIPE_BUSY`. Inlined to keep `windows-sys` out of the link line.
const ERROR_PIPE_BUSY: i32 = 231;

pub(super) struct Listener {
    name: String,
    next: Option<NamedPipeServer>,
}

impl Listener {
    pub async fn bind() -> anyhow::Result<Self> {
        let name = paths::control_pipe_name()?;
        Self::bind_at(name).await
    }

    pub async fn bind_at(name: String) -> anyhow::Result<Self> {
        let first = ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create(&name)
            .with_context(|| format!("creating named pipe {name}"))?;
        Ok(Self {
            name,
            next: Some(first),
        })
    }

    pub async fn accept(&mut self) -> anyhow::Result<Box<dyn ControlStream>> {
        let server = self
            .next
            .take()
            .context("accept called on a closed listener")?;
        server
            .connect()
            .await
            .context("waiting for client to connect to named pipe")?;
        let next = ServerOptions::new()
            .reject_remote_clients(true)
            .create(&self.name)
            .with_context(|| format!("re-arming named pipe {}", self.name))?;
        self.next = Some(next);
        Ok(Box::new(server))
    }
}

pub(super) async fn connect() -> anyhow::Result<Box<dyn ControlStream>> {
    let name = paths::control_pipe_name()?;
    connect_at(&name).await
}

pub(super) async fn connect_at(name: &str) -> anyhow::Result<Box<dyn ControlStream>> {
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        match ClientOptions::new().open(name) {
            Ok(client) => return Ok(Box::new(client)),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                if std::time::Instant::now() >= deadline {
                    return Err(anyhow::anyhow!(e))
                        .with_context(|| format!("named pipe {name} stayed busy past 1s"));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => {
                return Err(anyhow::anyhow!(e))
                    .with_context(|| format!("connecting to named pipe {name}"));
            }
        }
    }
}
