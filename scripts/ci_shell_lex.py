"""Shared shell lexer for the CI test-lane gates.

It finds word, quote, comment, operator and substitution boundaries the way Bash
does for the plain command subset CI writes, and reports every span whose
boundaries it cannot prove as an issue instead of guessing. It evaluates nothing:
callers decide what an issue means for their verdict, and must fail closed on it.
"""

from __future__ import annotations

from dataclasses import dataclass

# Longest first, so `&&` is never read as two `&`; `(`/`)` also close case patterns.
CONTROL_OPS = (";;&", ";;", ";&", "&&", "||", "|&", ";", "&", "|", "(", ")")
REDIRECT_OPS = ("&>>", "&>", "<<<", "<>", "<&", ">&", ">>", ">|", "<", ">")
# Issues after which no later boundary is known; the others cover one closed span.
OPEN_ISSUES = frozenset({"unclosed-context", "unsupported-heredoc", "ambiguous-boundary"})
_META = " \t\n;&|()<>"
_NAME = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_"


@dataclass(frozen=True)
class ShellToken:
    kind: str  # WORD, OP, REDIR or NEWLINE
    raw: str
    # Quote-removed text with expansions kept verbatim; None when an issue overlaps.
    value: str | None
    start: int
    end: int
    # 0 in the outer shell, one more inside each command substitution.
    depth: int
    # Holds an expansion whose value exists only when the shell runs.
    dynamic: bool = False


@dataclass(frozen=True)
class LexIssue:
    kind: str
    start: int
    end: int


@dataclass(frozen=True)
class ShellScan:
    tokens: tuple[ShellToken, ...]
    issues: tuple[LexIssue, ...]
    # Comments blanked to spaces; length and newlines are kept, so offsets carry over.
    comment_masked: str
    # Also quoted, escaped, substituted and redirection text as `_`: only real
    # outer operators and words stay visible.
    syntax_masked: str


