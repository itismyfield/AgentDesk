"""Generated-block resolver extracted unchanged from the pipeline merge_autoresolve.py."""

def hunks(lines):
    """Yield ("text", [lines]) and ("hunk", ours, base_or_None, theirs) in order; raise ValueError on bad markers."""
    i, plain = 0, []
    while i < len(lines):
        if not lines[i].startswith("<<<<<<< "):
            plain.append(lines[i]); i += 1; continue
        if plain: yield ("text", plain); plain = []
        parts, cur, i = {"ours": [], "base": None, "theirs": []}, "ours", i + 1
        while i < len(lines) and not lines[i].startswith(">>>>>>> "):
            if lines[i].startswith("||||||| "): parts["base"], cur = [], "base"
            elif lines[i].rstrip("\n") == "=======": cur = "theirs"
            else: parts[cur].append(lines[i])
            i += 1
        if i == len(lines): raise ValueError("unterminated conflict")
        yield ("hunk", parts["ours"], parts["base"], parts["theirs"]); i += 1
    if plain: yield ("text", plain)


def generated_block(lines, begin, end, marker):
    out, inside = [], False
    for kind, *rest in hunks(lines):
        if kind == "text":
            for line in rest[0]:
                if line.startswith(begin): inside = True
                elif line.startswith(end): inside = False
            out += rest[0]
        else:
            ours, _, theirs = rest
            if not inside or any(marker in l for l in ours + theirs): return None
            out += theirs
    return out
