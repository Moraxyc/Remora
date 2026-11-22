//! Wake-on-LAN. Hosts report their LAN identity after logging in; to wake an offline device,
//! the cloud asks an online device of the same user on the same IPv4 network to send the
//! magic packet, since broadcasts reach only the sender's LAN.

use std::fmt;
use std::net::Ipv4Addr;

use axum::extract::State;
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Database;
use crate::api::device::{CallerDevice, Devices};
use crate::api::user::User;
use crate::api::{self, ApiError, Body};
use crate::room::RoomClient;

/// A device's LAN identity. Its `Debug` hides the addresses.
#[derive(Clone, PartialEq, Eq)]
pub struct LanReport {
    pub mac: [u8; 6],
    pub ip: Ipv4Addr,
    pub subnet_mask: Ipv4Addr,
}

impl fmt::Debug for LanReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LanReport(..)")
    }
}

impl LanReport {
    /// The IPv4 network the device is on.
    fn network(&self) -> (u32, u32) {
        let mask = self.subnet_mask.to_bits();
        (self.ip.to_bits() & mask, mask)
    }

    fn mac_text(&self) -> String {
        let bytes: Vec<String> = self.mac.iter().map(|byte| format!("{byte:02x}")).collect();
        bytes.join(":")
    }

    /// `target_device` of the `wake_on_lan` push.
    fn target_device(&self) -> Value {
        json!({
            "mac": self.mac_text(),
            "ip": self.ip.to_string(),
            "subnet_mask": self.subnet_mask.to_string(),
        })
    }

    fn from_row(row: &Row, start: usize) -> rusqlite::Result<Option<Self>> {
        let mac: String = row.get(start)?;
        let ip: String = row.get(start + 1)?;
        let subnet_mask: String = row.get(start + 2)?;
        Ok(Self::parse(&mac, &ip, &subnet_mask))
    }

    fn parse(mac: &str, ip: &str, subnet_mask: &str) -> Option<Self> {
        let subnet_mask: Ipv4Addr = subnet_mask.parse().ok()?;
        let mask = subnet_mask.to_bits();
        if mask == 0 || mask.leading_ones() + mask.trailing_zeros() != 32 {
            return None;
        }
        Some(Self {
            mac: parse_mac(mac)?,
            ip: ip.parse().ok()?,
            subnet_mask,
        })
    }
}

