//! Splits a `capture-pane -e` capture into the text it shows and the CSI sequences styling it.
//! Codex and Claude pane readers share it so a hyperlink reads as its shown text in both.

/// One unit of a capture: a shown character, or a CSI sequence's parameters and final byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Piece<'a> {
    Text(char),
    Csi { params: &'a str, end: char },
}

/// OSC strings such as a hyperlink target are dropped up to BEL or ST, keeping the text they
/// wrap. One left open ends at its row, so the rows below are still read.
pub(crate) fn pieces(capture: &str) -> Pieces<'_> {
    Pieces { rest: capture }
}

pub(crate) struct Pieces<'a> {
    rest: &'a str,
}

impl<'a> Iterator for Pieces<'a> {
    type Item = Piece<'a>;

    fn next(&mut self) -> Option<Piece<'a>> {
        loop {
            let mut chars = self.rest.chars();
            let c = chars.next()?;
            let after = chars.as_str();
            if c == '\x1b' {
                // An unfinished CSI swallows the rest of the capture.
                if let Some(body) = after.strip_prefix('[') {
                    let Some(at) = body.find(|c| ('@'..='~').contains(&c)) else {
                        self.rest = "";
                        return None;
                    };
                    self.rest = &body[at + 1..];
                    let end = char::from(body.as_bytes()[at]);
                    return Some(Piece::Csi {
                        params: &body[..at],
                        end,
                    });
                }
                if let Some(body) = after.strip_prefix(']') {
                    #[cfg(test)]
                    if super::mutant("osc_as_text") {
                        self.rest = after;
                        return Some(Piece::Text(c));
                    }
                    self.rest = after_osc(body);
                    continue;
                }
            }
            // Any other ESC stays as text so the row cannot read as plain text it is not.
            self.rest = after;
            return Some(Piece::Text(c));
        }
    }
}

/// Past BEL or ST; at a newline or another escape, which an OSC never contains, it stops short.
fn after_osc(body: &str) -> &str {
    let Some(at) = body.find(['\x07', '\x1b', '\n']) else {
        return "";
    };
    let rest = &body[at..];
    rest.strip_prefix('\x07')
        .or_else(|| rest.strip_prefix("\x1b\\"))
        .unwrap_or(rest)
}

/// The text a capture shows, carriage returns removed.
pub(crate) fn strip_ansi_escape_sequences(capture: &str) -> String {
    pieces(capture)
        .filter_map(|piece| match piece {
            Piece::Text(c) if c != '\r' => Some(c),
            _ => None,
        })
        .collect()
}
