//! Cloud targets (`remora` standalone, `remora` role-split, external URL, and `fakecloud`).

pub mod fakecloud;
pub mod remora;
pub mod turn;

use remora_proto::control::SignKey;

use crate::error::LabError;
use crate::redact::find_secret_leak;
use crate::scenario::model::FakeCloudSpec;

/// Deployment mode or test target against which scenarios are executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetSpec {
    RemoraStandalone,
    RemoraSplit,
    ExternalUrl(String),
    FakeCloud,
}

impl TargetSpec {
    pub fn label(&self) -> &'static str {
        match self {
            Self::RemoraStandalone => "remora (standalone)",
            Self::RemoraSplit => "remora (split)",
            Self::ExternalUrl(_) => "remora (external)",
            Self::FakeCloud => "fakecloud",
        }
    }

    pub fn slug(&self) -> &'static str {
        match self {
            Self::RemoraStandalone => "remora-standalone",
            Self::RemoraSplit => "remora-split",
            Self::ExternalUrl(_) => "remora-external",
            Self::FakeCloud => "fakecloud",
        }
    }
}

/// Live target instance managed during scenario execution.
pub enum RunningTarget {
    Remora {
        instance: remora::RemoraInstance,
        sign_key_raw: Option<String>,
    },
    External {
        base_url: String,
        sign_key: Option<SignKey>,
        sign_key_raw: Option<String>,
    },
    FakeCloud {
        instance: fakecloud::FakeCloudInstance,
        sign_key_raw: Option<String>,
    },
}

impl RunningTarget {
    pub async fn start(
        spec: &TargetSpec,
        remora_bin: Option<&std::path::Path>,
        sign_key: Option<&str>,
        fakecloud_script: Option<&FakeCloudSpec>,
    ) -> Result<Self, LabError> {
        let sign_key_raw = sign_key.map(str::to_owned);
        match spec {
            TargetSpec::RemoraStandalone => {
                let bin = remora::find_remora_bin(remora_bin)?;
                let instance = remora::RemoraInstance::spawn_standalone(&bin, sign_key).await?;
                Ok(Self::Remora {
                    instance,
                    sign_key_raw,
                })
            }
            TargetSpec::RemoraSplit => {
                let bin = remora::find_remora_bin(remora_bin)?;
                let instance = remora::RemoraInstance::spawn_split(&bin, sign_key).await?;
                Ok(Self::Remora {
                    instance,
                    sign_key_raw,
                })
            }
            TargetSpec::ExternalUrl(url) => Ok(Self::External {
                base_url: url.trim_end_matches('/').to_owned(),
                sign_key: sign_key.map(SignKey::new),
                sign_key_raw,
            }),
            TargetSpec::FakeCloud => {
                let instance =
                    fakecloud::FakeCloudInstance::spawn(sign_key, fakecloud_script.cloned())
                        .await?;
                Ok(Self::FakeCloud {
                    instance,
                    sign_key_raw,
                })
            }
        }
    }

    pub fn base_url(&self) -> &str {
        match self {
            Self::Remora { instance, .. } => instance.base_url(),
            Self::External { base_url, .. } => base_url,
            Self::FakeCloud { instance, .. } => instance.base_url(),
        }
    }

    pub fn sign_key(&self) -> Option<&SignKey> {
        match self {
            Self::Remora { instance, .. } => instance.sign_key(),
            Self::External { sign_key, .. } => sign_key.as_ref(),
            Self::FakeCloud { instance, .. } => instance.sign_key(),
        }
    }

    /// Checks captured target logs against both runtime secrets and the target's signing key.
    pub fn find_log_secret_leak(&self, secrets: &[(String, String)]) -> Option<String> {
        let logs = self.logs();
        if let Some(found) = find_secret_leak(&logs, secrets) {
            return Some(found);
        }
        let own_key = match self {
            Self::Remora { sign_key_raw, .. }
            | Self::External { sign_key_raw, .. }
            | Self::FakeCloud { sign_key_raw, .. } => sign_key_raw.as_ref(),
        };
        if let Some(key) = own_key {
            let pair = [("sign_key".to_owned(), key.clone())];
            return find_secret_leak(&logs, &pair);
        }
        None
    }

    /// Captured stdout/stderr logs for targets spawned by the lab.
    pub fn logs(&self) -> String {
        match self {
            Self::Remora { instance, .. } => instance.logs(),
            Self::External { .. } | Self::FakeCloud { .. } => String::new(),
        }
    }

    pub async fn shutdown(self) {
        match self {
            Self::Remora { instance, .. } => instance.shutdown().await,
            Self::FakeCloud { instance, .. } => instance.shutdown().await,
            Self::External { .. } => {}
        }
    }
}
