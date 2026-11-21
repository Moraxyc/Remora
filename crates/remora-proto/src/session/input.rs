//! JSON input messages sent on `CONTROL_DATA_CHANNEL`.

use std::fmt;

use serde_json::{Value, json};

/// Decoded JSON input message on `CONTROL_DATA_CHANNEL`.
///
/// Unknown `action` names or unrecognized JSON objects decode into
/// [`InputMessage::Unknown`] without failing. Note that `Debug` redacts typed
/// text so keystrokes never reach logs.
#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub enum InputMessage {
    KeyboardHeartbeat,
    Keyboard { key: i32, pressed: bool },
    KeyboardClick { key: i32 },
    MouseButton { button: i32, pressed: bool },
    MouseClick { button: i32 },
    MouseAbsolute { x: f64, y: f64 },
    MouseRelative { dx: i32, dy: i32 },
    MouseScroll { dx: i32, dy: i32 },
    TextInput { text: String },
    Unknown { name: String, raw: Value },
}

impl fmt::Debug for InputMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KeyboardHeartbeat => f.write_str("KeyboardHeartbeat"),
            Self::Keyboard { pressed, .. } => f
                .debug_struct("Keyboard")
                .field("pressed", pressed)
                .finish(),
            Self::KeyboardClick { .. } => f.write_str("KeyboardClick"),
            Self::MouseButton { button, pressed } => f
                .debug_struct("MouseButton")
                .field("button", button)
                .field("pressed", pressed)
                .finish(),
            Self::MouseClick { button } => f
                .debug_struct("MouseClick")
                .field("button", button)
                .finish(),
            Self::MouseAbsolute { x, y } => f
                .debug_struct("MouseAbsolute")
                .field("x", x)
                .field("y", y)
                .finish(),
            Self::MouseRelative { dx, dy } => f
                .debug_struct("MouseRelative")
                .field("dx", dx)
                .field("dy", dy)
                .finish(),
            Self::MouseScroll { dx, dy } => f
                .debug_struct("MouseScroll")
                .field("dx", dx)
                .field("dy", dy)
                .finish(),
            Self::TextInput { text } => f
                .debug_struct("TextInput")
                .field("len", &text.len())
                .finish(),
            Self::Unknown { name, .. } => f.debug_struct("Unknown").field("name", name).finish(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InputDecodeError {
    InvalidJson,
    InvalidField(&'static str),
}

impl fmt::Display for InputDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson => f.write_str("input payload is not valid JSON"),
            Self::InvalidField(field) => write!(f, "invalid input field: {field}"),
        }
    }
}

impl std::error::Error for InputDecodeError {}

fn field_i32(v: &Value, name: &'static str) -> Result<i32, InputDecodeError> {
    v.get(name)
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
        .ok_or(InputDecodeError::InvalidField(name))
}

fn field_f64(v: &Value, name: &'static str) -> Result<f64, InputDecodeError> {
    v.get(name)
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite())
        .ok_or(InputDecodeError::InvalidField(name))
}

/// Decodes a JSON input message from wire bytes.
///
/// Unrecognized `action` names or objects without `action`/`heartbeat` return
/// [`InputMessage::Unknown`] instead of an error; extra unknown fields are ignored.
pub fn decode_input(bytes: &[u8]) -> Result<InputMessage, InputDecodeError> {
    let v: Value = serde_json::from_slice(bytes).map_err(|_| InputDecodeError::InvalidJson)?;
    if v.get("heartbeat").and_then(Value::as_str) == Some("kbd_heartbeat") {
        return Ok(InputMessage::KeyboardHeartbeat);
    }
    let Some(action) = v.get("action").and_then(Value::as_str) else {
        let name = v
            .get("heartbeat")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        return Ok(InputMessage::Unknown { name, raw: v });
    };

    Ok(match action {
        "kbd_press" => InputMessage::Keyboard {
            key: field_i32(&v, "key")?,
            pressed: true,
        },
        "kbd_release" => InputMessage::Keyboard {
            key: field_i32(&v, "key")?,
            pressed: false,
        },
        "kbd_click" => InputMessage::KeyboardClick {
            key: field_i32(&v, "key")?,
        },
        "mouse_press" => InputMessage::MouseButton {
            button: field_i32(&v, "button")?,
            pressed: true,
        },
        "mouse_release" => InputMessage::MouseButton {
            button: field_i32(&v, "button")?,
            pressed: false,
        },
        "mouse_click" => InputMessage::MouseClick {
            button: field_i32(&v, "button")?,
        },
        "mouse_move_absolute" => InputMessage::MouseAbsolute {
            x: field_f64(&v, "abs_x")?,
            y: field_f64(&v, "abs_y")?,
        },
        "mouse_move_relative" => InputMessage::MouseRelative {
            dx: field_i32(&v, "delta_x")?,
            dy: field_i32(&v, "delta_y")?,
        },
        "mouse_scroll" => InputMessage::MouseScroll {
            dx: field_i32(&v, "delta_x")?,
            dy: field_i32(&v, "delta_y")?,
        },
        "text_input" => {
            let text = v
                .get("content")
                .and_then(Value::as_str)
                .ok_or(InputDecodeError::InvalidField("content"))?;
            InputMessage::TextInput {
                text: text.to_owned(),
            }
        }
        other => InputMessage::Unknown {
            name: other.to_owned(),
            raw: v,
        },
    })
}

