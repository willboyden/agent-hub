"""M6: hardened YAML loading and bounded reads on the discover path."""
from __future__ import annotations

from fakes import FakeFS

from agent_hub.adapters import _util as u
from agent_hub.adapters.claude_code import ClaudeCodeAdapter
from agent_hub.adapters.hermes import HermesAdapter

ALIAS_BOMB = "a: &a [1,2,3,4,5,6,7,8,9]\nb: &b [*a,*a,*a,*a,*a,*a,*a,*a,*a]\nc: &c [*b,*b,*b,*b,*b,*b,*b,*b,*b]\nd: [*c,*c,*c,*c,*c,*c,*c,*c,*c]\n"
DEEP = "x: " + "[" * 200 + "]" * 200 + "\n"


def test_frontmatter_rejects_alias_bomb_and_anchors_and_depth() -> None:
    for head in (ALIAS_BOMB, "a: &x 1\nb: *x\n", DEEP, "k: 1\nk: 2\n"):
        assert u.parse_frontmatter(f"---\n{head}---\nbody\n")[0] == {}
    assert u.parse_frontmatter("---\nname: ok\n---\nb")[0] == {"name": "ok"}


def test_frontmatter_rejects_oversized_head() -> None:
    assert u.parse_frontmatter("---\ndescription: " + "x" * 300_000 + "\n---\nb")[0] == {}


def test_cap_str_bounds_everything() -> None:
    assert len(u.cap_str("x" * 10**6)) == u.MAX_DESC_ERR and u.cap_str(["a"]) == "" and u.cap_str(None) == "" and u.cap_str(7) == "7"


def test_discover_agent_with_alias_bomb_is_skipped_not_expanded() -> None:
    a = ClaudeCodeAdapter()
    cfg = a.default_config()
    cfg.roots = {"project": "/p"}
    fs = FakeFS({"/p/.claude/agents/bomb.md": f"---\n{ALIAS_BOMB}---\nbody".encode()})
    d = a.discover(cfg, fs)
    assert d.agents[0].description == "" and d.agents[0].capabilities == []


def test_hermes_config_alias_bomb_and_oversize_rejected() -> None:
    a = HermesAdapter()
    cfg = a.default_config()
    cfg.roots = {"stage": "/s"}
    cfg.params.update(source_root="/lab", source_dirs=[{"kind": "config", "path": "c.yaml"}])
    d = a.discover(cfg, FakeFS({"/lab/c.yaml": ALIAS_BOMB.encode()}))
    assert d.mcp_servers == [] and any("safe YAML loader" in n for n in d.notes)
    d2 = a.discover(cfg, FakeFS({"/lab/c.yaml": b"mcp_servers: {}\n" + b"#" * (u.MAX_READ_FILE_BYTES + 1)}))
    assert any("read cap" in n for n in d2.notes)


class SizedFS(FakeFS):
    """FileSystem that offers size(): oversize files must NOT be read at all."""

    def __init__(self, files: dict[str, bytes], sizes: dict[str, int]) -> None:
        super().__init__(files)
        self.sizes = sizes
        self.read: list[str] = []

    def size(self, path: str) -> int:
        return self.sizes.get(path, len(self.files[path]))

    def read_bytes(self, path: str) -> bytes:
        self.read.append(path)
        return super().read_bytes(path)


def test_read_tree_stats_before_reading() -> None:
    fs = SizedFS({"/b/ok.txt": b"1", "/b/big.bin": b"x"}, {"/b/big.bin": u.MAX_READ_FILE_BYTES + 1})
    notes: list[str] = []
    assert u.read_tree(fs, "/b", notes=notes) == {"ok.txt": b"1"}
    assert "/b/big.bin" not in fs.read and any("big.bin" in n for n in notes)


def test_read_tree_caps_without_size_port_file_count_and_total() -> None:
    notes: list[str] = []
    fs = FakeFS({"/b/big": b"x" * (u.MAX_READ_FILE_BYTES + 1), "/b/s": b"y"})
    assert u.read_tree(fs, "/b", notes=notes) == {"s": b"y"} and any("read cap" in n for n in notes)
    many = FakeFS({f"/m/f{i:04}": b"1" for i in range(450)})
    n2: list[str] = []
    assert len(u.read_tree(many, "/m", notes=n2)) == u.MAX_DISCOVER_FILES and any("file limit" in n for n in n2)
    chunk = b"z" * (u.MAX_READ_FILE_BYTES - 1)
    tot = FakeFS({f"/t/f{i:02}": chunk for i in range(40)})
    n3: list[str] = []
    got = u.read_tree(tot, "/t", notes=n3)
    assert sum(map(len, got.values())) <= u.MAX_READ_TOTAL_BYTES and any("total read cap" in n for n in n3)
