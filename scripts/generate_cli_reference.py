#!/usr/bin/env python3
"""Generate ``docs/generated/cli-reference.md`` from the clap derive sources.

The README used to carry a hand-written ``## CLI Reference`` block that
drifted from ``src/cli/args.rs`` (missing subcommands, flags that no longer
exist). This generator parses the ``#[derive(Parser | Subcommand | Args |
ValueEnum)]`` items under ``src/`` and renders every command, nested
subcommand, flag, and positional with the same names clap derives at runtime
(kebab-case variant names, ``--field-name`` longs, ``<FIELD_NAME>`` value
names, ``#[command(name = ...)]`` / ``#[arg(long = ...)]`` overrides).

Parsing ``args.rs`` instead of dumping ``agentdesk --help`` keeps the check
cheap enough to run on every CI pass without a cargo build. The clap unit
tests in ``src/cli/args.rs`` (``top_level_command_name_snapshot_...``) pin the
runtime name derivation; ``tests/test_generate_cli_reference.py`` asserts this
generator reproduces that snapshot so the two cannot diverge silently.

Test modules (``#[cfg(test)] mod``) are blanked before parsing; ``target/`` is
never scanned. Commands are rendered in declaration order (the order
``agentdesk --help`` prints) so the output is deterministic. CI regenerates
the document and fails on ``git diff`` drift (``scripts/ci-script-checks.sh``).
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from generate_env_reference import (  # noqa: E402
    blank_test_modules,
    production_rust_files,
    rel_posix,
)
from generate_inventory_docs import scan_balanced  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parent.parent
OUTPUT_DOC = REPO_ROOT / "docs" / "generated" / "cli-reference.md"
GENERATOR_COMMAND = "python3 scripts/generate_cli_reference.py"
BINARY_NAME = "agentdesk"

CLAP_DERIVES = ("Parser", "Subcommand", "Args", "ValueEnum")

_DERIVE_RE = re.compile(r"#\[derive\((?P<list>[^)]*)\)\]")
_ITEM_HEAD_RE = re.compile(
    r"(?:pub(?:\([^)]*\))?\s+)?(?P<kind>enum|struct)\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*"
)
_IDENT_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
_STRING_RE = re.compile(r'"((?:[^"\\]|\\.)*)"', re.DOTALL)


class ParseError(RuntimeError):
    pass


# --------------------------------------------------------------------------- #
# Lexical helpers
# --------------------------------------------------------------------------- #


def kebab_case(name: str) -> str:
    """Mirror heck's ``to_kebab_case`` for the identifiers clap derives."""

    words = re.findall(r"[A-Z]+(?![a-z])|[A-Z][a-z0-9]*|[a-z0-9]+", name)
    return "-".join(word.lower() for word in words)


def screaming_snake(name: str) -> str:
    return name.upper()


def unescape(text: str) -> str:
    return (
        text.replace('\\"', '"')
        .replace("\\n", " ")
        .replace("\\t", " ")
        .replace("\\\\", "\\")
    )


def split_top_level_commas(text: str) -> list[str]:
    parts: list[str] = []
    depth = 0
    last = 0
    index = 0
    while index < len(text):
        ch = text[index]
        if ch == '"':
            match = _STRING_RE.match(text, index)
            if match is None:
                raise ParseError(f"unterminated string in attribute: {text!r}")
            index = match.end()
            continue
        if ch == "'":
            # char literal such as short = 'x'
            index += 3 if index + 2 < len(text) and text[index + 2] == "'" else 1
            continue
        if ch in "([{<":
            depth += 1
        elif ch in ")]}>":
            depth -= 1
        elif ch == "," and depth == 0:
            parts.append(text[last:index].strip())
            last = index + 1
        index += 1
    tail = text[last:].strip()
    if tail:
        parts.append(tail)
    return parts


