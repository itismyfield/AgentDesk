//! Why held input got no consent card: Discord sees a stable code with fixed wording and the
//! request's own policy observation; the raw failure, which may name runtime paths, stays local.

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Code {
    RemoteGateway,
    ConsentStore,
    Custody,
    HostedRow,
    PeerSource,
    Preview,
    Owner,
    DiscordIdentity,
    Record,
    Card,
    Missing,
}

impl Code {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::RemoteGateway => "consent_remote_gateway",
            Self::ConsentStore => "consent_store_unavailable",
            Self::Custody => "consent_custody_unverified",
            Self::HostedRow => "hosted_row_unverified",
            Self::PeerSource => "consent_peer_unavailable",
            Self::Preview => "consent_preview_unavailable",
            Self::Owner => "consent_owner_unconfigured",
            Self::DiscordIdentity => "discord_identity_unavailable",
            Self::Record => "consent_record_unverified",
            Self::Card => "consent_card_unavailable",
            Self::Missing => "consent_assessment_missing",
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Self::RemoteGateway => {
                "실행 노드가 아닌 gateway는 동의와 입력 해제를 전달하지 않습니다."
            }
            Self::ConsentStore => "동의 저장소를 읽거나 쓰지 못했습니다.",
            Self::Custody => "보류 입력의 보관 상태를 확인하지 못했습니다.",
            Self::HostedRow => "이 채널의 실행 기록을 확인하지 못했습니다.",
            Self::PeerSource => "herdr 서버 관측 경로를 준비하지 못했습니다.",
            Self::Preview => "herdr 설정 변경 미리보기를 만들지 못했습니다.",
            Self::Owner => "owner가 설정되지 않아 동의를 물을 수 없습니다.",
            Self::DiscordIdentity => "Discord 봇 정보를 확인하지 못했습니다.",
            Self::Record => "보류 입력의 동의 기록을 확인하지 못했습니다.",
            Self::Card => "동의 카드를 보내지 못했습니다.",
            Self::Missing => "동의 판정 결과가 없습니다.",
        }
    }

    /// Tags a raw failure with this code; the raw text is kept for local logs only.
    pub(super) fn of(self) -> impl FnOnce(String) -> Refusal {
        move |detail| Refusal {
            code: self,
            observed: None,
            detail,
        }
    }
}

#[derive(Debug, Clone)]
pub(in crate::services::discord) struct Refusal {
    code: Code,
    /// This request's own next-restart observation as `(reason, action)`, when it made one.
    observed: Option<(String, String)>,
    detail: String,
}

impl Refusal {
    /// Attaches the observation this request recorded, so a later record by another request for
    /// the same socket cannot change what this hold reports.
    pub(super) fn observed(mut self, observation: &Value) -> Self {
        let text = |key: &str| observation[key].as_str().unwrap_or_default().to_owned();
        self.observed = Some((text("reason"), text("action")));
        self
    }

    pub(super) fn code(&self) -> &'static str {
        self.code.as_str()
    }

    pub(super) fn detail(&self) -> &str {
        &self.detail
    }

    fn reason(&self) -> &str {
        self.observed
            .as_ref()
            .map_or("not_observed", |(reason, _)| reason)
    }

    pub(super) fn notice(&self) -> String {
        let action = self.observed.as_ref().map_or(
            "실행 노드의 로컬 로그에서 이 오류 코드를 확인한 뒤 retry하세요.",
            |(_, action)| action,
        );
        format!(
            "herdr 사용자 서버를 검증하지 못해 이 입력을 보류했습니다. 원 입력은 보존됨(대기열에 남아 있음).\n원인: {}\n조치: {action}\n오류: {} — {}",
            self.reason(),
            self.code.as_str(),
            self.code.describe()
        )
    }

    /// The duplicate-suppression key: no path, time or raw text, so one cause stays one notice.
    pub(super) fn semantic(&self) -> String {
        format!("{}|{}", self.reason(), self.code.as_str())
    }
}

/// The text any Discord-visible error carries, including a command error reply.
impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "herdr consent hold {}: {}",
            self.code.as_str(),
            self.code.describe()
        )
    }
}
