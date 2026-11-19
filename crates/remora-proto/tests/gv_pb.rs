use prost::Message as _;
use remora_proto::gv_pb::{FeatureFlag, Message, SimpleAction, message, simple_action};

/// Echo response: seq 1, ts 2, request seq 200, host FeatureFlag capture_setting 4.
#[test]
fn echo_response_matches_documented_bytes() {
    let feature_flag = FeatureFlag {
        ff_capture_setting: 4,
        ..Default::default()
    };
    let msg = Message {
        seq: 1,
        timestamp: 2,
        which: Some(message::Which::SimpleAction(SimpleAction {
            action: simple_action::ActionType::EchoResponse.into(),
            args: r#"{"seq":200}"#.into(),
            params: Some(simple_action::Params::FeatureFlag(feature_flag)),
        })),
    };

    let mut expected = vec![0x08, 0x01, 0x10, 0x02, 0x1A, 19];
    expected.extend([0x08, 0x01, 0x12, 0x0B]);
    expected.extend(br#"{"seq":200}"#);
    expected.extend([0x22, 0x02, 0x08, 0x04]);

    let encoded = msg.encode_to_vec();
    assert_eq!(encoded, expected);
    assert_eq!(Message::decode(encoded.as_slice()).unwrap(), msg);
}

#[test]
fn unknown_enum_values_are_preserved() {
    let bytes = [0x08, 0x7F];
    let action = SimpleAction::decode(bytes.as_slice()).unwrap();
    assert_eq!(action.action, 0x7F);
    assert!(simple_action::ActionType::try_from(action.action).is_err());
}
