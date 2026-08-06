use std::{
    io,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

#[cfg(unix)]
use interprocess::local_socket::{GenericFilePath, ToFsName};
#[cfg(windows)]
use interprocess::local_socket::{GenericNamespaced, ToNsName};
use interprocess::local_socket::{
    ListenerOptions,
    tokio::{Listener, Stream},
    traits::tokio::Stream as _,
};
use mcp_host_core::{
    CONTROL_PROTOCOL_VERSION, ControlRequest, ControlRequestEnvelope, ControlResponseEnvelope,
    RuntimeError, RuntimeErrorCode,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    time::timeout,
};

/// The largest permitted control-plane frame payload, excluding its four-byte length prefix.
pub const MAX_CONTROL_FRAME_SIZE: usize = 8 * 1024 * 1024;

#[cfg(unix)]
const UNIX_SOCKET_PATH_LIMIT: usize = 100;
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// A local IPC endpoint exposed by the daemon.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointKind {
    Control,
    Mcp,
}

impl EndpointKind {
    #[must_use]
    pub const fn display(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Mcp => "mcp",
        }
    }
}

/// Deterministic local IPC endpoint addresses for one daemon runtime directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndpointSet {
    pub control: String,
    pub mcp: String,
}

impl EndpointSet {
    /// Builds deterministic endpoint addresses for `runtime_dir`.
    ///
    /// On Windows, the named pipes use interprocess's default security descriptor because this
    /// module does not supply a custom descriptor to `ListenerOptions`.
    pub fn for_runtime_dir(runtime_dir: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let control = runtime_dir.join("control.sock");
            let mcp = runtime_dir.join("mcp.sock");
            validate_unix_socket_path(&control)?;
            validate_unix_socket_path(&mcp)?;

            Ok(Self {
                control: control.to_string_lossy().into_owned(),
                mcp: mcp.to_string_lossy().into_owned(),
            })
        }

