use tokio::io::{AsyncRead, AsyncReadExt};

const OUTPUT_READ_BUFFER_SIZE: usize = 8192;

pub(super) async fn read_output_stream<R>(
    reader: Option<R>,
    capture: bool,
    max_output_size: usize,
) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let Some(mut reader) = reader else {
        return Ok(Vec::new());
    };

    let mut captured = Vec::new();
    let mut buffer = [0_u8; OUTPUT_READ_BUFFER_SIZE];
    let mut truncated = false;

    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }

        if capture && !truncated {
            let chunk = &buffer[..read];
            let available = max_output_size.saturating_sub(captured.len());
            if available == 0 {
                truncated = true;
                continue;
            }

            let to_copy = available.min(chunk.len());
            captured.extend_from_slice(&chunk[..to_copy]);
            if to_copy < chunk.len() {
                truncated = true;
            }
        }
    }

    Ok(captured)
}

pub(super) fn append_capped(target: &mut Vec<u8>, chunk: &[u8], max_output_size: usize) {
    let available = max_output_size.saturating_sub(target.len());
    if available == 0 {
        return;
    }

    let to_copy = available.min(chunk.len());
    target.extend_from_slice(&chunk[..to_copy]);
}

#[cfg(test)]
mod tests {
    use super::{append_capped, read_output_stream};
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn read_output_stream_truncates_to_requested_size() {
        let (mut writer, reader) = tokio::io::duplex(32);
        tokio::spawn(async move {
            writer
                .write_all(b"hello world")
                .await
                .expect("writer should succeed");
        });

        let output = read_output_stream(Some(reader), true, 5)
            .await
            .expect("reader should succeed");
        assert_eq!(output, b"hello");
    }

    #[tokio::test]
    async fn read_output_stream_discards_when_capture_is_disabled() {
        let (mut writer, reader) = tokio::io::duplex(32);
        tokio::spawn(async move {
            writer
                .write_all(b"hello world")
                .await
                .expect("writer should succeed");
        });

        let output = read_output_stream(Some(reader), false, 5)
            .await
            .expect("reader should succeed");
        assert!(output.is_empty());
    }

    #[test]
    fn append_capped_respects_remaining_capacity() {
        let mut output = b"hello".to_vec();
        append_capped(&mut output, b" world", 8);
        assert_eq!(output, b"hello wo");
    }
}