def parse_attribute_args(inner: str) -> dict[str, str | bool]:
    """Parse ``key = value, flag, key(value)`` attribute bodies."""

    parsed: dict[str, str | bool] = {}
    for part in split_top_level_commas(inner):
        if not part:
            continue
        key, eq, value = part.partition("=")
        key = key.strip()
        if not eq:
            paren = key.find("(")
            if paren != -1:
                parsed[key[:paren].strip()] = key[paren + 1 : -1].strip()
            else:
                parsed[key] = True
            continue
        value = value.strip()
        match = _STRING_RE.fullmatch(value)
        if match is not None:
            parsed[key] = unescape(match.group(1))
        elif value.startswith("'") and value.endswith("'") and len(value) == 3:
            parsed[key] = value[1]
        else:
            parsed[key] = value
    return parsed


@dataclass
class Attribute:
    name: str
    args: dict[str, str | bool]
    raw: str


@dataclass
class Field:
    name: str
    type_text: str
    docs: list[str]
    attrs: list[Attribute]
    line: int

    def attr(self, name: str) -> dict[str, str | bool]:
        merged: dict[str, str | bool] = {}
        for attribute in self.attrs:
            if attribute.name == name:
                merged.update(attribute.args)
        return merged


@dataclass
class Variant:
    name: str
    docs: list[str]
    attrs: list[Attribute]
    tuple_type: str | None
    fields: list[Field]
    line: int

    def attr(self, name: str) -> dict[str, str | bool]:
        merged: dict[str, str | bool] = {}
        for attribute in self.attrs:
            if attribute.name == name:
                merged.update(attribute.args)
        return merged


@dataclass
class Item:
    kind: str  # enum | struct
    name: str
    derives: set[str]
    docs: list[str]
    attrs: list[Attribute]
    path: str
    line: int
    variants: list[Variant] = field(default_factory=list)
    fields: list[Field] = field(default_factory=list)

    def attr(self, name: str) -> dict[str, str | bool]:
        merged: dict[str, str | bool] = {}
        for attribute in self.attrs:
            if attribute.name == name:
                merged.update(attribute.args)
        return merged


class Cursor:
    """Small scanner over an item body (variants or struct fields)."""

    def __init__(self, text: str, base_line: int) -> None:
        self.text = text
        self.index = 0
        self.base_line = base_line

    def line(self) -> int:
        return self.base_line + self.text.count("\n", 0, self.index)

    def skip_ws(self) -> None:
        while self.index < len(self.text) and self.text[self.index].isspace():
            self.index += 1

    def at_end(self) -> bool:
        self.skip_ws()
        return self.index >= len(self.text)

    def peek(self, literal: str) -> bool:
        self.skip_ws()
        return self.text.startswith(literal, self.index)

    def take_docs_and_attrs(self) -> tuple[list[str], list[Attribute]]:
        docs: list[str] = []
        attrs: list[Attribute] = []
        while True:
            self.skip_ws()
            if self.text.startswith("///", self.index):
                end = self.text.find("\n", self.index)
                end = len(self.text) if end == -1 else end
                docs.append(self.text[self.index + 3 : end].strip())
                self.index = end
                continue
            if self.text.startswith("//", self.index):
                end = self.text.find("\n", self.index)
                self.index = len(self.text) if end == -1 else end
                continue
            if self.text.startswith("#[", self.index):
                inner, after = scan_balanced(self.text, self.index + 1, "[", "]")
                self.index = after
                match = _IDENT_RE.match(inner.strip())
                if match is None:
                    raise ParseError(f"unparseable attribute {inner!r}")
                name = match.group(0)
                rest = inner.strip()[match.end() :].strip()
                args: dict[str, str | bool] = {}
                if rest.startswith("(") and rest.endswith(")"):
                    args = parse_attribute_args(rest[1:-1])
                attrs.append(Attribute(name, args, inner.strip()))
                continue
            return docs, attrs

    def take_ident(self) -> str:
        self.skip_ws()
        match = _IDENT_RE.match(self.text, self.index)
        if match is None:
            raise ParseError(
                f"expected identifier at line {self.line()}: {self.text[self.index:self.index + 40]!r}"
            )
        self.index = match.end()
        return match.group(0)

    def take_balanced(self, open_char: str, close_char: str) -> str:
        self.skip_ws()
        inner, after = scan_balanced(self.text, self.index, open_char, close_char)
        self.index = after
        return inner

    def take_type(self) -> str:
        """Consume a type expression up to the next top-level ``,``."""

        self.skip_ws()
        depth = 0
        start = self.index
        while self.index < len(self.text):
            ch = self.text[self.index]
            if ch in "<([":
                depth += 1
            elif ch in ">)]":
                depth -= 1
            elif ch == "," and depth == 0:
                break
            self.index += 1
        return " ".join(self.text[start : self.index].split())

    def take_comma(self) -> None:
        self.skip_ws()
        if self.index < len(self.text) and self.text[self.index] == ",":
            self.index += 1


