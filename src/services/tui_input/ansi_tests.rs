use super::ansi::{Piece, pieces, strip_ansi_escape_sequences as strip};

#[test]
fn csi_sequences_are_removed_and_reported_with_their_parameters() {
    assert_eq!(
        strip("\x1b[1;2m› \x1b[0mdraft\r\n\x1b[38;5;244m───"),
        "› draft\n───"
    );
    let csi: Vec<_> = pieces("a\x1b[2mb\x1b[K")
        .filter(|piece| matches!(piece, Piece::Csi { .. }))
        .collect();
    assert_eq!(
        csi,
        [
            Piece::Csi {
                params: "2",
                end: 'm'
            },
            Piece::Csi {
                params: "",
                end: 'K'
            }
        ]
    );
    // An ESC opening neither CSI nor OSC stays visible; an unfinished CSI ends the capture.
    assert_eq!(strip("a\x1b(Bb"), "a\x1b(Bb");
    assert_eq!(strip("a\x1b[12"), "a");
    assert_eq!(strip("plain › 한글 😀\ttext"), "plain › 한글 😀\ttext");
}

#[test]
fn osc_strings_are_dropped_and_the_text_they_wrap_is_kept() {
    let url = "https://discord.com/channels/1/2/3";
    for (open, close) in [
        (format!("\x1b]8;;{url}\x07"), "\x1b]8;;\x07"),
        (format!("\x1b]8;;{url}\x1b\\"), "\x1b]8;;\x1b\\"),
    ] {
        let row = format!("› see {open}{url}{close} now");
        assert_eq!(strip(&row), format!("› see {url} now"));
        let two = format!("› {open}https://discord.{close}\n  {open}com/x{close} y");
        assert_eq!(strip(&two), "› https://discord.\n  com/x y");
    }
    assert_eq!(strip("\x1b]0;title\x07\x1b[1mbold\x1b[0m"), "bold");
}

#[test]
fn an_osc_left_open_ends_at_its_row_or_the_next_escape() {
    assert_eq!(
        strip("› \x1b]8;;https://x hidden\n  next row"),
        "› \n  next row"
    );
    assert_eq!(strip("› \x1b]8;;https://x"), "› ");
    let interrupted: Vec<_> = pieces("\x1b]8;;u\x1b[1mb").collect();
    assert_eq!(
        interrupted,
        [
            Piece::Csi {
                params: "1",
                end: 'm'
            },
            Piece::Text('b')
        ]
    );
    assert_eq!(strip("\x1b]8;;u\x1bxy"), "\x1bxy");
}
