//! A prepared native frame registry and ticket the upper-crate native input tests start from.

use super::native::{Disposition, Episode, Outcome, Ticket};
use super::*;

pub struct Fixture {
    pub history: Registry,
    pub ticket: Ticket,
    pub input: Value,
}

impl Fixture {
    pub fn at(path: PathBuf, channel: u64, primary: bool) -> Self {
        Self::at_execution(path, channel, primary, "incarnation-1")
    }
    pub fn at_execution(path: PathBuf, channel: u64, primary: bool, execution: &str) -> Self {
        Self::at_target(
            path,
            channel,
            primary,
            json!({"host":"herdr","owner":"fixture-pane","execution_nonce":execution}),
        )
    }

    pub fn at_target(path: PathBuf, channel: u64, primary: bool, target: Value) -> Self {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let transcript = path.with_extension("jsonl");
        std::fs::write(&transcript, "native input\n").unwrap();
        let key = json!({"identity":{"primary":channel+1,"sources":[{"message_id":channel+1,"queued_generation":1}]},
            "attempt":{"nonce":format!("attempt-{channel}"),"provider":"claude",
                "target":target,
                "source":{"kind":"existing","session_id":"native","transcript":transcript,
                    "file":file_id(&std::fs::metadata(&transcript).unwrap()),"offset":0}}});
        let input = json!({"identity":key["identity"],"attempt":key["attempt"],
            "original":{"author_id":7},"payload":{"frame":"한국어 원본 입력"}});
        let history = Registry::at(path);
        history.prepare("native-fixture", &input).unwrap();
        let ticket = Ticket {
            key: key.clone(),
            channel_id: channel,
            source: Some(history.source("native-fixture", &key).unwrap()),
            extra: Default::default(),
            owning_episode: primary.then(|| Episode {
                user_message_id: channel + 1,
                turn_nonce: format!("episode-{channel}"),
                generation: 1,
                extra: Default::default(),
            }),
        };
        history.prepare_native("native-fixture", &ticket).unwrap();
        Self {
            history,
            ticket,
            input,
        }
    }
    pub fn outcome(disposition: Disposition) -> Outcome {
        Outcome {
            disposition,
            range: Some([0, 1]),
            extra: Default::default(),
        }
    }
}
