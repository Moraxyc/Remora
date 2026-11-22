//! Cloud REST API: request signing and the response envelope.

pub mod signing;
pub mod types;

pub use signing::{
    PARAM_PREFIX, SIGN_HEADER, SignKey, share_code_digest, sign, verify, verify_share_code_digest,
};
pub use types::{
    ApiError, AssistDeviceRecord, AssistDeviceRecordPage, AssistFavoriteInput,
    AssistFavoritesRequest, AssistRoomConfig, CancelRemoteAssistRequest, DeviceEntry,
    DeviceGroupsResponse, DeviceInitRequest, DeviceInitResponse, DeviceListResponse,
    DevicePreferences, DevicePreferencesResponse, DeviceShareInfoResponse, DeviceStatus,
    DeviceTools, GuestCreateResponse, JoinShareByCodeRequest, JoinShareByConfirmationRequest,
    JoinShareByConfirmationResult, LoginByQrcodeRequest, LoginResponse, MacControllableRequest,
    ParticipantInfo, QrCodeStatus, QrLoginGenResponse, QrLoginStatusRequest, QrLoginStatusResponse,
    ReleaseUpdateResponse, RoomCreateRequest, RoomCreateResponse, RoomJoinRequest,
    RoomJoinResponse, SavedShareKind, ShareConfirmationRequest, ShareControlMode,
    ShareControlModeRequest, ShareControlModeResponse, UploadControlModeRequest,
    UploadShareSignRequest, UserInfoResponse, parse_join_confirmation_response, parse_response,
};