        #[cfg(windows)]
        {
            let hash = fnv1a(runtime_dir.to_string_lossy().as_bytes());
            Ok(Self {
                control: format!("mcp-host-{hash:016x}-control"),
                mcp: format!("mcp-host-{hash:016x}-mcp"),
            })
        }
    }

    #[must_use]
    pub fn address(&self, kind: EndpointKind) -> &str {
        match kind {
            EndpointKind::Control => &self.control,
            EndpointKind::Mcp => &self.mcp,
        }
    }

    pub fn bind(&self, kind: EndpointKind) -> io::Result<Listener> {
        #[cfg(unix)]
        {
            use interprocess::os::unix::local_socket::ListenerOptionsExt;

            let name = Path::new(self.address(kind)).to_fs_name::<GenericFilePath>()?;
            let result = ListenerOptions::new()
                .name(name)
                .reclaim_name(false)
                .mode(0o600)
                .create_tokio();
            if result
                .as_ref()
                .is_err_and(|error| error.kind() == io::ErrorKind::Unsupported)
            {
                let name = Path::new(self.address(kind)).to_fs_name::<GenericFilePath>()?;
                return ListenerOptions::new()
                    .name(name)
                    .reclaim_name(false)
                    .create_tokio();
            }
            result
        }

        #[cfg(windows)]
        {
            let name = self.address(kind).to_ns_name::<GenericNamespaced>()?;
            ListenerOptions::new()
                .name(name)
                .reclaim_name(false)
                .create_tokio()
        }
    }

    pub async fn connect(&self, kind: EndpointKind) -> io::Result<Stream> {
        #[cfg(unix)]
        {
            let name = Path::new(self.address(kind)).to_fs_name::<GenericFilePath>()?;
            Stream::connect(name).await
        }

        #[cfg(windows)]
        {
            let name = self.address(kind).to_ns_name::<GenericNamespaced>()?;
            Stream::connect(name).await
        }
    }

    /// Removes stale Unix socket files. The daemon lock owner must call this before binding.
    pub fn cleanup_stale(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::{fs, os::unix::fs::FileTypeExt};

            for address in [&self.control, &self.mcp] {
                match fs::symlink_metadata(address) {
                    Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(address)?,
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            if let Some(directory) = Path::new(&self.mcp).parent()
                && let Ok(entries) = fs::read_dir(directory)
            {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with("mcp-") && name.ends_with(".sock") {
                        match entry.file_type() {
                            Ok(file_type) if file_type.is_socket() => {
                                let _ = fs::remove_file(entry.path());
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        #[cfg(windows)]
        {}

        Ok(())
    }
}

/// Writes one length-prefixed control-plane frame.
pub async fn write_frame<W>(writer: &mut W, payload: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    if payload.len() > MAX_CONTROL_FRAME_SIZE {
        return Err(frame_size_error());
    }

    let length = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "control frame is too large"))?;
    writer.write_all(&length.to_be_bytes()).await?;
    writer.write_all(payload).await
}

/// Reads one bounded length-prefixed control-plane frame.
pub async fn read_frame<R>(reader: &mut R) -> io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut prefix = [0_u8; 4];
    reader.read_exact(&mut prefix).await?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length > MAX_CONTROL_FRAME_SIZE {
        return Err(frame_size_error());
    }

    let mut payload = vec![0; length];
    reader.read_exact(&mut payload).await?;
    Ok(payload)
}

/// Serializes and writes a JSON control-plane frame.
pub async fn write_json<W, T>(writer: &mut W, value: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize + ?Sized,
{
    let payload = serde_json::to_vec(value).map_err(json_error)?;
    write_frame(writer, &payload).await
}

/// Reads and deserializes a JSON control-plane frame.
pub async fn read_json<R, T>(reader: &mut R) -> io::Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let payload = read_frame(reader).await?;
    serde_json::from_slice(&payload).map_err(json_error)
}

/// Sends one control request and returns its sole successful response value.
pub async fn send_control(
    runtime_dir: &Path,
    request: &ControlRequest,
    request_timeout: Duration,
) -> Result<Value, RuntimeError> {
    let endpoints = EndpointSet::for_runtime_dir(runtime_dir).map_err(ipc_unavailable)?;
    let request_id = next_request_id();
    let envelope = ControlRequestEnvelope::new(request_id.clone(), request.clone());

    let mut stream = timeout(request_timeout, endpoints.connect(EndpointKind::Control))
        .await
        .map_err(|_| ipc_unavailable_timeout())?
        .map_err(ipc_unavailable)?;

    timeout(request_timeout, write_json(&mut stream, &envelope))
        .await
        .map_err(|_| ipc_unavailable_timeout())?
        .map_err(ipc_unavailable)?;

    let response: ControlResponseEnvelope = timeout(request_timeout, read_json(&mut stream))
        .await
        .map_err(|_| ipc_unavailable_timeout())?
        .map_err(control_read_error)?;

    if response.protocol_version != CONTROL_PROTOCOL_VERSION || response.request_id != request_id {
        return Err(ipc_protocol_mismatch());
    }

    match (response.result, response.error) {
        (Some(result), None) => Ok(result),
        (None, Some(error)) => Err(error),
        (None, None) | (Some(_), Some(_)) => Err(ipc_protocol_mismatch()),
    }
}

/// Connects a bridge to the daemon's MCP endpoint without applying control-plane framing.
pub async fn connect_mcp(
    runtime_dir: &Path,
    request_timeout: Duration,
) -> Result<Stream, RuntimeError> {
    let endpoints = EndpointSet::for_runtime_dir(runtime_dir).map_err(ipc_unavailable)?;
    timeout(request_timeout, endpoints.connect(EndpointKind::Mcp))
        .await
        .map_err(|_| ipc_unavailable_timeout())?
        .map_err(ipc_unavailable)
}

/// Connects a bridge to an arbitrary MCP endpoint path without control-plane framing.
pub async fn connect_mcp_at(
    endpoint: &Path,
    request_timeout: Duration,
) -> Result<Stream, RuntimeError> {
    timeout(request_timeout, endpoints_connect_at(endpoint))
        .await
        .map_err(|_| ipc_unavailable_timeout())?
        .map_err(ipc_unavailable)
}

/// Binds a listener for an arbitrary Unix socket path (or Windows pipe name).
pub fn bind_socket_at(endpoint: &Path) -> io::Result<Listener> {
    #[cfg(unix)]
    {
        use interprocess::os::unix::local_socket::ListenerOptionsExt as _;

        let name = endpoint.to_fs_name::<GenericFilePath>()?;
        ListenerOptions::new()
            .name(name)
            .reclaim_name(false)
            .mode(0o600)
            .create_tokio()
    }

    #[cfg(windows)]
    {
        use interprocess::local_socket::{GenericNamespaced, ToNsName};

        let label = endpoint.to_string_lossy();
        let name = label.to_ns_name::<GenericNamespaced>()?;
        ListenerOptions::new()
            .name(name)
            .reclaim_name(false)
            .create_tokio()
    }
}

async fn endpoints_connect_at(endpoint: &Path) -> io::Result<Stream> {
    #[cfg(unix)]
    {
        let name = endpoint.to_fs_name::<GenericFilePath>()?;
        interprocess::local_socket::tokio::Stream::connect(name).await
    }

    #[cfg(windows)]
    {
        use interprocess::local_socket::{GenericNamespaced, ToNsName};

        let label = endpoint.to_string_lossy();
        let name = label.to_ns_name::<GenericNamespaced>()?;
        interprocess::local_socket::tokio::Stream::connect(name).await
    }
}

#[cfg(unix)]
fn validate_unix_socket_path(path: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    if path.as_os_str().as_bytes().len() > UNIX_SOCKET_PATH_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Unix socket path exceeds the 100-byte safety limit",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn next_request_id() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
    )
}

fn frame_size_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "control frame exceeds the 8 MiB limit",
    )
}

