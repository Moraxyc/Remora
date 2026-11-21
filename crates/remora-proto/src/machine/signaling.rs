//! Sans-IO Socket.IO signaling state machine skeleton.

use std::collections::VecDeque;

use super::Machine;
use crate::signal::{CONNECT, Handshake, PING, PONG, Packet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SignalingState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    RoomReady,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SignalingInput {
    Connect,
    Packet(Packet),
    TickMillis(u64),
    Disconnect,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SignalingOutput {
    SendText(String),
    EmitEvent { name: String },
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalingError {
    UnexpectedPacket,
}

#[derive(Debug, Default)]
pub struct SignalingMachine {
    state: SignalingState,
    handshake: Option<Handshake>,
    outputs: VecDeque<SignalingOutput>,
}

impl SignalingMachine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> SignalingState {
        self.state
    }

    pub fn handshake(&self) -> Option<&Handshake> {
        self.handshake.as_ref()
    }
}

impl Machine for SignalingMachine {
    type Input = SignalingInput;
    type Output = SignalingOutput;
    type Error = SignalingError;

    fn handle(&mut self, input: Self::Input) -> Result<(), Self::Error> {
        match input {
            SignalingInput::Connect => {
                self.state = SignalingState::Connecting;
                Ok(())
            }
            SignalingInput::Packet(Packet::Open(handshake)) => {
                self.handshake = Some(handshake);
                self.outputs
                    .push_back(SignalingOutput::SendText(CONNECT.to_owned()));
                Ok(())
            }
            SignalingInput::Packet(Packet::Connected) => {
                self.state = SignalingState::Connected;
                Ok(())
            }
            SignalingInput::Packet(Packet::Ping) => {
                self.outputs
                    .push_back(SignalingOutput::SendText(PONG.to_owned()));
                Ok(())
            }
            SignalingInput::Packet(Packet::Pong) => Ok(()),
            SignalingInput::Packet(Packet::Event(event)) => {
                self.outputs
                    .push_back(SignalingOutput::EmitEvent { name: event.name });
                Ok(())
            }
            SignalingInput::Packet(Packet::Close | Packet::Disconnected)
            | SignalingInput::Disconnect => {
                self.state = SignalingState::Disconnected;
                self.outputs.push_back(SignalingOutput::Closed);
                Ok(())
            }
            SignalingInput::Packet(_) => Ok(()),
            SignalingInput::TickMillis(_) => {
                let _ = PING;
                Ok(())
            }
        }
    }

    fn poll_output(&mut self) -> Option<Self::Output> {
        self.outputs.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responds_to_open_and_ping() {
        let mut machine = SignalingMachine::new();
        machine.handle(SignalingInput::Connect).unwrap();
        machine
            .handle(SignalingInput::Packet(Packet::Open(Handshake {
                sid: "s1".into(),
                ping_interval: 25_000,
                ping_timeout: 20_000,
            })))
            .unwrap();
        assert_eq!(
            machine.poll_output(),
            Some(SignalingOutput::SendText(CONNECT.into()))
        );

        machine
            .handle(SignalingInput::Packet(Packet::Connected))
            .unwrap();
        assert_eq!(machine.state(), SignalingState::Connected);

        machine
            .handle(SignalingInput::Packet(Packet::Ping))
            .unwrap();
        assert_eq!(
            machine.poll_output(),
            Some(SignalingOutput::SendText(PONG.into()))
        );
    }
}
