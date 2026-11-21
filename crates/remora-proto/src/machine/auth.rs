//! Sans-IO device registration and QR login state machine skeleton.

use std::collections::VecDeque;

use super::Machine;
use crate::control::ApiError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthState {
    #[default]
    Idle,
    DeviceInitPending,
    QrPending,
    Authenticated,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthInput {
    StartDeviceInit,
    DeviceInitSucceeded { device_id: String },
    RequestQrLogin,
    QrGenerated { ticket: String, jump_url: String },
    QrStatusUpdated { status: u8 },
    TokenAccepted { user_id: String },
    ApiFailed(ApiError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthOutput {
    SendDeviceInit,
    SendGenerateQr,
    PollQrStatus { ticket: String, last_status: u8 },
    RedeemQrLogin { ticket: String },
    Authenticated { user_id: String },
    NeedsRelogin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    InvalidTransition { state: AuthState },
}

#[derive(Debug, Default)]
pub struct AuthMachine {
    state: AuthState,
    outputs: VecDeque<AuthOutput>,
}

impl AuthMachine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> AuthState {
        self.state
    }
}

impl Machine for AuthMachine {
    type Input = AuthInput;
    type Output = AuthOutput;
    type Error = AuthError;

    fn handle(&mut self, input: Self::Input) -> Result<(), Self::Error> {
        match (self.state, input) {
            (AuthState::Idle, AuthInput::StartDeviceInit) => {
                self.state = AuthState::DeviceInitPending;
                self.outputs.push_back(AuthOutput::SendDeviceInit);
                Ok(())
            }
            (AuthState::DeviceInitPending, AuthInput::DeviceInitSucceeded { .. }) => {
                self.state = AuthState::Idle;
                Ok(())
            }
            (AuthState::Idle, AuthInput::RequestQrLogin) => {
                self.state = AuthState::QrPending;
                self.outputs.push_back(AuthOutput::SendGenerateQr);
                Ok(())
            }
            (AuthState::QrPending, AuthInput::QrGenerated { ticket, .. }) => {
                self.outputs.push_back(AuthOutput::PollQrStatus {
                    ticket,
                    last_status: 1,
                });
                Ok(())
            }
            (AuthState::QrPending, AuthInput::TokenAccepted { user_id }) => {
                self.state = AuthState::Authenticated;
                self.outputs
                    .push_back(AuthOutput::Authenticated { user_id });
                Ok(())
            }
            (_, AuthInput::ApiFailed(ApiError::TokenExpired)) => {
                self.state = AuthState::Expired;
                self.outputs.push_back(AuthOutput::NeedsRelogin);
                Ok(())
            }
            (state, _) => Err(AuthError::InvalidTransition { state }),
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
    fn advances_qr_login_skeleton() {
        let mut machine = AuthMachine::new();
        machine.handle(AuthInput::RequestQrLogin).unwrap();
        assert_eq!(machine.poll_output(), Some(AuthOutput::SendGenerateQr));
        machine
            .handle(AuthInput::TokenAccepted {
                user_id: "1".into(),
            })
            .unwrap();
        assert_eq!(machine.state(), AuthState::Authenticated);
        assert_eq!(
            machine.poll_output(),
            Some(AuthOutput::Authenticated {
                user_id: "1".into()
            })
        );
    }
}