fn json_error(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn ipc_unavailable(_: io::Error) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::IpcUnavailable,
        "ipc",
        "the daemon IPC endpoint is unavailable",
    )
    .retryable()
}

fn ipc_unavailable_timeout() -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::IpcUnavailable,
        "ipc",
        "the daemon IPC endpoint did not respond before the timeout",
    )
    .retryable()
}

fn ipc_protocol_mismatch() -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::IpcProtocolMismatch,
        "ipc",
        "the daemon returned an invalid control response",
    )
}

fn control_read_error(error: io::Error) -> RuntimeError {
    if error.kind() == io::ErrorKind::InvalidData {
        ipc_protocol_mismatch()
    } else {
        ipc_unavailable(error)
    }
}

#[cfg(test)]
mod tests {
    use std::{io, path::Path};

    use tokio::io::{AsyncWriteExt, duplex};

    #[cfg(unix)]
    use super::UNIX_SOCKET_PATH_LIMIT;
    use super::{
        EndpointKind, EndpointSet, MAX_CONTROL_FRAME_SIZE, read_frame, read_json, write_frame,
    };

    #[test]
    fn endpoints_are_deterministic_and_distinct() {
        let first = EndpointSet::for_runtime_dir(Path::new("runtime")).unwrap();
        let second = EndpointSet::for_runtime_dir(Path::new("runtime")).unwrap();

        assert_eq!(first, second);
        assert_ne!(first.control, first.mcp);
        assert_eq!(first.address(EndpointKind::Control), first.control);
        assert_eq!(first.address(EndpointKind::Mcp), first.mcp);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_socket_paths_beyond_the_safety_limit() {
        let runtime_dir = "x".repeat(UNIX_SOCKET_PATH_LIMIT);
        let error = EndpointSet::for_runtime_dir(Path::new(&runtime_dir)).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn reads_a_fragmented_frame() {
        let (mut writer, mut reader) = duplex(64);
        let task = tokio::spawn(async move {
            for fragment in [&b"\0\0"[..], &b"\0\x05he"[..], &b"llo"[..]] {
                writer.write_all(fragment).await.unwrap();
            }
        });

        assert_eq!(read_frame(&mut reader).await.unwrap(), b"hello");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn reads_multiple_frames_from_one_stream() {
        let (mut writer, mut reader) = duplex(64);
        write_frame(&mut writer, b"first").await.unwrap();
        write_frame(&mut writer, b"second").await.unwrap();

        assert_eq!(read_frame(&mut reader).await.unwrap(), b"first");
        assert_eq!(read_frame(&mut reader).await.unwrap(), b"second");
    }

    #[tokio::test]
    async fn rejects_an_oversized_length_prefix() {
        let (mut writer, mut reader) = duplex(64);
        writer
            .write_all(&((MAX_CONTROL_FRAME_SIZE as u32) + 1).to_be_bytes())
            .await
            .unwrap();

        let error = read_frame(&mut reader).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn rejects_an_oversized_write() {
        let (mut writer, _) = duplex(64);
        let payload = vec![0; MAX_CONTROL_FRAME_SIZE + 1];

        let error = write_frame(&mut writer, &payload).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn rejects_invalid_json() {
        let (mut writer, mut reader) = duplex(64);
        write_frame(&mut writer, b"not json").await.unwrap();

        let error = read_json::<_, serde_json::Value>(&mut reader)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