def parse_fields(body: str, base_line: int) -> list[Field]:
    cursor = Cursor(body, base_line)
    fields: list[Field] = []
    while not cursor.at_end():
        docs, attrs = cursor.take_docs_and_attrs()
        if cursor.at_end():
            break
        line = cursor.line()
        name = cursor.take_ident()
        if name == "pub":
            if cursor.peek("("):
                cursor.take_balanced("(", ")")
            name = cursor.take_ident()
        cursor.skip_ws()
        if not cursor.peek(":"):
            raise ParseError(f"expected ':' after field {name} at line {line}")
        cursor.index += 1
        type_text = cursor.take_type()
        cursor.take_comma()
        fields.append(Field(name, type_text, docs, attrs, line))
    return fields


def parse_variants(body: str, base_line: int) -> list[Variant]:
    cursor = Cursor(body, base_line)
    variants: list[Variant] = []
    while not cursor.at_end():
        docs, attrs = cursor.take_docs_and_attrs()
        if cursor.at_end():
            break
        line = cursor.line()
        name = cursor.take_ident()
        tuple_type: str | None = None
        fields: list[Field] = []
        if cursor.peek("("):
            tuple_type = " ".join(cursor.take_balanced("(", ")").split())
        elif cursor.peek("{"):
            cursor.skip_ws()
            field_line = cursor.line()
            fields = parse_fields(cursor.take_balanced("{", "}"), field_line)
        cursor.take_comma()
        variants.append(Variant(name, docs, attrs, tuple_type, fields, line))
    return variants


def collect_items() -> dict[str, Item]:
    items: dict[str, Item] = {}
    for path in production_rust_files():
        text = blank_test_modules(path.read_text(encoding="utf-8"))
        if not any(f"{derive}" in text for derive in CLAP_DERIVES):
            continue
        for match in _DERIVE_RE.finditer(text):
            derives = {part.strip() for part in match.group("list").split(",")}
            if not derives & set(CLAP_DERIVES):
                continue
            # Walk backwards over docs; forwards over sibling attributes to
            # the item head.
            head_cursor = Cursor(text, 1)
            head_cursor.index = match.end()
            _docs_after, attrs_after = head_cursor.take_docs_and_attrs()
            head = _ITEM_HEAD_RE.match(text, head_cursor.index)
            if head is None:
                continue
            docs, attrs_before = _docs_and_attrs_before(text, match.start())
            attrs = attrs_before + attrs_after
            kind = head.group("kind")
            name = head.group("name")
            body_open = head.end()
            if body_open >= len(text) or text[body_open] != "{":
                continue
            body, _after = scan_balanced(text, body_open, "{", "}")
            body_line = text.count("\n", 0, body_open) + 1
            item = Item(
                kind=kind,
                name=name,
                derives=derives & set(CLAP_DERIVES),
                docs=docs,
                attrs=attrs,
                path=rel_posix(path),
                line=text.count("\n", 0, head.start()) + 1,
            )
            if kind == "enum":
                item.variants = parse_variants(body, body_line)
            else:
                item.fields = parse_fields(body, body_line)
            if name in items:
                raise ParseError(
                    f"duplicate clap item name {name} in {item.path} and {items[name].path}"
                )
            items[name] = item
    return items


