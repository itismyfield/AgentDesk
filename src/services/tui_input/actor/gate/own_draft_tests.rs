use super::own_draft;
use crate::services::tui_o::shadow::ShadowProvider;

fn composer(body: &str) -> String {
    let body = body.replace('\n', "\n  ");
    format!(
        "────────────────────\n❯ {body}\n────────────────────\n  ⏵⏵ bypass permissions on (shift+tab to cycle)"
    )
}

// A Claude pane captured after a busy-turn paste was left unentered: the second row drawn two
// columns in, the earlier submitted prompt above in scrollback.
const CLAUDE_TWO_ROW_PASTE: &str = "\
❯ E2E PR1 direct hold. First output exactly [E2E:PR1:pb1-c-s5d-pr1-074645:HOLD]
  then run in the foreground python3 -c 'import time; time.sleep(60)' then
  output exactly [E2E:PR1:pb1-c-s5d-pr1-074645:DONE]

⏺ [E2E:PR1:pb1-c-s5d-pr1-074645:DONE]

✻ Churned for 1m 4s · done 7:47 AM

────────────────────────────────────────────────────────────────────────────────
❯\u{00a0}[📱 adk-e2e-phase-b · 343742347365974026 · b9bf9a71]
  응답에 정확히 한 줄로 [E2E:PR1:pb1-c-s5d-pr1-074645] 만 출력해줘.
────────────────────────────────────────────────────────────────────────────────
  ⏱ 34m │ █░░░░░░░░░ │ 11% │ 105K/1.0M │ 📦️ 100% │ $0.43
  MCP: 2 │ Tools: 2 done
  ⏵⏵ bypass permissions on (shift+tab to cycle)
";

#[test]
fn a_flat_claude_paste_is_owned_only_as_drawn() {
    let frame = "[📱 adk-e2e-phase-b · 343742347365974026 · b9bf9a71]\n\
                 응답에 정확히 한 줄로 [E2E:PR1:pb1-c-s5d-pr1-074645] 만 출력해줘.";
    let owns = |capture: &str, frame: &str| own_draft(ShadowProvider::Claude, capture, frame, true);
    assert!(owns(CLAUDE_TWO_ROW_PASTE, frame));
    let ansi = CLAUDE_TWO_ROW_PASTE
        .replace("❯\u{00a0}", "\u{1b}[39m❯\u{00a0}")
        .replace("\n───", "\n\u{1b}[38;5;244m───");
    assert!(owns(&ansi, frame));
    let row = "\n  응답에 정확히 한 줄로 [E2E:PR1:pb1-c-s5d-pr1-074645] 만 출력해줘.";
    let second = |with: &str| CLAUDE_TWO_ROW_PASTE.replace(row, with);
    // A complete body differing only in whitespace uses the same wrap ownership proof.
    assert!(owns(&second(&row.replace("\n  ", "\n   ")), frame));
    let scrollback_only = CLAUDE_TWO_ROW_PASTE
        .replace(
            "❯\u{00a0}[📱 adk-e2e-phase-b · 343742347365974026 · b9bf9a71]",
            "❯\u{00a0}",
        )
        .replace(row, "")
        .replace(
            "\n\n✻",
            &format!("\n\n❯ {}\n\n✻", frame.replace('\n', "\n  ")),
        );
    for (case, capture) in [
        (
            "one character",
            second(&row.replace("출력해줘.", "출력해줘!")),
        ),
        ("typed after", second(&format!("{row}x"))),
        ("typed row", second(&format!("{row}\n  x"))),
        ("one-column indent", second(&row.replace("\n  ", "\n "))),
        ("unindented", second(&row.replace("\n  ", "\n"))),
        ("one row", second("")),
        ("scrollback only", scrollback_only),
    ] {
        assert!(!owns(&capture, frame), "{case}");
    }
    assert!(!owns(CLAUDE_TWO_ROW_PASTE, &format!("{frame}\nmore")));
    assert!(!own_draft(
        ShadowProvider::Claude,
        CLAUDE_TWO_ROW_PASTE,
        frame,
        false
    ));

    // Rows captured from Claude Code 2.1.293 at 80 columns: a line's own leading spaces stay
    // ahead of the indent, a blank line is an empty row, and three rows still render flat.
    let header = "[📱 s · a · n1]";
    let drawn = |rows: &[&str]| {
        let border = "─".repeat(80);
        format!(
            "{border}\n❯\u{00a0}{header}\n{}\n{border}\n",
            rows.join("\n")
        )
    };
    for (rows, lines) in [
        (
            vec!["     three leading spaces"],
            vec!["   three leading spaces"],
        ),
        (vec!["   one leading space"], vec![" one leading space"]),
        (vec!["", "  third"], vec!["", "third"]),
        (
            vec!["  line two", "  line three"],
            vec!["line two", "line three"],
        ),
    ] {
        let frame = format!("{header}\n{}", lines.join("\n"));
        assert!(owns(&drawn(&rows), &frame), "{frame}");
    }
    let lead = format!("{header}\nthree leading spaces");
    assert!(owns(&drawn(&["     three leading spaces"]), &lead));
    // Whole-body proof tolerates visual wrap boundaries while rejecting extra nonwhite text.
    for (first, rest, gap) in [
        (
            format!("{}abcdef", "abcdefghij".repeat(7)),
            format!("ghij{}", "abcdefghij".repeat(2)),
            "",
        ),
        (
            format!("{}가나다라마바사아", "가나다라마바사아자차".repeat(3)),
            format!("자차{}", "가나다라마바사아자차"),
            "",
        ),
        ("😀a".repeat(25), "😀a".repeat(15), ""),
        (
            ["word"; 15].join(" "),
            "word word word word word end".to_string(),
            " ",
        ),
    ] {
        let frame = format!("{header}\n{first}{gap}{rest}");
        let rows = [format!("  {first}"), format!("  {rest}")];
        assert!(owns(&drawn(&[&rows[0], &rows[1]]), &frame), "{frame}");
        let foreign = format!("{}x", rows[1]);
        assert!(!owns(&drawn(&[&rows[0], &foreign]), &frame), "{frame}");
    }
}

