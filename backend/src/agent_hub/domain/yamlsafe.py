"""Safe YAML: no python tags, no anchors/aliases (billion-laughs), no duplicate keys, bounded size/depth/nodes."""
from __future__ import annotations

from typing import Any

import yaml

MAX_DEPTH = 12
MAX_NODES = 5000


class YamlError(ValueError):
    pass


class _StrictLoader(yaml.SafeLoader):
    def compose_node(self, parent: Any, index: Any) -> Any:
        if self.check_event(yaml.AliasEvent):
            raise YamlError("YAML aliases are not allowed")
        ev = self.peek_event()  # type: ignore[no-untyped-call]
        if getattr(ev, "anchor", None) is not None:
            raise YamlError("YAML anchors are not allowed")
        return super().compose_node(parent, index)

    def construct_mapping(self, node: Any, deep: bool = False) -> dict[Any, Any]:
        seen: set[Any] = set()
        for key_node, _ in node.value:
            key = self.construct_object(key_node, deep=True)
            if key in seen:
                raise YamlError(f"duplicate key: {key!r}")
            seen.add(key)
        return super().construct_mapping(node, deep)


def _walk(obj: Any, depth: int, counter: list[int]) -> None:
    counter[0] += 1
    if depth > MAX_DEPTH:
        raise YamlError("document nested too deeply")
    if counter[0] > MAX_NODES:
        raise YamlError("document has too many nodes")
    if isinstance(obj, dict):
        for k, v in obj.items():
            _walk(k, depth + 1, counter)
            _walk(v, depth + 1, counter)
    elif isinstance(obj, list):
        for v in obj:
            _walk(v, depth + 1, counter)


def load(text: str, *, max_bytes: int = 256 * 1024) -> Any:
    if len(text.encode("utf-8", "replace")) > max_bytes:
        raise YamlError("document too large")
    try:
        data = yaml.load(text, Loader=_StrictLoader)  # noqa: S506 - SafeLoader subclass
    except yaml.YAMLError as exc:
        raise YamlError(f"invalid YAML: {str(exc).splitlines()[0] if str(exc) else 'parse error'}") from exc
    _walk(data, 0, [0])
    return data


def dump(data: Any) -> str:
    return yaml.safe_dump(data, sort_keys=False, allow_unicode=True, default_flow_style=False, width=100)


def split_frontmatter(text: str) -> tuple[dict[str, Any], str]:
    """`---\\n<yaml>\\n---\\n<body>` -> (meta, body). The body is returned untouched (it is untrusted text)."""
    if not text.startswith("---\n") and not text.startswith("---\r\n"):
        raise YamlError("missing YAML frontmatter")
    lines = text.split("\n")
    end = None
    for i in range(1, len(lines)):
        if lines[i].rstrip("\r") == "---":
            end = i
            break
    if end is None:
        raise YamlError("unterminated YAML frontmatter")
    meta = load("\n".join(lines[1:end]))
    if meta is None:
        meta = {}
    if not isinstance(meta, dict):
        raise YamlError("frontmatter must be a mapping")
    return meta, "\n".join(lines[end + 1:])


def join_frontmatter(meta: dict[str, Any], body: str) -> str:
    return "---\n" + dump(meta) + "---\n" + body