def _docs_and_attrs_before(text: str, offset: int) -> tuple[list[str], list[Attribute]]:
    """Collect ``///`` docs and ``#[...]`` attributes directly above ``offset``."""

    lines = text[:offset].splitlines()
    docs: list[str] = []
    attrs: list[Attribute] = []
    cursor = len(lines) - 1
    while cursor >= 0:
        stripped = lines[cursor].strip()
        if stripped.startswith("///"):
            docs.append(stripped[3:].strip())
        elif stripped.startswith("#[") and stripped.endswith("]"):
            inner = stripped[2:-1]
            match = _IDENT_RE.match(inner)
            if match is None:
                break
            rest = inner[match.end() :].strip()
            args = parse_attribute_args(rest[1:-1]) if rest.startswith("(") else {}
            attrs.append(Attribute(match.group(0), args, inner))
        elif stripped.startswith("//") or not stripped:
            pass
        else:
            break
        cursor -= 1
    docs.reverse()
    attrs.reverse()
    return docs, attrs


# --------------------------------------------------------------------------- #
# clap semantics
# --------------------------------------------------------------------------- #


@dataclass
class ArgSpec:
    display: str  # e.g. `--channel <CHANNEL>` or `<PATH>`
    value: str  # value type / enum choices
    default: str
    description: str
    positional: bool
    required: bool


@dataclass
class CommandSpec:
    path: list[str]
    summary: str
    long_about: str
    after_help: str
    notes: list[str]
    args: list[ArgSpec]
    subcommands: list["CommandSpec"]
    subcommand_optional: bool
    source: str


def strip_generic(type_text: str, wrapper: str) -> str | None:
    if type_text.startswith(wrapper + "<") and type_text.endswith(">"):
        return type_text[len(wrapper) + 1 : -1].strip()
    return None


def last_segment(path: str) -> str:
    return path.split("::")[-1].strip()


def doc_summary(docs: list[str]) -> tuple[str, str]:
    """Split clap-style docs into (first paragraph, remaining paragraphs)."""

    paragraphs: list[list[str]] = [[]]
    for line in docs:
        if not line.strip():
            if paragraphs[-1]:
                paragraphs.append([])
            continue
        paragraphs[-1].append(line.strip())
    joined = [" ".join(paragraph) for paragraph in paragraphs if paragraph]
    if not joined:
        return "", ""
    return joined[0], " ".join(joined[1:])