#[test]
fn folded_own_draft_requires_exact_k_empty_attestation_and_no_other_text() {
    for lines in [2, 20, 200, 2000] {
        let text = vec!["line"; lines].join("\n");
        let folded = format!("[Pasted text #42 +{} lines]", lines - 1);
        assert!(own_draft(
            ShadowProvider::Claude,
            &composer(&folded),
            &text,
            true
        ));
        assert!(!own_draft(
            ShadowProvider::Claude,
            &composer(&folded),
            &text,
            false
        ));
        for body in [
            format!("[Pasted text #42 +{lines} lines]"),
            format!("{folded}foreign"),
            format!("{folded}\n[Pasted text #43 +1 lines]"),
        ] {
            assert!(
                !own_draft(ShadowProvider::Claude, &composer(&body), &text, true),
                "{body}"
            );
        }
    }
    for count in [799, 800, 801] {
        let text = "x".repeat(count);
        assert_eq!(
            own_draft(
                ShadowProvider::Claude,
                &composer("[Pasted text #7]"),
                &text,
                true
            ),
            count > 800
        );
    }
    assert!(!own_draft(
        ShadowProvider::Codex,
        "› [Pasted Content 900 chars]",
        "x",
        true
    ));
}

// A Codex composer holding a pasted Discord URL that Codex 0.160 draws as an OSC 8 hyperlink,
// wrapped across two rows; each row opens and closes its own link.
#[test]
fn a_codex_draft_with_a_wrapped_osc8_link_is_owned_by_its_shown_text() {
    let url =
        "https://discord.com/channels/1469870512812462284/1479430397975728169/1565218453684486198";
    let frame = format!(
        "[User: 0hbujang (ID: 343742347365974026)] {url} 이 메시지에 대한 답변이 게시가 안된거같은데 확인좀해봐"
    );
    let link = |shown: &str| format!("\x1b]8;;{url}\x1b\\{shown}\x1b]8;;\x1b\\");
    let pane = |first: &str, second: &str| {
        include_str!("../../../../../tests/fixtures/tui_input/codex-0.160.0-draft.ansi").replace(
            "pending fixture draft",
            &format!("[User: 0hbujang (ID: 343742347365974026)] {first}\n  {second} 이 메시지에 대한 답변이 게시가 안된거같은데 확인좀해봐"),
        )
    };
    let tail = &url["https://discord.com/".len()..];
    let owns = |capture: &str| own_draft(ShadowProvider::Codex, capture, &frame, true);
    assert!(owns(&pane(&link("https://discord.com/"), &link(tail))));
    let bel = |shown: &str| format!("\x1b]8;;{url}\x07{shown}\x1b]8;;\x07");
    assert!(owns(&pane(&bel("https://discord.com/"), &bel(tail))));
    // The link target never stands in for text the composer does not show.
    assert!(!owns(&pane(&link(""), &link(""))));
    // A link left open stays as text, so it neither reads as the frame nor hides foreign text.
    let open = format!("\x1b]8;;{url}https://discord.com/");
    assert!(!owns(&pane(&open, &link(tail))));
    let status = "\n\n  gpt-5.4 · Fast off · Context 100% left";
    assert!(own_draft(
        ShadowProvider::Codex,
        &format!("› own{status}"),
        "own",
        true
    ));
    let hidden = format!("› own\x1b]8;;u foreign{status}");
    assert!(!own_draft(ShadowProvider::Codex, &hidden, "own", true));
}
