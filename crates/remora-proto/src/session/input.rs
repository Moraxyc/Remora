//! JSON input messages sent on `CONTROL_DATA_CHANNEL`.

use std::fmt;

use serde_json::{Value, json};

/// Windows `MK_*` wire masks for mouse buttons, plus the legacy middle-button code.
pub mod mouse_button {
    pub const LEFT: i32 = 1;
    pub const RIGHT: i32 = 2;
    pub const MIDDLE: i32 = 16;
    pub const LEGACY_MIDDLE: i32 = 4;
    pub const XBUTTON1: i32 = 32;
    pub const XBUTTON2: i32 = 64;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    XButton1,
    XButton2,
    Unknown(i32),
}

impl MouseButton {
    pub const fn from_wire(code: i32) -> Self {
        match code {
            mouse_button::LEFT => Self::Left,
            mouse_button::RIGHT => Self::Right,
            mouse_button::MIDDLE | mouse_button::LEGACY_MIDDLE => Self::Middle,
            mouse_button::XBUTTON1 => Self::XButton1,
            mouse_button::XBUTTON2 => Self::XButton2,
            other => Self::Unknown(other),
        }
    }

    pub const fn to_wire(self) -> i32 {
        match self {
            Self::Left => mouse_button::LEFT,
            Self::Right => mouse_button::RIGHT,
            Self::Middle => mouse_button::MIDDLE,
            Self::XButton1 => mouse_button::XBUTTON1,
            Self::XButton2 => mouse_button::XBUTTON2,
            Self::Unknown(code) => code,
        }
    }
}

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

impl InputMessage {
    pub const fn mouse_button(button: MouseButton, pressed: bool) -> Self {
        Self::MouseButton {
            button: button.to_wire(),
            pressed,
        }
    }

    pub const fn mouse_click(button: MouseButton) -> Self {
        Self::MouseClick {
            button: button.to_wire(),
        }
    }

    pub const fn decoded_mouse_button(&self) -> Option<MouseButton> {
        match self {
            Self::MouseButton { button, .. } | Self::MouseClick { button } => {
                Some(MouseButton::from_wire(*button))
            }
            _ => None,
        }
    }
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
    fn maps_mouse_button_codes_and_preserves_raw_messages() {
        for (code, button, canonical) in [
            (1, MouseButton::Left, 1),
            (2, MouseButton::Right, 2),
            (16, MouseButton::Middle, 16),
            (4, MouseButton::Middle, 16),
            (32, MouseButton::XButton1, 32),
            (64, MouseButton::XButton2, 64),
            (128, MouseButton::Unknown(128), 128),
        ] {
            assert_eq!(MouseButton::from_wire(code), button);
            assert_eq!(button.to_wire(), canonical);
            let raw_click = InputMessage::MouseClick { button: code };
            assert_eq!(raw_click.decoded_mouse_button(), Some(button));
            assert_eq!(decode_input(&encode_input(&raw_click)).unwrap(), raw_click);
            assert_eq!(
                InputMessage::mouse_click(button),
                InputMessage::MouseClick { button: canonical }
            );
            let press = InputMessage::mouse_button(button, true);
            assert_eq!(press.decoded_mouse_button(), Some(button));
            assert_eq!(
                decode_input(&encode_input(&press)).unwrap(),
                InputMessage::MouseButton {
                    button: canonical,
                    pressed: true,
                }
            );
        }
        assert_eq!(InputMessage::KeyboardHeartbeat.decoded_mouse_button(), None);
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
