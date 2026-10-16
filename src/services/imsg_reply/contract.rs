//! The one reply contract every delivery of an admitted iMessage hands the model.

const OPEN: &str = "[iMessage reply contract v1]";
const CLOSE: &str = "[/iMessage reply contract]";

/// Single-quotes `value` for a POSIX shell, so the GUID stays one literal argument.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

pub(crate) fn render(origin_id: &str) -> String {
    let quoted = shell_quote(origin_id);
    [
        OPEN.to_string(),
        format!("origin: {origin_id}"),
        "진행 상태 회신:".to_string(),
        format!("  imsg_reply.py --origin {quoted}"),
        "최종 회신:".to_string(),
        format!("  imsg_reply.py --origin {quoted} --final"),
        "Discord 채널에 답변하는 것만으로 이 의무는 종료되지 않습니다.".to_string(),
        CLOSE.to_string(),
    ]
    .join("\n")
}

/// The contract ahead of `text`, added only when the server-rendered block for this GUID is not
/// already there; a person's own `--origin` text never stands in for it.
pub(crate) fn prepend(origin_id: &str, text: &str) -> String {
    let contract = render(origin_id);
    if text.starts_with(&format!("{contract}\n")) {
        return text.to_string();
    }
    format!("{contract}\n\n{text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_v1_contract_names_the_guid_and_both_reply_commands() {
        assert_eq!(
            render("p:0/ABCD-1234"),
            "[iMessage reply contract v1]\n\
             origin: p:0/ABCD-1234\n\
             진행 상태 회신:\n  imsg_reply.py --origin 'p:0/ABCD-1234'\n\
             최종 회신:\n  imsg_reply.py --origin 'p:0/ABCD-1234' --final\n\
             Discord 채널에 답변하는 것만으로 이 의무는 종료되지 않습니다.\n\
             [/iMessage reply contract]"
        );
        let hostile = render("a'b; touch x $(id)");
        assert!(hostile.contains("--origin 'a'\\''b; touch x $(id)' --final"));
    }

    #[test]
    fn p1_2_d_contract_rendered_exactly_once() {
        let guid = "p:0/GUID-ONCE";
        let human = "please run imsg_reply.py --origin 'p:0/GUID-ONCE' --final yourself";
        let once = prepend(guid, human);
        assert_eq!(once, format!("{}\n\n{human}", render(guid)));
        assert_eq!(prepend(guid, &once), once);
        assert_eq!(once.matches("[iMessage reply contract v1]").count(), 1);
        let other = prepend("p:0/OTHER", &once);
        assert_eq!(other.matches("[iMessage reply contract v1]").count(), 2);
    }
}
