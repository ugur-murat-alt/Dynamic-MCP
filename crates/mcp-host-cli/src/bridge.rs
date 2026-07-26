use std::{io, path::Path, sync::Arc, time::Duration};

use interprocess::local_socket::traits::tokio::Stream as _;
use mcp_host_core::{RuntimeError, RuntimeErrorCode};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    sync::Mutex,
};

use crate::ipc;

const STDIN_EOF_DRAIN_GRACE: Duration = Duration::from_secs(1);

/// Connects standard input and output directly to the daemon's MCP endpoint.
pub async fn run_stdio_bridge(
    runtime_dir: &Path,
    connect_timeout: Duration,
) -> Result<(), RuntimeError> {
    let stream = ipc::connect_mcp(runtime_dir, connect_timeout).await?;
    let (socket_reader, socket_writer) = stream.split();

    bridge_streams(
        tokio::io::stdin(),
        tokio::io::stdout(),
        socket_reader,
        socket_writer,
    )
    .await
    .map_err(ipc_unavailable)
}

async fn bridge_streams<Input, Output, SocketReader, SocketWriter>(
    input: Input,
    output: Output,
    socket_reader: SocketReader,
    socket_writer: SocketWriter,
) -> io::Result<()>
where
    Input: AsyncRead + Send + Unpin + 'static,
    Output: AsyncWrite + Send + Unpin + 'static,
    SocketReader: AsyncRead + Send + Unpin + 'static,
    SocketWriter: AsyncWrite + Send + Unpin + 'static,
{
    let mut input_to_socket = tokio::spawn(copy(input, socket_writer));
    let output = Arc::new(Mutex::new(output));
    let output_for_copy = Arc::clone(&output);
    let mut socket_to_output = tokio::spawn(async move {
        let mut output = output_for_copy.lock().await;
        copy(socket_reader, &mut *output).await
    });

    let (result, output_was_first) = tokio::select! {
        result = &mut input_to_socket => (result, false),
        result = &mut socket_to_output => (result, true),
    };

    if output_was_first {
        input_to_socket.abort();
        let _ = input_to_socket.await;
    } else {
        if tokio::time::timeout(STDIN_EOF_DRAIN_GRACE, &mut socket_to_output)
            .await
            .is_err()
        {
            socket_to_output.abort();
            let _ = socket_to_output.await;
        }
    }

    let flush_result = output.lock().await.flush().await;
    if let Err(error) = flush_result
        && error.kind() != io::ErrorKind::BrokenPipe
    {
        return Err(error);
    }

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) if output_was_first && error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(error) => Err(io::Error::other(error)),
    }
}

async fn copy<Reader, Writer>(mut reader: Reader, mut writer: Writer) -> io::Result<()>
where
    Reader: AsyncRead + Unpin,
    Writer: AsyncWrite + Unpin,
{
    tokio::io::copy(&mut reader, &mut writer).await?;
    Ok(())
}

fn ipc_unavailable(_: io::Error) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::IpcUnavailable,
        "ipc",
        "the daemon IPC endpoint is unavailable",
    )
    .retryable()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt, duplex},
        time::timeout,
    };

    use super::bridge_streams;

    #[tokio::test]
    async fn preserves_bytes_in_both_directions() {
        let (mut client_input, bridge_input) = duplex(1024);
        let (bridge_output, mut client_output) = duplex(1024);
        let (bridge_socket, mut daemon_socket) = duplex(1024);
        let (socket_reader, socket_writer) = tokio::io::split(bridge_socket);
        let bridge = tokio::spawn(bridge_streams(
            bridge_input,
            bridge_output,
            socket_reader,
            socket_writer,
        ));
        let input_payload = b"\0client\xffpayload";
        let output_payload = b"\xfddaemon\0payload";

        client_input.write_all(input_payload).await.unwrap();
        let mut received_input = vec![0; input_payload.len()];
        daemon_socket.read_exact(&mut received_input).await.unwrap();
        assert_eq!(received_input, input_payload);

        daemon_socket.write_all(output_payload).await.unwrap();
        let mut received_output = vec![0; output_payload.len()];
        client_output
            .read_exact(&mut received_output)
            .await
            .unwrap();
        assert_eq!(received_output, output_payload);

        drop(client_input);
        bridge.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn stops_after_one_side_eof() {
        let (client_input, bridge_input) = duplex(64);
        let (bridge_output, _client_output) = duplex(64);
        let (bridge_socket, mut daemon_socket) = duplex(64);
        let (socket_reader, socket_writer) = tokio::io::split(bridge_socket);
        let bridge = tokio::spawn(bridge_streams(
            bridge_input,
            bridge_output,
            socket_reader,
            socket_writer,
        ));

        drop(client_input);
        timeout(Duration::from_secs(3), bridge)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        let mut byte = [0; 1];
        assert_eq!(
            timeout(Duration::from_secs(1), daemon_socket.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn preserves_newline_and_multiline_payloads() {
        let (mut client_input, bridge_input) = duplex(1024);
        let (bridge_output, mut client_output) = duplex(1024);
        let (bridge_socket, mut daemon_socket) = duplex(1024);
        let (socket_reader, socket_writer) = tokio::io::split(bridge_socket);
        let bridge = tokio::spawn(bridge_streams(
            bridge_input,
            bridge_output,
            socket_reader,
            socket_writer,
        ));
        let input_payload = b"first line\nsecond line\n\nthird line\n";
        let output_payload = b"{\"result\":\"first\\nsecond\\n\"}\n{\"next\":true}\n";

        client_input.write_all(input_payload).await.unwrap();
        let mut received_input = vec![0; input_payload.len()];
        daemon_socket.read_exact(&mut received_input).await.unwrap();
        assert_eq!(received_input, input_payload);

        daemon_socket.write_all(output_payload).await.unwrap();
        let mut received_output = vec![0; output_payload.len()];
        client_output
            .read_exact(&mut received_output)
            .await
            .unwrap();
        assert_eq!(received_output, output_payload);

        drop(client_input);
        bridge.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn drains_an_in_flight_response_after_stdin_eof() {
        let (mut client_input, bridge_input) = duplex(1024);
        let (bridge_output, mut client_output) = duplex(1024);
        let (bridge_socket, mut daemon_socket) = duplex(1024);
        let (socket_reader, socket_writer) = tokio::io::split(bridge_socket);
        let bridge = tokio::spawn(bridge_streams(
            bridge_input,
            bridge_output,
            socket_reader,
            socket_writer,
        ));
        client_input.write_all(b"request\n").await.unwrap();
        drop(client_input);
        let mut request = [0_u8; 8];
        daemon_socket.read_exact(&mut request).await.unwrap();
        assert_eq!(&request, b"request\n");
        daemon_socket.write_all(b"response\n").await.unwrap();

        let mut response = [0_u8; 9];
        timeout(
            Duration::from_secs(1),
            client_output.read_exact(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&response, b"response\n");
        timeout(Duration::from_secs(2), bridge)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