/// Six hex bytes separated by `:` or `-`.
fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let separator = if text.contains('-') { '-' } else { ':' };
    let mut mac = [0; 6];
    let mut parts = text.split(separator);
    for byte in &mut mac {
        let part = parts.next()?;
        if part.len() != 2 {
            return None;
        }
        *byte = u8::from_str_radix(part, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

/// Devices' last LAN reports.
#[derive(Debug, Clone)]
pub struct LanReports(Database);

impl From<Database> for LanReports {
    fn from(database: Database) -> Self {
        Self(database)
    }
}

impl LanReports {
    fn store(&self, device_id: &str, report: &LanReport) -> rusqlite::Result<()> {
        self.0.connection().execute(
            "INSERT INTO lan_reports (device_id, mac, ip, subnet_mask) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (device_id) DO UPDATE
             SET mac = excluded.mac, ip = excluded.ip, subnet_mask = excluded.subnet_mask",
            params![
                device_id,
                report.mac_text(),
                report.ip.to_string(),
                report.subnet_mask.to_string()
            ],
        )?;
        Ok(())
    }

    /// The reports of `user_id`'s devices, by device id in order.
    pub fn of_user(&self, user_id: &str) -> rusqlite::Result<Vec<(String, LanReport)>> {
        let connection = self.0.connection();
        let mut statement = connection.prepare_cached(
            "SELECT lan_reports.device_id, lan_reports.mac, lan_reports.ip, lan_reports.subnet_mask
             FROM lan_reports JOIN devices ON devices.id = lan_reports.device_id
             WHERE devices.owner = ?1
             ORDER BY lan_reports.device_id",
        )?;
        let rows = statement.query_map([user_id], |row| {
            let device_id: String = row.get(0)?;
            Ok(LanReport::from_row(row, 1)?.map(|report| (device_id, report)))
        })?;
        rows.filter_map(Result::transpose).collect()
    }

    /// Identifiers of `user_id`'s devices that have reported a LAN identity.
    pub fn reported_devices(
        &self,
        user_id: &str,
    ) -> rusqlite::Result<std::collections::HashSet<String>> {
        let connection = self.0.connection();
        let mut statement = connection.prepare_cached(
            "SELECT lan_reports.device_id
             FROM lan_reports JOIN devices ON devices.id = lan_reports.device_id
             WHERE devices.owner = ?1",
        )?;
        let rows = statement.query_map([user_id], |row| row.get(0))?;
        rows.collect()
    }

    /// Whether `device_id` has reported a LAN identity.
    pub fn has_report(&self, device_id: &str) -> rusqlite::Result<bool> {
        self.0
            .connection()
            .query_row(
                "SELECT 1 FROM lan_reports WHERE device_id = ?1",
                [device_id],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
    }

    /// The report of `device_id`, if it belongs to `user_id`.
    fn of_owned(&self, device_id: &str, user_id: &str) -> rusqlite::Result<Option<LanReport>> {
        self.0
            .connection()
            .query_row(
                "SELECT lan_reports.mac, lan_reports.ip, lan_reports.subnet_mask
                 FROM lan_reports JOIN devices ON devices.id = lan_reports.device_id
                 WHERE lan_reports.device_id = ?1 AND devices.owner = ?2",
                [device_id, user_id],
                |row| LanReport::from_row(row, 0),
            )
            .optional()
            .map(Option::flatten)
    }
}

/// Body of `POST /api/v1/device/wake_on_lan/info`.
#[derive(Deserialize)]
pub struct ReportRequest {
    mac: String,
    inner_ip: String,
    #[serde(default)]
    ip: Option<String>,
    subnet_mask: String,
}

#[derive(Serialize)]
pub struct ReportResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    support_wol: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_on_wol: Option<bool>,
}

/// `POST /api/v1/device/wake_on_lan/info`: the calling host's LAN identity, kept as its last
/// report.
pub async fn report(
    user: User,
    CallerDevice(caller): CallerDevice,
    State(devices): State<Devices>,
    State(reports): State<LanReports>,
    Body(request): Body<ReportRequest>,
) -> Result<api::Ok<ReportResponse>, ApiError> {
    let ip = request.ip.as_deref().unwrap_or_default();
    if request.mac.is_empty()
        && request.inner_ip.is_empty()
        && request.subnet_mask.is_empty()
        && ip.is_empty()
    {
        return Ok(api::Ok(ReportResponse {
            support_wol: Some(false),
            turn_on_wol: None,
        }));
    }

    // `inner_ip` is the address on the LAN; `ip` may be the address the host is seen at from
    // outside, so it is only checked.
    let report = LanReport::parse(&request.mac, &request.inner_ip, &request.subnet_mask)
        .ok_or(ApiError::INVALID_REQUEST)?;
    if !ip.is_empty() && ip.parse::<Ipv4Addr>().is_err() {
        return Err(ApiError::INVALID_REQUEST);
    }
    let device_id = caller.ok_or(ApiError::UNKNOWN_DEVICE)?;
    if devices.platform_of_owned(&device_id, &user.id)?.is_none() {
        return Err(ApiError::UNKNOWN_DEVICE);
    }
    reports.store(&device_id, &report)?;
    tracing::info!(user_id = %user.id, %device_id, "LAN report stored");
    Ok(api::Ok(ReportResponse {
        support_wol: None,
        turn_on_wol: Some(true),
    }))
}

/// Why a device could not be woken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeRefused {
    /// It never reported its LAN identity.
    NoLanReport,
    /// None of the user's online devices is on its network.
    NoHelper,
}

/// Asks an online device of `user_id` on the same IPv4 network as their offline device
/// `device_id` to wake it. Returns the helper's device id once the push is queued.
pub async fn wake(
    reports: &LanReports,
    rooms: &RoomClient,
    user_id: &str,
    device_id: &str,
) -> rusqlite::Result<Result<String, WakeRefused>> {
    let Some(target) = reports.of_owned(device_id, user_id)? else {
        return Ok(Err(WakeRefused::NoLanReport));
    };
    let push = json!({
        "type": "wake_on_lan",
        "data": { "target_device": target.target_device() },
    });
    let candidates = reports.of_user(user_id)?;
    let mut same_network = 0;
    for (helper_id, report) in &candidates {
        if helper_id == device_id || report.network() != target.network() {
            continue;
        }
        same_network += 1;
        if rooms
            .push_to_host(helper_id, "bmsg_push", push.clone())
            .await
        {
            return Ok(Ok(helper_id.clone()));
        }
    }
    tracing::debug!(%device_id, same_network, "no online wake helper");
    Ok(Err(WakeRefused::NoHelper))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_are_validated() {
        let report = LanReport::parse("AA-bb-cc-dd-ee-01", "192.168.1.20", "255.255.255.0");
        assert_eq!(
            report.unwrap().target_device(),
            json!({ "mac": "aa:bb:cc:dd:ee:01", "ip": "192.168.1.20", "subnet_mask": "255.255.255.0" })
        );
        for (mac, ip, mask) in [
            ("aa:bb:cc:dd:ee", "192.168.1.20", "255.255.255.0"),
            ("aa:bb:cc:dd:ee:01:02", "192.168.1.20", "255.255.255.0"),
            ("aabbccddee01", "192.168.1.20", "255.255.255.0"),
            ("aa:bb:cc:dd:ee:zz", "192.168.1.20", "255.255.255.0"),
            ("aa:bb:cc:dd:ee:01", "fe80::1", "255.255.255.0"),
            ("aa:bb:cc:dd:ee:01", "", "255.255.255.0"),
            ("aa:bb:cc:dd:ee:01", "192.168.1.20", "255.0.255.0"),
            ("aa:bb:cc:dd:ee:01", "192.168.1.20", "0.0.0.0"),
        ] {
            assert_eq!(LanReport::parse(mac, ip, mask), None, "{mac} {ip} {mask}");
        }
    }

    #[test]
    fn debug_hides_the_addresses() {
        let report = LanReport::parse("aa:bb:cc:dd:ee:01", "192.168.1.20", "255.255.255.0");
        let shown = format!("{report:?}");
        assert!(
            !shown.contains("192.168") && !shown.contains("aa"),
            "{shown}"
        );
    }

    #[test]
    fn only_the_users_own_devices_help() {
        let database = Database::in_memory();
        let reports = LanReports::from(database.clone());
        let report = LanReport::parse("aa:bb:cc:dd:ee:01", "192.168.1.20", "255.255.255.0");
        let report = report.unwrap();
        database
            .connection()
            .execute_batch(
                "INSERT INTO users (id, name) VALUES ('2', 'Other');
                 INSERT INTO devices (id, client_id, name, platform, owner)
                 VALUES ('mine', 'a', '', 1, '1'), ('theirs', 'b', '', 1, '2');",
            )
            .unwrap();
        reports.store("mine", &report).unwrap();
        reports.store("theirs", &report).unwrap();
        let ids: Vec<String> = reports
            .of_user("1")
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, ["mine"]);
        assert_eq!(reports.of_owned("theirs", "1").unwrap(), None);
    }
}