class Renderer:
    def __init__(self, items: dict[str, Item]) -> None:
        self.items = items
        self.consts = _collect_str_consts()

    def value_enum_choices(self, type_name: str) -> list[str] | None:
        item = self.items.get(last_segment(type_name))
        if item is None or "ValueEnum" not in item.derives:
            return None
        choices: list[str] = []
        for variant in item.variants:
            value_attr = variant.attr("value")
            if value_attr.get("skip"):
                continue
            name = value_attr.get("name")
            choices.append(str(name) if isinstance(name, str) else kebab_case(variant.name))
        return choices

    def resolve_help(self, expr: str) -> str:
        const = self.consts.get(last_segment(expr))
        if const is not None:
            return const
        return f"(see `{expr}`)"

    def build_args(self, fields: list[Field], args: list[ArgSpec]) -> tuple[str | None, bool]:
        """Append argument specs; return (subcommand enum type, optional?)."""

        subcommand_type: str | None = None
        subcommand_optional = False
        for field_ in fields:
            command_attr = field_.attr("command")
            if command_attr.get("subcommand"):
                inner = strip_generic(field_.type_text, "Option")
                subcommand_optional = inner is not None
                subcommand_type = inner or field_.type_text
                continue
            if command_attr.get("flatten"):
                nested = self.items.get(last_segment(field_.type_text))
                if nested is None:
                    raise ParseError(f"unknown flattened Args type {field_.type_text}")
                nested_sub, nested_opt = self.build_args(nested.fields, args)
                if nested_sub is not None:
                    subcommand_type, subcommand_optional = nested_sub, nested_opt
                continue
            args.append(self.arg_spec(field_))
        return subcommand_type, subcommand_optional

    def arg_spec(self, field_: Field) -> ArgSpec:
        arg = field_.attr("arg")
        type_text = field_.type_text
        optional = False
        repeatable = False
        inner = strip_generic(type_text, "Option")
        if inner is not None:
            optional = True
            type_text = inner
        inner = strip_generic(type_text, "Vec")
        if inner is not None:
            repeatable = True
            type_text = inner

        arg_id = str(arg["id"]) if isinstance(arg.get("id"), str) else field_.name
        value_name = (
            str(arg["value_name"])
            if isinstance(arg.get("value_name"), str)
            else screaming_snake(arg_id)
        )
        action = str(arg.get("action", ""))
        is_flag = type_text == "bool" and "Set" not in action
        is_count = "Count" in action

        long_name: str | None = None
        if "long" in arg:
            long_name = str(arg["long"]) if isinstance(arg["long"], str) else kebab_case(field_.name)
        short_name: str | None = None
        if "short" in arg:
            short_name = str(arg["short"]) if isinstance(arg["short"], str) else field_.name[0]

        choices = self.value_enum_choices(type_text) if arg.get("value_enum") else None
        if choices is None and type_text == "bool" and not is_flag:
            choices = ["true", "false"]

        if choices is not None:
            value = "`" + "`, `".join(choices) + "`"
        elif is_flag or is_count:
            value = "flag"
        else:
            value = f"`{type_text}`"
        if repeatable:
            value += " (repeatable)"

        default = ""
        if "default_value" in arg:
            default = f"`{arg['default_value']}`"
        elif "default_value_t" in arg:
            raw = str(arg["default_value_t"])
            if choices is not None and "::" in raw:
                default = f"`{kebab_case(last_segment(raw))}`"
            else:
                default = f"`{raw}`"

        # clap derive: a non-Option, non-Vec, non-bool field without a default
        # is required; `Vec<T>` and `Option<T>` are optional unless
        # `required = true` is spelled out.
        required = bool(arg.get("required")) or (
            not optional and not repeatable and not is_flag and not is_count and not default
        )

        positional = long_name is None and short_name is None
        if positional:
            display = f"<{value_name}>" if required else f"[{value_name}]"
            if repeatable:
                display += "..."
        else:
            flags = []
            if short_name:
                flags.append(f"-{short_name}")
            if long_name:
                flags.append(f"--{long_name}")
            display = ", ".join(flags)
            if not is_flag and not is_count:
                display += f" <{value_name}>"
            if repeatable:
                display += "..."

        summary, rest = doc_summary(field_.docs)
        if isinstance(arg.get("help"), str):
            help_text = str(arg["help"])
            if _STRING_RE.fullmatch('"' + help_text + '"') is None or "::" in help_text:
                help_text = self.resolve_help(help_text)
            summary = help_text
        description = summary
        if rest:
            description = f"{summary} {rest}"
        notes: list[str] = []
        if isinstance(arg.get("alias"), str):
            notes.append(f"alias `--{arg['alias']}`")
        if isinstance(arg.get("visible_alias"), str):
            notes.append(f"alias `--{arg['visible_alias']}`")
        if arg.get("global"):
            notes.append("accepted before or after the subcommand")
        if arg.get("last"):
            notes.append("after `--`")
        if isinstance(arg.get("conflicts_with"), str):
            notes.append(f"conflicts with `{arg['conflicts_with']}`")
        if isinstance(arg.get("env"), str):
            notes.append(f"env `{arg['env']}`")
        if notes:
            description = f"{description} ({'; '.join(notes)})" if description else "; ".join(notes)
        return ArgSpec(display, value, default, description, positional, required)

    def command_from_variant(self, variant: Variant, parent: list[str]) -> CommandSpec | None:
        for attribute in variant.attrs:
            if attribute.name == "cfg" and "test" in attribute.raw:
                return None
        command_attr = variant.attr("command")
        name = str(command_attr["name"]) if isinstance(command_attr.get("name"), str) else kebab_case(variant.name)
        summary, long_about = doc_summary(variant.docs)
        if isinstance(command_attr.get("about"), str):
            summary = str(command_attr["about"])
        notes: list[str] = []
        if command_attr.get("hide"):
            notes.append("hidden from `--help`")
        for attribute in variant.attrs:
            if attribute.name == "cfg":
                notes.append(f"`#[{attribute.raw}]`")
        for key in ("alias", "visible_alias"):
            if isinstance(command_attr.get(key), str):
                notes.append(f"alias `{command_attr[key]}`")
        after_help = str(command_attr["after_help"]) if isinstance(command_attr.get("after_help"), str) else ""
        if after_help.startswith("Deprecated"):
            notes.insert(0, "deprecated")

        args: list[ArgSpec] = []
        fields = variant.fields
        if variant.tuple_type is not None:
            nested = self.items.get(last_segment(variant.tuple_type))
            if nested is None:
                raise ParseError(f"unknown Args type {variant.tuple_type} for {variant.name}")
            fields = nested.fields
        subcommand_type, subcommand_optional = self.build_args(fields, args)
        path = parent + [name]
        subcommands: list[CommandSpec] = []
        if subcommand_type is not None:
            subcommands = self.commands_from_enum(subcommand_type, path)
        return CommandSpec(
            path=path,
            summary=summary,
            long_about=long_about,
            after_help=after_help,
            notes=notes,
            args=args,
            subcommands=subcommands,
            subcommand_optional=subcommand_optional,
            source=f"{self.items[last_segment(variant.tuple_type)].path if variant.tuple_type else ''}",
        )

    def commands_from_enum(self, type_name: str, parent: list[str]) -> list[CommandSpec]:
        item = self.items.get(last_segment(type_name))
        if item is None or "Subcommand" not in item.derives:
            raise ParseError(f"unknown Subcommand enum {type_name}")
        commands: list[CommandSpec] = []
        for variant in item.variants:
            command = self.command_from_variant(variant, parent)
            if command is not None:
                command.source = f"{item.path}:{variant.line}"
                commands.append(command)
        return commands

    def root(self) -> tuple[Item, list[ArgSpec], list[CommandSpec]]:
        parsers = [item for item in self.items.values() if "Parser" in item.derives]
        if len(parsers) != 1:
            raise ParseError(f"expected exactly one #[derive(Parser)] item, found {len(parsers)}")
        parser = parsers[0]
        args: list[ArgSpec] = []
        subcommand_type, _optional = self.build_args(parser.fields, args)
        if subcommand_type is None:
            raise ParseError("root Parser has no #[command(subcommand)] field")
        return parser, args, self.commands_from_enum(subcommand_type, [])


