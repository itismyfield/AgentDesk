"""Independent, version-pinned O unit exclusion, UTF-16 splitting and payload hashes."""
import json
import re

from . import Unknown
from .manifest import digest

WHITE_SPACE = "\t\n\v\f\r \x85\xa0\u1680" + "".join(chr(n) for n in range(0x2000, 0x200b)) + "\u2028\u2029\u202f\u205f\u3000"
EXTENSIONS = set("bash css csv db env gif html jpeg jpg js json jsx lock log md pdf png py rs scss sh sql sqlite svg toml ts tsx txt webp xml yaml yml zsh".split())
FILE_STEMS = set("app cargo changelog client config defaults dockerfile index lib license main makefile mod package readme requirements schema server settings test tests tsconfig".split())


def canonical(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")


def units(text):
    return len(text.encode("utf-16-le")) // 2


def cut(text, budget):
    total = 0
    for n, char in enumerate(text):
        total += 2 if ord(char) > 0xffff else 1
        if total > budget:
            return n
    return len(text)


def terminal_allowed(text, index):
    if text[index] != ".":
        return True
    before = text[:index].rstrip(WHITE_SPACE)
    last, after = before[-1:] or " ", text[index + 1:index + 2]
    if last in "0123456789" and after and after in "0123456789":
        return False
    token = re.split("[" + re.escape(WHITE_SPACE + "`([{ /\\") + "]", text[:index])[-1]
    incoming = text[index + 1:].lstrip(WHITE_SPACE)
    match = re.match(r"[A-Za-z0-9_-]*", incoming)
    extension, rest = match.group(), incoming[match.end():]
    file_like = bool(re.search(r"[0-9_.-]", token)) or token.lower() in FILE_STEMS
    joins = bool(token) and not any("\uac00" <= ch <= "\ud7a3" for ch in token)
    joins = joins and extension in EXTENSIONS and (not rest.strip(WHITE_SPACE) or file_like)
    return not (last.isascii() and last.isalnum() and joins)


def boundary(window, in_fence):
    newline = window.rfind("\n")
    if newline >= 0:
        return newline or len(window)
    if in_fence or window.lstrip(WHITE_SPACE).startswith(("```", "|", "- ", "* ", "+ ", "> ")):
        return len(window)
    inline, last = False, None
    for index, char in enumerate(window):
        if char == "`":
            inline = not inline
        elif not inline and char in ".!?…。！？" and terminal_allowed(window, index):
            last = index + 1
    return last or len(window)


def split(text):
    pieces, in_fence, language = [], False, ""
    while text:
        overhead = 7 + units(language) if in_fence else 0
        budget = 2000 - overhead - 10
        prefix = "```" + language + "\n" if in_fence else ""
        if units(text) <= budget:
            pieces.append(prefix + text)
            break
        end = boundary(text[:cut(text, budget)], in_fence)
        if not end:
            raise Unknown("split_no_progress")
        head, text = text[:end], text[end:]
        for line in head.split("\n"):
            line = line.removesuffix("\r").lstrip(WHITE_SPACE)
            if line.startswith("```"):
                in_fence = not in_fence
                language = line[3:][:cut(line[3:], 500)] if in_fence else ""
        pieces.append(prefix + head + ("\n```" if in_fence else ""))
        text = text.removeprefix("\n")
    if any(not piece or units(piece) > 2000 for piece in pieces):
        raise Unknown("split_piece_over_limit")
    return pieces


def plan(unit, scope):
    kind, excluded = unit["kind"], unit["excluded"]
    payload = unit["payload"].strip(WHITE_SPACE) if unit["payload"] is not None else None
    if kind == "tool":
        excluded = "tool_call_panel"
    elif payload == "":
        excluded = "empty_payload"
    pieces = [] if excluded else [{"piece_index": n, "units": units(piece), "payload_hash": digest(piece.encode())}
                                   for n, piece in enumerate(split(payload))]
    signature = digest(canonical([excluded, pieces, payload if kind == "tool" else None]))
    key = dict(channel_id=scope["channel_id"], provider=scope["provider"], native_key=unit["native_key"], kind=kind)
    return dict(unit_key=key, excluded=excluded, pieces=pieces, seal_signature=signature)
