use anyhow::{Context, anyhow};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

pub async fn read_json_frame<T, R>(reader: &mut R) -> anyhow::Result<T>
where
    T: DeserializeOwned,
    R: AsyncRead + Unpin,
{
    let len = reader.read_u32().await.context("reading frame length")? as usize;
    if len > MAX_FRAME_BYTES {
        return Err(anyhow!("frame too large: {len} bytes"));
    }
    let mut buf = vec![0u8; len];
    reader
        .read_exact(&mut buf)
        .await
        .context("reading frame body")?;
    serde_json::from_slice(&buf).context("decoding JSON frame")
}

pub async fn write_json_frame<T, W>(writer: &mut W, value: &T) -> anyhow::Result<()>
where
    T: Serialize,
    W: AsyncWrite + Unpin,
{
    let buf = serde_json::to_vec(value).context("encoding JSON frame")?;
    if buf.len() > MAX_FRAME_BYTES {
        return Err(anyhow!("frame too large: {} bytes", buf.len()));
    }
    writer
        .write_u32(buf.len() as u32)
        .await
        .context("writing frame length")?;
    writer.write_all(&buf).await.context("writing frame body")?;
    writer.flush().await.context("flushing frame")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use tokio::io::{AsyncWriteExt, duplex};

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct TestFrame {
        v: u32,
        token: String,
    }

    #[tokio::test]
    async fn json_frame_round_trips() {
        let (mut client, mut server) = duplex(1024);
        let expected = TestFrame {
            v: 1,
            token: "abc123".to_string(),
        };
        write_json_frame(&mut client, &expected).await.unwrap();
        let actual: TestFrame = read_json_frame(&mut server).await.unwrap();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn rejects_oversized_frame_length_before_reading_body() {
        let (mut client, mut server) = duplex(16);
        client
            .write_u32((MAX_FRAME_BYTES + 1) as u32)
            .await
            .unwrap();
        let err = read_json_frame::<TestFrame, _>(&mut server)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("frame too large"));
    }
}
