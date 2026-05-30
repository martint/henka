//! `Content-Length`-framed JSON-RPC message I/O, as used by LSP over stdio.

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{LspError, Result};

/// Write a single JSON-RPC message with an LSP `Content-Length` header.
pub async fn write_message<W: AsyncWrite + Unpin>(writer: &mut W, message: &Value) -> Result<()> {
    let body = serde_json::to_vec(message)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

/// Read a single framed JSON-RPC message, or `None` at end of stream.
pub async fn read_message<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            content_length = rest.trim().parse().ok();
        }
        // Other headers (e.g. Content-Type) are ignored.
    }

    let len = content_length
        .ok_or_else(|| LspError::Protocol("message had no Content-Length header".into()))?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    let value = serde_json::from_slice(&buf)?;
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn round_trips_a_message() {
        let (mut a, b) = tokio::io::duplex(4096);
        let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping", "params": { "x": 1 } });

        write_message(&mut a, &msg).await.unwrap();
        drop(a); // signal EOF after the message

        let mut reader = BufReader::new(b);
        let got = read_message(&mut reader).await.unwrap().unwrap();
        assert_eq!(got, msg);
        // Next read hits EOF.
        assert!(read_message(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn reads_two_messages_back_to_back() {
        let (mut a, b) = tokio::io::duplex(4096);
        write_message(&mut a, &json!({ "id": 1 })).await.unwrap();
        write_message(&mut a, &json!({ "id": 2 })).await.unwrap();
        drop(a);

        let mut reader = BufReader::new(b);
        assert_eq!(
            read_message(&mut reader).await.unwrap().unwrap(),
            json!({"id":1})
        );
        assert_eq!(
            read_message(&mut reader).await.unwrap().unwrap(),
            json!({"id":2})
        );
        assert!(read_message(&mut reader).await.unwrap().is_none());
    }
}
