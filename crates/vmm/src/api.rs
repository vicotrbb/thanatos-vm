//! The control API's types and the guest agent's frames: the one definition the server, CLI,
//! agent and JSON schema share.
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
};

/// One line from a client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub body: Body,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Body {
    /// State, uptime, vCPUs, memory, devices and display mode.
    Status,
    /// `graceful` asks the agent to power off and stops the VM after `timeout` seconds regardless.
    Stop {
        #[serde(default)]
        graceful: bool,
        #[serde(default = "default_timeout")]
        timeout: u64,
    },
    /// Token for a bidirectional serial stream.
    Console,
    /// Token for a stream the VMM connects to this guest port.
    Vsock { port: u32 },
    /// Runs a command through the agent. The token's stream carries [`AgentFrame`]s.
    Exec {
        argv: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        stdin: bool,
    },
    /// Token for a stream that takes `Data` frames, ended by an empty one, and answers `Done`.
    FilePut {
        path: String,
        #[serde(default)]
        mode: Option<u32>,
    },
    /// Token for a stream that yields `Data` frames, then `Done`.
    FileGet { path: String },
}

fn default_timeout() -> u64 {
    30
}

/// One line to a client, answering the request with the same `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What the host asks of the agent on a new vsock connection, one per connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "agent", rename_all = "snake_case")]
pub enum AgentRequest {
    Exec {
        argv: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<String>,
        stdin: bool,
    },
    FilePut {
        path: String,
        mode: Option<u32>,
    },
    FileGet {
        path: String,
    },
    Poweroff,
    Heartbeat,
}

/// The frames after the request, in either direction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum AgentFrame {
    /// An empty body is end of input.
    Stdin,
    Stdout,
    Stderr,
    Exit {
        code: i32,
    },
    Data,
    Done,
    Error {
        message: String,
    },
}

/// The agent's vsock port.
pub const AGENT_PORT: u32 = 1;
const MAX_HEADER: usize = 1 << 20;
/// Largest body of one frame; a sender splits anything longer.
pub const MAX_BODY: usize = 1 << 20;

/// `u32` header length, `u32` body length (little endian), JSON header, body.
pub fn write_frame(out: &mut impl Write, header: &impl Serialize, body: &[u8]) -> io::Result<()> {
    let json = serde_json::to_vec(header)?;
    if json.len() > MAX_HEADER || body.len() > MAX_BODY {
        return Err(io::Error::other("frame too large"));
    }
    let mut frame = Vec::with_capacity(8 + json.len() + body.len());
    frame.extend_from_slice(&(json.len() as u32).to_le_bytes());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&json);
    frame.extend_from_slice(body);
    out.write_all(&frame)?;
    out.flush()
}

/// The next frame, or `None` at a clean end of stream.
pub fn read_frame<H: DeserializeOwned>(input: &mut impl Read) -> io::Result<Option<(H, Vec<u8>)>> {
    let mut lengths = [0; 8];
    match input.read_exact(&mut lengths) {
        Ok(()) => (),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let header = u32::from_le_bytes(lengths[..4].try_into().unwrap()) as usize;
    let body = u32::from_le_bytes(lengths[4..].try_into().unwrap()) as usize;
    if header > MAX_HEADER || body > MAX_BODY {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut json = vec![0; header];
    input.read_exact(&mut json)?;
    let mut data = vec![0; body];
    input.read_exact(&mut data)?;
    Ok(Some((serde_json::from_slice(&json)?, data)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_oversized_ones_are_refused() {
        let mut wire = Vec::new();
        write_frame(&mut wire, &AgentFrame::Exit { code: 3 }, b"body").unwrap();
        let mut reader = wire.as_slice();
        let (frame, body) = read_frame::<AgentFrame>(&mut reader).unwrap().unwrap();
        assert_eq!(frame, AgentFrame::Exit { code: 3 });
        assert_eq!(body, b"body");
        assert!(read_frame::<AgentFrame>(&mut reader).unwrap().is_none());
        let huge = [u32::MAX.to_le_bytes(), 0u32.to_le_bytes()].concat();
        assert!(read_frame::<AgentFrame>(&mut huge.as_slice()).is_err());
    }

    #[test]
    fn requests_are_flat_json_lines() {
        let request: Request =
            serde_json::from_str(r#"{"id":7,"request":"stop","graceful":true}"#).unwrap();
        assert_eq!(request.id, 7);
        assert_eq!(
            request.body,
            Body::Stop {
                graceful: true,
                timeout: 30
            }
        );
        assert!(serde_json::from_str::<Request>(r#"{"id":1,"request":"nope"}"#).is_err());
    }
}
