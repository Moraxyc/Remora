//! Cloud REST API: request signing and the response envelope.

pub mod signing;
pub mod types;

pub use signing::{PARAM_PREFIX, SIGN_HEADER, SignKey, sign, verify};
pub use types::{
    ApiError, DeviceEntry, DeviceInitRequest, DeviceInitResponse, DeviceListResponse, DeviceStatus,
    LoginByQrcodeRequest, LoginResponse, QrCodeStatus, QrLoginGenResponse, QrLoginStatusRequest,
    QrLoginStatusResponse, RoomCreateResponse, RoomJoinRequest, RoomJoinResponse, UserInfoResponse,
    parse_response,
};