def _collect_str_consts() -> dict[str, str]:
    """``pub const NAME: &str = "..."`` literals used as ``help = path::NAME``."""

    pattern = re.compile(
        r"const\s+(?P<ident>[A-Z][A-Z0-9_]*)\s*:\s*&(?:'static\s+)?str\s*=\s*\"(?P<value>(?:[^\"\\]|\\.)*)\""
    )
    consts: dict[str, str] = {}
    for path in production_rust_files():
        if "cli" not in path.parts and "discord" not in path.parts:
            continue
        text = blank_test_modules(path.read_text(encoding="utf-8"))
        for match in pattern.finditer(text):
            consts.setdefault(match.group("ident"), unescape(match.group("value")))
    return consts


# --------------------------------------------------------------------------- #
# Markdown rendering
# --------------------------------------------------------------------------- #


def markdown_cell(text: str) -> str:
    return " ".join(text.replace("|", "\\|").split())


def usage_line(command: CommandSpec) -> str:
    parts = [BINARY_NAME, *command.path]
    if command.subcommands:
        parts.append("[COMMAND]" if command.subcommand_optional else "<COMMAND>")
    options = [arg for arg in command.args if not arg.positional]
    if options:
        parts.append("[OPTIONS]")
    for arg in command.args:
        if arg.positional:
            parts.append(arg.display)
    return " ".join(parts)


def flatten(commands: list[CommandSpec]) -> list[CommandSpec]:
    ordered: list[CommandSpec] = []
    for command in commands:
        ordered.append(command)
        ordered.extend(flatten(command.subcommands))
    return ordered


def render_args_table(args: list[ArgSpec]) -> list[str]:
    lines = ["| Argument | Value | Default | Description |", "|---|---|---|---|"]
    for arg in args:
        display = f"`{arg.display}`"
        if arg.required and not arg.positional:
            display += " (required)"
        lines.append(
            f"| {display} | {markdown_cell(arg.value)} | {markdown_cell(arg.default)} | {markdown_cell(arg.description)} |"
        )
    return lines


