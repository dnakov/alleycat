use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

pub async fn read_json_line<T, R>(reader: &mut R) -> anyhow::Result<Option<T>>
where
    T: DeserializeOwned,
    R: AsyncBufRead + Unpin,
{
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader.read_line(&mut line).await?;
        if bytes == 0 {
            return Ok(None);
        }
        if line.trim().is_empty() {
            continue;
        }
        return Ok(Some(serde_json::from_str(line.trim_end())?));
    }
}

pub async fn write_json_line<T, W>(writer: &mut W, value: &T) -> anyhow::Result<()>
where
    T: Serialize,
    W: AsyncWrite + Unpin,
{
    let line = serde_json::to_vec(value)?;
    writer.write_all(&line).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}
