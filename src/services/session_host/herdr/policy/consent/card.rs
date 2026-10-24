use super::{CardState, Consent, State};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Button {
    pub label: &'static str,
    pub custom_id: String,
    pub disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Card {
    pub content: String,
    pub buttons: Vec<Button>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Click {
    pub nonce: String,
    pub yes: bool,
}

fn bounded(card: Card) -> Result<Card, String> {
    if card.content.encode_utf16().count() > 1900 {
        return Err("consent card path or node is too long to display; preserve held inputs and shorten the configured path before owner retry".into());
    }
    Ok(card)
}

fn valid_nonce(nonce: &str) -> bool {
    !nonce.is_empty()
        && nonce.len() <= 64
        && nonce
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
}

pub(crate) fn parse(custom_id: &str) -> Option<Click> {
    let mut parts = custom_id.strip_prefix("hresume:v1:")?.split(':');
    let nonce = parts.next()?;
    let yes = match parts.next()? {
        "yes" => true,
        "no" => false,
        _ => return None,
    };
    (parts.next().is_none() && valid_nonce(nonce)).then(|| Click {
        nonce: nonce.into(),
        yes,
    })
}

#[derive(Clone, Copy)]
pub(crate) enum ResultKind {
    FirstApply,
    Existing,
}

pub(crate) fn render(consent: &Consent) -> Result<Card, String> {
    render_result(consent, ResultKind::Existing)
}

pub(crate) fn render_result(consent: &Consent, kind: ResultKind) -> Result<Card, String> {
    if !valid_nonce(&consent.nonce) {
        return Err(
            "invalid consent card nonce; preserve held inputs and retry on the execution node"
                .into(),
        );
    }
    let content = match consent.state {
        State::Pending => format!(
            "이 herdr 서버에서 새 세션을 열려면 에이전트 자동 재개를 꺼야 합니다.\n{}의 session.resume_agents_on_restore를 false로 변경할까요?\n개인 workspace를 포함한 이 서버 전체에 적용되며, 다음 herdr 시작부터 자동 재개가 꺼집니다. 레이아웃 복원은 유지됩니다. 현재 서버는 재시작하지 않습니다.",
            consent.key.config.display()
        ),
        State::Accepted => {
            "설정 변경을 적용하고 있습니다. 완료 전에는 다시 시도할 수 없습니다.".into()
        }
        State::Applied => format!(
            "{}설정을 껐습니다. 보류한 입력을 다시 확인해 세션을 엽니다. 자동 재개 Off는 다음 herdr 시작부터 적용됩니다. 현재 서버는 재시작하지 않았습니다.\n시작 당시 Off 확인: awaiting_server_restart · 새 세션 준비: ready\n세션 시작 결과는 별도로 안내합니다.",
            match kind {
                ResultKind::FirstApply => "",
                ResultKind::Existing => "이미 적용됨: ",
            }
        ),
        State::Denied => "아니오를 선택했습니다. 입력은 보류한 채 보존합니다.".into(),
        State::Expired => "동의 카드가 만료되었습니다. 입력은 보류한 채 보존합니다.".into(),
        State::ApplyFailed => {
            let detail: String = consent
                .apply
                .as_ref()
                .and_then(|result| result.detail.as_deref())
                .unwrap_or("원인 정보 없음")
                .chars()
                .take(512)
                .collect();
            format!("설정 적용을 확인하지 못했습니다. 입력은 보류한 채 보존합니다.\n원인: {detail}")
        }
    };
    let content = if matches!(
        consent.state,
        State::Denied | State::Expired | State::ApplyFailed
    ) {
        format!(
            "{content}\n현재 owner가 실행 노드 {}의 gateway에서 !herdr consent retry를 실행하세요.",
            consent.key.node
        )
    } else {
        content
    };
    let buttons = if consent.state == State::Pending && consent.card != CardState::Awaiting {
        [("예", "yes"), ("아니오", "no")]
            .into_iter()
            .map(|(label, choice)| Button {
                label,
                custom_id: format!("hresume:v1:{}:{choice}", consent.nonce),
                // A sent card stays disabled until its message binding is durable.
                disabled: consent.card != CardState::Bound || consent.message == 0,
            })
            .collect()
    } else {
        Vec::new()
    };
    bounded(Card { content, buttons })
}

pub(crate) fn revision_changed(consent: &Consent) -> Result<Card, String> {
    let mut card = render(consent)?;
    card.content = format!(
        "설정 revision이 변경되었습니다. 입력은 보류한 채 보존합니다. 현재 owner가 실행 노드 {}의 gateway에서 !herdr consent retry를 실행하세요.",
        consent.key.node
    );
    card.buttons.clear();
    bounded(card)
}

pub(crate) fn binding_failed(consent: &Consent) -> Result<Card, String> {
    bounded(Card {
        content: format!(
            "카드 연결 실패 — 입력은 보류한 채 보존합니다. 현재 owner가 실행 노드 {}의 gateway에서 !herdr consent retry를 실행하세요.",
            consent.key.node
        ),
        buttons: Vec::new(),
    })
}

#[cfg(test)]
#[path = "card_tests.rs"]
mod tests;
