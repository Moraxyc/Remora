//! Wire-level error codes returned in the `{code, msg, data}` cloud envelope.

use std::fmt;

/// Numeric status code carried in the cloud JSON response envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorCode {
    /// Success (`0`).
    Ok,
    /// Server-side internal error (`1000`).
    Internal,
    /// Missing, malformed, or invalid request payload (`1001`).
    InvalidRequest,
    /// Missing or invalid `x-param-sign` signature (`1002`).
    InvalidSignature,
    /// User token is missing, invalid, or expired (`1120`).
    TokenExpired,
    /// Login QR ticket is unknown, expired, or already used (`1190`).
    QrCodeExpired,
    /// Remote assistance cannot be initiated (e.g. disabled or same-account target) (`1130`).
    CannotInitiateRemoteConnection,
    /// Incorrect share device ID or verification code (`1131`).
    IncorrectShareCode,
    /// Remote assistance confirmation was denied by the host (`1134`).
    ShareConfirmationDenied,
    /// Remote assistance is waiting for confirmation from the host (`1136`).
    ShareConfirmationRequired,
    /// Cannot favorite a device owned by the same account (`1137`).
    SameAccountShareFavorite,
    /// No client update available (`1151`).
    ClientUpToDate,
    /// No configuration update available (`1182`).
    ConfigUpToDate,
    /// Rate limit exceeded (`1429`).
    TooManyRequests,
    /// Target host is not connected to signaling (`2001`).
    DeviceOffline,
    /// Target host already has an active desktop controller (`2002`).
    BeingControlled,
    /// Device does not exist or is not bound to the caller (`2003`).
    UnknownDevice,
    /// Guest share info requested before creating a guest room (`2004`).
    GuestRoomRequired,
    /// Any unrecognized numeric code from the wire.
    Unknown(i64),
}

impl ErrorCode {
    /// Parses a raw wire code, preserving unrecognized numbers in [`ErrorCode::Unknown`].
    pub const fn from_code(code: i64) -> Self {
        match code {
            0 => Self::Ok,
            1000 => Self::Internal,
            1001 => Self::InvalidRequest,
            1002 => Self::InvalidSignature,
            1120 => Self::TokenExpired,
            1130 => Self::CannotInitiateRemoteConnection,
            1131 => Self::IncorrectShareCode,
            1134 => Self::ShareConfirmationDenied,
            1136 => Self::ShareConfirmationRequired,
            1137 => Self::SameAccountShareFavorite,
            1151 => Self::ClientUpToDate,
            1182 => Self::ConfigUpToDate,
            1190 => Self::QrCodeExpired,
            1429 => Self::TooManyRequests,
            2001 => Self::DeviceOffline,
            2002 => Self::BeingControlled,
            2003 => Self::UnknownDevice,
            2004 => Self::GuestRoomRequired,
            other => Self::Unknown(other),
        }
    }

    /// The numeric wire value.
    pub const fn code(self) -> i64 {
        match self {
            Self::Ok => 0,
            Self::Internal => 1000,
            Self::InvalidRequest => 1001,
            Self::InvalidSignature => 1002,
            Self::TokenExpired => 1120,
            Self::CannotInitiateRemoteConnection => 1130,
            Self::IncorrectShareCode => 1131,
            Self::ShareConfirmationDenied => 1134,
            Self::ShareConfirmationRequired => 1136,
            Self::SameAccountShareFavorite => 1137,
            Self::ClientUpToDate => 1151,
            Self::ConfigUpToDate => 1182,
            Self::QrCodeExpired => 1190,
            Self::TooManyRequests => 1429,
            Self::DeviceOffline => 2001,
            Self::BeingControlled => 2002,
            Self::UnknownDevice => 2003,
            Self::GuestRoomRequired => 2004,
            Self::Unknown(code) => code,
        }
    }

    pub const fn is_ok(self) -> bool {
        matches!(self, Self::Ok)
    }
}

impl From<i64> for ErrorCode {
    fn from(code: i64) -> Self {
        Self::from_code(code)
    }
}

impl From<ErrorCode> for i64 {
    fn from(code: ErrorCode) -> Self {
        code.code()
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok => f.write_str("ok (0)"),
            Self::Internal => f.write_str("internal error (1000)"),
            Self::InvalidRequest => f.write_str("invalid request (1001)"),
            Self::InvalidSignature => f.write_str("invalid signature (1002)"),
            Self::TokenExpired => f.write_str("token expired (1120)"),
            Self::CannotInitiateRemoteConnection => {
                f.write_str("cannot initiate remote connection (1130)")
            }
            Self::IncorrectShareCode => f.write_str("incorrect device id or code (1131)"),
            Self::ShareConfirmationDenied => f.write_str("share confirmation denied (1134)"),
            Self::ShareConfirmationRequired => f.write_str("share confirmation required (1136)"),
            Self::SameAccountShareFavorite => {
                f.write_str("cannot favorite same-account device (1137)")
            }
            Self::ClientUpToDate => f.write_str("client is up to date (1151)"),
            Self::ConfigUpToDate => f.write_str("configuration is up to date (1182)"),
            Self::QrCodeExpired => f.write_str("qr code expired (1190)"),
            Self::TooManyRequests => f.write_str("too many requests (1429)"),
            Self::DeviceOffline => f.write_str("device offline (2001)"),
            Self::BeingControlled => f.write_str("being controlled (2002)"),
            Self::UnknownDevice => f.write_str("unknown device (2003)"),
            Self::GuestRoomRequired => f.write_str("guest room required (2004)"),
            Self::Unknown(code) => write!(f, "unknown code ({code})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_known_and_unknown_codes() {
        for code in [
            0, 1000, 1001, 1002, 1120, 1130, 1131, 1134, 1136, 1137, 1151, 1182, 1190, 1429, 2001,
            2002, 2003, 2004,
        ] {
            let parsed = ErrorCode::from_code(code);
            assert!(!matches!(parsed, ErrorCode::Unknown(_)));
            assert_eq!(parsed.code(), code);
        }
        assert_eq!(ErrorCode::from_code(9999), ErrorCode::Unknown(9999));
        assert_eq!(ErrorCode::Unknown(9999).code(), 9999);
    }
}
