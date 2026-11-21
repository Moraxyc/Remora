//! Sans-IO per-connection peer session state machine skeleton.

use std::collections::VecDeque;

use super::Machine;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PeerState {
    #[default]
    Idle,
    Negotiating,
    Connected,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerInput {
    Start { ice_id: String },
    IceConnected,
    ControlChannelOpened,
    Release,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerOutput {
    SendForwardSetting { ice_id: String },
    SendIceFinishedLog { ice_id: String },
    SendControlInitBurst { ice_id: String },
    ClosePeer { ice_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerError {
    MissingIceId,
}

#[derive(Debug, Default)]
pub struct PeerMachine {
    state: PeerState,
    ice_id: Option<String>,
    outputs: VecDeque<PeerOutput>,
}

impl PeerMachine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> PeerState {
        self.state
    }
}

impl Machine for PeerMachine {
    type Input = PeerInput;
    type Output = PeerOutput;
    type Error = PeerError;

    fn handle(&mut self, input: Self::Input) -> Result<(), Self::Error> {
        match input {
            PeerInput::Start { ice_id } => {
                self.ice_id = Some(ice_id);
                self.state = PeerState::Negotiating;
                Ok(())
            }
            PeerInput::IceConnected => {
                let ice_id = self.ice_id.clone().ok_or(PeerError::MissingIceId)?;
                self.state = PeerState::Connected;
                self.outputs.push_back(PeerOutput::SendForwardSetting {
                    ice_id: ice_id.clone(),
                });
                self.outputs
                    .push_back(PeerOutput::SendIceFinishedLog { ice_id });
                Ok(())
            }
            PeerInput::ControlChannelOpened => {
                let ice_id = self.ice_id.clone().ok_or(PeerError::MissingIceId)?;
                self.outputs
                    .push_back(PeerOutput::SendControlInitBurst { ice_id });
                Ok(())
            }
            PeerInput::Release => {
                self.state = PeerState::Closed;
                if let Some(ice_id) = self.ice_id.take() {
                    self.outputs.push_back(PeerOutput::ClosePeer { ice_id });
                }
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
    fn emits_connected_and_control_burst_outputs() {
        let mut peer = PeerMachine::new();
        peer.handle(PeerInput::Start {
            ice_id: "ice-1".into(),
        })
        .unwrap();
        peer.handle(PeerInput::IceConnected).unwrap();
        assert_eq!(peer.state(), PeerState::Connected);
        assert_eq!(
            peer.poll_output(),
            Some(PeerOutput::SendForwardSetting {
                ice_id: "ice-1".into()
            })
        );
        assert_eq!(
            peer.poll_output(),
            Some(PeerOutput::SendIceFinishedLog {
                ice_id: "ice-1".into()
            })
        );
        peer.handle(PeerInput::Release).unwrap();
        assert_eq!(peer.state(), PeerState::Closed);
    }
}