class _Lexer:
    def __init__(self, text: str) -> None:
        self.text, self.end = text, len(text)
        self.tokens: list[ShellToken] = []
        self.issues: list[LexIssue] = []
        self.comment = list(text)
        self.syntax = list(text)

    def mask(self, start: int, end: int, char: str = "_") -> None:
        for index in range(start, min(end, len(self.text))):
            if self.text[index] != "\n":
                self.syntax[index] = char
                if char == " ":
                    self.comment[index] = char

    def issue(self, kind: str, start: int, end: int) -> None:
        self.issues.append(LexIssue(kind, start, end))

    def emit(self, kind: str, start: int, end: int, depth: int,
             value: str | None = None, dynamic: bool = False) -> None:
        self.tokens.append(ShellToken(kind, self.text[start:end], value, start, end, depth,
                                      dynamic))

    def commands(self, index: int, depth: int, closer: str = "") -> int:
        """Lex commands from ``index`` to ``closer`` (a substitution's `)`) or the end;
        return the offset just past the closer."""
        opened, parens = index, 0
        while index < self.end:
            char = self.text[index]
            if closer and char == closer and not parens:
                return index + 1
            if char in " \t":
                index += 1
            elif char == "\n":
                self.emit("NEWLINE", index, index + 1, depth)
                index += 1
            elif char == "#":
                stop = self.text.find("\n", index, self.end)
                stop = self.end if stop < 0 else stop
                self.mask(index, stop, " ")
                index = stop
            elif self.text.startswith("\\\n", index):
                self.mask(index, index + 1)
                index += 2
            elif self.text.startswith("((", index):
                index = self.arithmetic(index, index + 2, depth)
            elif char in "<>" and self.text.startswith("(", index + 1):
                stop = self.commands(index + 2, depth + 1, ")")
                self.issue("unsupported-process-substitution", index, stop)
                self.mask(index, stop)
                self.emit("WORD", index, stop, depth, None, True)
                index = stop
            elif self.redirect_at(index):
                index = self.redirect(index, depth)
            elif char in ";&|()":
                op = next(op for op in CONTROL_OPS if self.text.startswith(op, index))
                if op == "(":
                    parens += 1
                elif op == ")":
                    parens -= bool(parens)
                self.emit("OP", index, index + len(op), depth)
                index += len(op)
            else:
                index = self.word(index, depth)
                if closer == ")" and self.tokens[-1].raw == "case":
                    # A case pattern's `)` would close this substitution early.
                    self.issue("ambiguous-boundary", opened, self.end)
        if closer:
            self.issue("unclosed-context", opened - 1, self.end)
        return self.end

    def redirect_at(self, index: int) -> bool:
        while index < self.end and self.text[index].isdigit():
            index += 1
        return index < self.end and (self.text[index] in "<>" or
                                     self.text.startswith("&>", index))

    def redirect(self, index: int, depth: int) -> int:
        start = index
        while self.text[index].isdigit():
            index += 1
        if self.text.startswith("<<", index) and not self.text.startswith("<<<", index):
            # Heredoc body lines are data up to a delimiter this lexer does not track.
            self.issue("unsupported-heredoc", start, len(self.text))
            self.end = index
            return index
        op = next(op for op in REDIRECT_OPS if self.text.startswith(op, index))
        index += len(op)
        if op in ("<&", ">&"):
            while index < self.end and (self.text[index].isdigit() or self.text[index] == "-"):
                index += 1
        self.mask(start, index)
        self.emit("REDIR", start, index, depth)
        return index

    def arithmetic(self, start: int, index: int, depth: int) -> int:
        """Consume `((…))` or `$((…))` as one dynamic word: nothing inside runs."""
        content, parens = index, 0
        while index < self.end:
            if self.text.startswith("))", index) and not parens:
                index += 2
                if any(mark in self.text[content:index] for mark in ("$(", "`")):
                    self.issue("unsupported-expansion", start, index)
                self.mask(start, index)
                if self.text[start] != "$":
                    self.emit("WORD", start, index, depth, self.text[start:index], True)
                return index
            parens += {"(": 1, ")": -1}.get(self.text[index], 0)
            index += 1
        self.issue("unclosed-context", start, self.end)
        self.mask(start, self.end)
        return self.end

    def word(self, index: int, depth: int) -> int:
        start, issues = index, len(self.issues)
        value: list[str] = []
        dynamic = False
        while index < self.end and self.text[index] not in _META:
            char = self.text[index]
            if char == "\\":
                if index + 1 < self.end and self.text[index + 1] != "\n":
                    value.append(self.text[index + 1])
                self.mask(index, index + 2)
                index += 2
            elif char == "'":
                stop = self.text.find("'", index + 1, self.end)
                if stop < 0:
                    self.issue("unclosed-context", index, self.end)
                    stop = self.end - 1
                value.append(self.text[index + 1:stop])
                self.mask(index, stop + 1)
                index = stop + 1
            elif char == '"':
                stop = self.double_quoted(index, depth, value)
                dynamic = dynamic or any(mark in self.text[index:stop] for mark in "$`")
                index = stop
            elif char in "$`":
                stop = self.expansion(index, depth)
                value.append(self.text[index:stop])
                dynamic = dynamic or stop > index + 1
                index = stop
            else:
                value.append(char)
                index += 1
        index = min(index, self.end)
        self.emit("WORD", start, index, depth,
                  None if len(self.issues) > issues else "".join(value), dynamic)
        return index

    def double_quoted(self, index: int, depth: int, value: list[str]) -> int:
        start, index = index, index + 1
        while index < self.end and self.text[index] != '"':
            char = self.text[index]
            if char == "\\" and index + 1 < self.end:
                follower = self.text[index + 1]
                if follower not in '$`"\\\n':
                    value.append(char)
                if follower != "\n":
                    value.append(follower)
                index += 2
            elif char in "$`":
                stop = self.expansion(index, depth)
                value.append(self.text[index:stop])
                index = stop
            else:
                value.append(char)
                index += 1
        if index >= self.end:
            self.issue("unclosed-context", start, self.end)
        self.mask(start, index + 1)
        return index + 1

    def expansion(self, index: int, depth: int) -> int:
        """The offset past the `$…` or backtick expansion at ``index``."""
        text = self.text
        if text[index] == "`":
            return self.backtick(index, depth)
        if text.startswith("$((", index):
            return self.arithmetic(index, index + 3, depth)
        if text.startswith("$(", index):
            stop = self.commands(index + 2, depth + 1, ")")
            self.mask(index, stop)
            return stop
        if text.startswith("${{", index):
            # A GitHub expression, replaced before the shell ever runs.
            stop = text.find("}}", index, self.end)
            return self.closed(index, stop + 2 if stop >= 0 else -1)
        if text.startswith("${", index):
            return self.braced(index)
        if text.startswith("$'", index):
            stop = index + 2
            while stop < self.end and text[stop] != "'":
                stop += 1 + (text[stop] == "\\")
            self.issue("unsupported-expansion", index, stop + 1)
            return self.closed(index, stop + 1 if stop < self.end else -1)
        if text.startswith('$"', index):
            self.mask(index, index + 1)
            return self.double_quoted(index + 1, depth, [])
        stop = index + 1
        if stop < self.end and text[stop] in "@*#?$!-":
            stop += 1
        else:
            while stop < self.end and text[stop] in _NAME:
                stop += 1
        self.mask(index, stop)
        return stop

    def closed(self, start: int, stop: int) -> int:
        if stop < 0:
            self.issue("unclosed-context", start, self.end)
            stop = self.end
        self.mask(start, stop)
        return stop

    def braced(self, start: int) -> int:
        """`${…}` up to its matching brace; a command inside it is not followed."""
        index, braces = start + 2, 1
        while index < self.end and braces:
            char = self.text[index]
            if char == "\\":
                index += 1
            elif char == "'":
                index = self.text.find("'", index + 1, self.end)
                if index < 0:
                    return self.closed(start, -1)
            elif self.text.startswith(("$(", "`"), index):
                self.issue("unsupported-expansion", start, index)
            braces += {"{": 1, "}": -1}.get(char, 0)
            index += 1
        return self.closed(start, index if not braces else -1)

    def backtick(self, start: int, depth: int) -> int:
        index, escaped = start + 1, False
        while index < self.end and self.text[index] != "`":
            escaped |= self.text[index] == "\\" and self.text[index + 1:index + 2] in "`\\$"
            index += 1 + (self.text[index] == "\\")
        if escaped:
            # Bash unescapes these before parsing the inside; nesting is not followed.
            self.issue("ambiguous-boundary", start, self.end)
        if index >= self.end:
            return self.closed(start, -1)
        outer, self.end = self.end, index
        self.commands(start + 1, depth + 1)
        # A heredoc inside ends the scan there, outside too.
        self.end = outer if self.end == index else self.end
        self.mask(start, index + 1)
        return index + 1


