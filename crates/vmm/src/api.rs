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
    /// The host's copy of the scanout as last flushed by the guest. The reply has a token for a
    /// stream of the image, then its `width` and `height`. `raw` is RGBA.
    Screenshot {
        #[serde(default = "default_format")]
        format: ImageFormat,
    },
    /// Injects input into the guest's virtio-input devices, in order.
    Input { events: Vec<InputEvent> },
    /// Switches the display sink. `vnc` applies to the `vnc` mode.
    Display {
        mode: String,
        #[serde(default)]
        vnc: Option<VncOptions>,
    },
    /// Opens a port forward such as `127.0.0.1:2222->22`.
    ForwardAdd { spec: String },
    /// Closes a port forward by the same text.
    ForwardRemove { spec: String },
    /// Subscribes this connection to [`Event`] lines.
    Events,
}

fn default_format() -> ImageFormat {
    ImageFormat::Png
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageFormat {
    Png,
    Raw,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VncOptions {
    /// Defaults to the configured `vnc_bind`.
    #[serde(default)]
    pub bind: Option<String>,
    /// Required for an address other machines can reach.
    #[serde(default)]
    pub password: Option<String>,
}

/// One input action, in screen pixels and Linux keycodes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputEvent {
    Key {
        code: u16,
        down: bool,
    },
    PointerMove {
        x: u32,
        y: u32,
    },
    /// 0 left, 1 middle, 2 right.
    Button {
        button: u8,
        down: bool,
    },
    /// Wheel clicks, positive up.
    Scroll {
        clicks: i32,
    },
}

/// An unsolicited line to a connection that sent `events`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Boot,
    Shutdown { status: String },
    NetworkDenied { destination: String },
    DisplayMode { mode: String },
    Agent { alive: bool },
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

/// The JSON Schema of the API, for consumers in other languages. `docs/api.schema.json` is this,
/// checked in; serde has no reflection, so a test holds it to the types with one sample per
/// variant, in a `match` the compiler keeps exhaustive.
pub fn schema() -> serde_json::Value {
    use serde_json::{Value, json};
    fn object(tag: &str, name: &str, fields: &[(&str, Value, bool)]) -> Value {
        let mut properties = serde_json::Map::new();
        properties.insert(tag.into(), json!({ "const": name }));
        let mut required = vec![tag.to_string()];
        for (field, schema, needed) in fields {
            properties.insert((*field).into(), schema.clone());
            if *needed {
                required.push((*field).into());
            }
        }
        json!({ "type": "object", "properties": properties, "required": required })
    }
    let string = json!({ "type": "string" });
    let integer = json!({ "type": "integer", "minimum": 0 });
    let boolean = json!({ "type": "boolean" });
    let optional_string = json!({ "type": ["string", "null"] });
    let token_for = |name: &str, fields: &[(&str, Value, bool)]| object("request", name, fields);
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Thanatos control API",
        "$defs": {
            "request": {
                "description": "One line from a client. Every request also carries an integer id.",
                "oneOf": [
                    token_for("status", &[]),
                    token_for("stop", &[("graceful", boolean.clone(), false), ("timeout", integer.clone(), false)]),
                    token_for("console", &[]),
                    token_for("vsock", &[("port", integer.clone(), true)]),
                    token_for("exec", &[
                        ("argv", json!({ "type": "array", "items": string }), true),
                        ("env", json!({ "type": "object", "additionalProperties": string }), false),
                        ("cwd", optional_string.clone(), false),
                        ("stdin", boolean.clone(), false),
                    ]),
                    token_for("file_put", &[("path", string.clone(), true), ("mode", json!({ "type": ["integer", "null"] }), false)]),
                    token_for("file_get", &[("path", string.clone(), true)]),
                    token_for("screenshot", &[("format", json!({ "enum": ["png", "raw"] }), false)]),
                    token_for("input", &[("events", json!({ "type": "array", "items": { "$ref": "#/$defs/input_event" } }), true)]),
                    token_for("display", &[
                        ("mode", json!({ "enum": ["headless", "vnc", "window"] }), true),
                        ("vnc", json!({
                            "type": ["object", "null"],
                            "properties": { "bind": optional_string, "password": optional_string },
                        }), false),
                    ]),
                    token_for("forward_add", &[("spec", string.clone(), true)]),
                    token_for("forward_remove", &[("spec", string.clone(), true)]),
                    token_for("events", &[]),
                ],
            },
            "input_event": {
                "oneOf": [
                    object("kind", "key", &[("code", integer.clone(), true), ("down", boolean.clone(), true)]),
                    object("kind", "pointer_move", &[("x", integer.clone(), true), ("y", integer.clone(), true)]),
                    object("kind", "button", &[("button", integer.clone(), true), ("down", boolean.clone(), true)]),
                    object("kind", "scroll", &[("clicks", json!({ "type": "integer" }), true)]),
                ],
            },
            "reply": {
                "description": "One line per request, with its id: a result or an error.",
                "type": "object",
                "properties": { "id": integer, "result": {}, "error": string },
                "required": ["id"],
            },
            "event": {
                "oneOf": [
                    object("event", "boot", &[]),
                    object("event", "shutdown", &[("status", string.clone(), true)]),
                    object("event", "network_denied", &[("destination", string.clone(), true)]),
                    object("event", "display_mode", &[("mode", string.clone(), true)]),
                    object("event", "agent", &[("alive", boolean, true)]),
                ],
            },
            "attach": {
                "description": "The first line of a connection opening a binary channel; raw bytes follow.",
                "type": "object",
                "properties": { "attach": string },
                "required": ["attach"],
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Does `value` match one branch of a `oneOf` by its tag, with every field known and every
    /// required field present?
    fn matches_branch(value: &Value, tag: &str, branches: &Value) -> bool {
        branches["oneOf"].as_array().unwrap().iter().any(|branch| {
            let properties = branch["properties"].as_object().unwrap();
            properties[tag]["const"] == value[tag]
                && value
                    .as_object()
                    .unwrap()
                    .keys()
                    .all(|key| key == "id" || properties.contains_key(key))
                && branch["required"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|key| value.get(key.as_str().unwrap()).is_some())
        })
    }

    #[test]
    fn the_schema_is_current_and_describes_every_type() {
        let schema = schema();
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/api.schema.json");
        let text = serde_json::to_string_pretty(&schema).unwrap() + "\n";
        if std::env::var_os("UPDATE_SCHEMA").is_some() {
            std::fs::write(path, &text).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            text,
            "run with UPDATE_SCHEMA=1"
        );

        let requests = [
            Body::Status,
            Body::Stop {
                graceful: true,
                timeout: 1,
            },
            Body::Console,
            Body::Vsock { port: 1 },
            Body::Exec {
                argv: vec!["x".into()],
                env: BTreeMap::new(),
                cwd: None,
                stdin: true,
            },
            Body::FilePut {
                path: "x".into(),
                mode: None,
            },
            Body::FileGet { path: "x".into() },
            Body::Screenshot {
                format: ImageFormat::Raw,
            },
            Body::Input {
                events: vec![InputEvent::Scroll { clicks: -1 }],
            },
            Body::Display {
                mode: "vnc".into(),
                vnc: None,
            },
            Body::ForwardAdd { spec: "x".into() },
            Body::ForwardRemove { spec: "x".into() },
            Body::Events,
        ];
        for body in requests {
            // Adding a variant stops this compiling until it has a sample above.
            match body {
                Body::Status
                | Body::Stop { .. }
                | Body::Console
                | Body::Vsock { .. }
                | Body::Exec { .. }
                | Body::FilePut { .. }
                | Body::FileGet { .. }
                | Body::Screenshot { .. }
                | Body::Input { .. }
                | Body::Display { .. }
                | Body::ForwardAdd { .. }
                | Body::ForwardRemove { .. }
                | Body::Events => (),
            }
            let value = serde_json::to_value(&Request { id: 1, body }).unwrap();
            assert!(
                matches_branch(&value, "request", &schema["$defs"]["request"]),
                "{value}"
            );
        }
        let inputs = [
            InputEvent::Key {
                code: 1,
                down: true,
            },
            InputEvent::PointerMove { x: 1, y: 1 },
            InputEvent::Button {
                button: 0,
                down: true,
            },
            InputEvent::Scroll { clicks: 1 },
        ];
        for input in inputs {
            let value = serde_json::to_value(input).unwrap();
            assert!(
                matches_branch(&value, "kind", &schema["$defs"]["input_event"]),
                "{value}"
            );
        }
        let events = [
            Event::Boot,
            Event::Shutdown { status: "x".into() },
            Event::NetworkDenied {
                destination: "x".into(),
            },
            Event::DisplayMode { mode: "x".into() },
            Event::Agent { alive: true },
        ];
        for event in events {
            let value = serde_json::to_value(event).unwrap();
            assert!(
                matches_branch(&value, "event", &schema["$defs"]["event"]),
                "{value}"
            );
        }
    }

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
