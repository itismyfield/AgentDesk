use super::super::ingress::Hold;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HoldReason {
    NativeTurn,
    Readiness,
    QueueNotice(Hold),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LogEpisode {
    number: u64,
    reason: HoldReason,
}

#[derive(Debug, PartialEq, Eq)]
struct NoticeObligation<'a> {
    ticket: &'a str,
    queued_generation: u64,
    episode: u64,
}

#[derive(Default)]
struct Episodes(Option<LogEpisode>);

impl Episodes {
    fn observe(&mut self, reason: HoldReason) -> Option<LogEpisode> {
        if self.0.is_some_and(|previous| previous.reason == reason) {
            return None;
        }
        let number = self.0.map_or(1, |previous| previous.number + 1);
        let episode = LogEpisode { number, reason };
        self.0 = Some(episode);
        Some(episode)
    }
}

impl LogEpisode {
    fn notice(self, ticket: &str, queued_generation: u64) -> Option<NoticeObligation<'_>> {
        matches!(self.reason, HoldReason::QueueNotice(_)).then_some(NoticeObligation {
            ticket,
            queued_generation,
            episode: self.number,
        })
    }
}

#[test]
fn drive_foundation_log_episodes_and_notice_obligations_are_separate() {
    use HoldReason::*;

    let mut held = Episodes::default();
    let a = QueueNotice(Hold::Modal);
    let b = QueueNotice(Hold::SessionSwitch);
    let log: Vec<_> = [a, a, b, b, a, NativeTurn, Readiness]
        .into_iter()
        .filter_map(|reason| held.observe(reason))
        .collect();
    assert_eq!(
        log.iter().map(|event| event.reason).collect::<Vec<_>>(),
        [a, b, a, NativeTurn, Readiness]
    );
    let owed: Vec<_> = log
        .iter()
        .filter_map(|event| event.notice("ticket", 1))
        .collect();
    assert_eq!(
        owed.iter().map(|notice| notice.episode).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(
        log[0].notice("ticket", 1),
        Some(NoticeObligation {
            ticket: "ticket",
            queued_generation: 1,
            episode: 1,
        })
    );
    assert_ne!(log[0].notice("ticket", 1), log[0].notice("ticket", 2));
}