def render_command(command: CommandSpec) -> list[str]:
    depth = min(len(command.path), 4)
    heading = "#" * (depth + 1)
    lines = [f"{heading} `{BINARY_NAME} {' '.join(command.path)}`", ""]
    if command.summary:
        lines.append(command.summary)
        lines.append("")
    if command.long_about:
        lines.append(command.long_about)
        lines.append("")
    if command.notes:
        lines.append("Notes: " + "; ".join(command.notes) + ".")
        lines.append("")
    lines.append(f"Usage: `{usage_line(command)}`")
    lines.append("")
    if command.args:
        lines.extend(render_args_table(command.args))
        lines.append("")
    if command.subcommands:
        lines.append("Subcommands:")
        lines.append("")
        for sub in command.subcommands:
            lines.append(f"- `{sub.path[-1]}` — {sub.summary}" if sub.summary else f"- `{sub.path[-1]}`")
        lines.append("")
    if command.after_help:
        lines.append(f"> {command.after_help}")
        lines.append("")
    for sub in command.subcommands:
        lines.extend(render_command(sub))
    return lines


def render(parser: Item, global_args: list[ArgSpec], commands: list[CommandSpec]) -> str:
    command_attr = parser.attr("command")
    about = str(command_attr.get("about", "")) if isinstance(command_attr.get("about"), str) else ""
    all_commands = flatten(commands)
    lines = [
        "# CLI Reference",
        "",
        f"<!-- Generated by `{GENERATOR_COMMAND}`. Do not edit by hand. -->",
        "",
        f"`{BINARY_NAME}` — {about}" if about else f"`{BINARY_NAME}`",
        "",
        f"Derived from the clap definitions in `{parser.path}` (and the `Args` /",
        "`Subcommand` types it references). Commands are listed in declaration",
        "order, matching `agentdesk --help`. Running `agentdesk` with no",
        "subcommand starts the server.",
        "",
        f"Regenerate with `{GENERATOR_COMMAND}`; CI fails when this file drifts.",
        "",
        f"- Top-level commands: {len(commands)}",
        f"- Commands including nested subcommands: {len(all_commands)}",
        "",
        "## Global options",
        "",
        *render_args_table(
            global_args
            + [
                ArgSpec("-h, --help", "flag", "", "Print help", False, False),
                ArgSpec("-V, --version", "flag", "", "Print version", False, False),
            ]
        ),
        "",
        "## Command index",
        "",
        "| Command | Summary | Notes |",
        "|---|---|---|",
    ]
    for command in all_commands:
        anchor = "-".join([BINARY_NAME, *command.path])
        lines.append(
            f"| [`{BINARY_NAME} {' '.join(command.path)}`](#{anchor}) | {markdown_cell(command.summary)} | {markdown_cell('; '.join(command.notes))} |"
        )
    lines.append("")
    lines.append("## Commands")
    lines.append("")
    for command in commands:
        lines.extend(render_command(command))
    return "\n".join(lines).rstrip("\n") + "\n"


def generate() -> tuple[str, list[CommandSpec]]:
    renderer = Renderer(collect_items())
    parser, global_args, commands = renderer.root()
    return render(parser, global_args, commands), commands


def main(argv: list[str] | None = None) -> int:
    arg_parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    arg_parser.add_argument(
        "--output",
        type=Path,
        default=OUTPUT_DOC,
        help="destination markdown path (default: docs/generated/cli-reference.md)",
    )
    arg_parser.add_argument(
        "--stdout",
        action="store_true",
        help="print the rendered document instead of writing it",
    )
    args = arg_parser.parse_args(argv)
    rendered, commands = generate()
    if args.stdout:
        sys.stdout.write(rendered)
        return 0
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(rendered, encoding="utf-8")
    print(
        f"wrote {args.output.relative_to(REPO_ROOT).as_posix()}: "
        f"{len(commands)} top-level commands, {len(flatten(commands))} including nested"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