def scan_shell(text: str) -> ShellScan:
    """Lex ``text`` as one shell's source."""
    lexer = _Lexer(text)
    lexer.commands(0, 0)
    tokens = sorted(lexer.tokens, key=lambda token: (token.start, token.depth))
    return ShellScan(tuple(tokens), tuple(lexer.issues), "".join(lexer.comment),
                     "".join(lexer.syntax))


def comment_start(text: str, scan: ShellScan | None = None) -> int:
    """Where the first comment of ``text`` starts, or its length when there is none."""
    masked = (scan or scan_shell(text)).comment_masked
    return next((index for index, (char, kept) in enumerate(zip(text, masked))
                 if char != kept), len(text))


def simple_commands(scan: ShellScan) -> list[list[ShellToken]]:
    """The outer shell's simple commands: its tokens split at operators and newlines."""
    commands: list[list[ShellToken]] = [[]]
    for token in scan.tokens:
        if token.depth:
            continue
        if token.kind in ("OP", "NEWLINE"):
            commands.append([])
        else:
            commands[-1].append(token)
    return [command for command in commands if command]


def shell_words(text: str) -> tuple[str, ...] | None:
    """The outer words of ``text`` when it holds only words, comments and newlines."""
    scan = scan_shell(text)
    if scan.issues or any(token.kind in ("OP", "REDIR") for token in scan.tokens):
        return None
    return tuple(token.value for token in scan.tokens
                 if token.kind == "WORD" and not token.depth)