/// Encodes an [`InputMessage`] into wire JSON bytes.
pub fn encode_input(msg: &InputMessage) -> Vec<u8> {
    let val = match msg {
        InputMessage::KeyboardHeartbeat => json!({ "heartbeat": "kbd_heartbeat" }),
        InputMessage::Keyboard { key, pressed } => json!({
            "action": if *pressed { "kbd_press" } else { "kbd_release" },
            "key": key,
        }),
        InputMessage::KeyboardClick { key } => json!({
            "action": "kbd_click",
            "key": key,
        }),
        InputMessage::MouseButton { button, pressed } => json!({
            "action": if *pressed { "mouse_press" } else { "mouse_release" },
            "button": button,
        }),
        InputMessage::MouseClick { button } => json!({
            "action": "mouse_click",
            "button": button,
        }),
        InputMessage::MouseAbsolute { x, y } => json!({
            "action": "mouse_move_absolute",
            "abs_x": x,
            "abs_y": y,
        }),
        InputMessage::MouseRelative { dx, dy } => json!({
            "action": "mouse_move_relative",
            "delta_x": dx,
            "delta_y": dy,
        }),
        InputMessage::MouseScroll { dx, dy } => json!({
            "action": "mouse_scroll",
            "delta_x": dx,
            "delta_y": dy,
        }),
        InputMessage::TextInput { text } => json!({
            "action": "text_input",
            "content": text,
        }),
        InputMessage::Unknown { raw, .. } => raw.clone(),
    };
    serde_json::to_vec(&val).expect("input message serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_input_messages() {
        let cases = [
            InputMessage::KeyboardHeartbeat,
            InputMessage::Keyboard {
                key: 65,
                pressed: true,
            },
            InputMessage::Keyboard {
                key: 65,
                pressed: false,
            },
            InputMessage::MouseButton {
                button: 1,
                pressed: true,
            },
            InputMessage::MouseAbsolute { x: 0.5, y: 0.25 },
            InputMessage::MouseRelative { dx: -4, dy: 5 },
            InputMessage::MouseScroll { dx: 0, dy: 120 },
            InputMessage::TextInput {
                text: "hello".into(),
            },
        ];
        for msg in cases {
            let bytes = encode_input(&msg);
            assert_eq!(decode_input(&bytes).unwrap(), msg);
        }
    }

    #[test]
    fn tolerates_unknown_actions_and_extra_fields() {
        let unknown = br#"{"action":"future_gesture","fingers":3}"#;
        assert_eq!(
            decode_input(unknown).unwrap(),
            InputMessage::Unknown {
                name: "future_gesture".into(),
                raw: json!({"action": "future_gesture", "fingers": 3}),
            }
        );

        let with_extra = br#"{"action":"kbd_press","key":65,"extra_field":"ignored"}"#;
        assert_eq!(
            decode_input(with_extra).unwrap(),
            InputMessage::Keyboard {
                key: 65,
                pressed: true,
            }
        );
    }

    #[test]
    fn debug_redacts_text_input() {
        let msg = InputMessage::TextInput {
            text: "super-secret-password".into(),
        };
        assert!(!format!("{msg:?}").contains("super-secret-password"));
    }
}
