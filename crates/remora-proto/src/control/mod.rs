//! Cloud REST API: request signing and the response envelope.

pub mod signing;
pub mod types;

pub use signing::{PARAM_PREFIX, SIGN_HEADER, SignKey, sign, verify};
pub use types::{
    ApiError, DeviceEntry, DeviceGroupsResponse, DeviceInitRequest, DeviceInitResponse,
    DeviceListResponse, DevicePreferences, DevicePreferencesResponse, DeviceStatus, DeviceTools,
    LoginByQrcodeRequest, LoginResponse, MacControllableRequest, ParticipantInfo, QrCodeStatus,
    QrLoginGenResponse, QrLoginStatusRequest, QrLoginStatusResponse, RoomCreateRequest,
    RoomCreateResponse, RoomJoinRequest, RoomJoinResponse, UserInfoResponse, parse_response,
};
