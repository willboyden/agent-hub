"""ContentStore: strict, schema-validated read/write access to a content tree (the git working tree, or a read-only
snapshot of HEAD). Nothing here commits; edits only change files (ARCHITECTURE decision 3)."""
from __future__ import annotations

import base64
import contextlib
import shutil
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, TypeVar

from pydantic import BaseModel, ValidationError

from agent_hub.domain import yamlsafe
from agent_hub.domain.errors import BadRequest, Conflict, NotFound, Unprocessable
from agent_hub.domain.ids import CONCERNS, FROM_PLURAL, KINDS, PLURAL, valid_id
from agent_hub.domain.models import (
    AgentMeta,
    ClientDoc,
    CollectionDoc,
    HubMeta,
    InstructionMeta,
    Issue,
    McpDoc,
    MemoryMeta,
    ProfileDoc,
    RuleDoc,
    Sidecar,
    SkillMeta,
    issues_from,
)
from agent_hub.domain.pathsafe import PathError, check_relpath, resolve_under
from agent_hub.services.attest import Attestor
from agent_hub.services.common import atomic_write

SKILL_RESERVED = {"SKILL.md", "hub.yaml"}
MAX_SKILL_FILES = 200
_MD: dict[str, tuple[str, type[BaseModel]]] = {"agent": ("agents", AgentMeta), "instruction": ("instructions", InstructionMeta),
       "memory": ("memory", MemoryMeta)}
_YAML: dict[str, tuple[str, type[BaseModel]]] = {"mcp": ("mcp", McpDoc), "rule": ("rules", RuleDoc)}
SEED_DIRS = ("instructions", "skills", "agents", "mcp", "rules", "memory", "inbox", "collections", "clients", "profiles")
DEFAULT_ENDPOINTS = {"router": "http://127.0.0.1:4000/v1", "knowledge": "http://127.0.0.1:8795"}


@dataclass
class Item:
    kind: str
    name: str
    meta: BaseModel | None = None
    body: str = ""
    files: dict[str, bytes] = field(default_factory=dict)       # skills: every file except the hub.yaml sidecar
    sidecar: Sidecar | None = None
    issues: list[Issue] = field(default_factory=list)
    updated: float = 0.0

    @property
    def valid(self) -> bool:
        return self.meta is not None and not self.issues

    def _get(self, attr: str, default: Any = None) -> Any:
        return getattr(self.meta, attr, default) if self.meta is not None else default

    @property
    def title(self) -> str:
        return str(self._get("title") or self._get("description") or self.name)[:200]

    @property
    def description(self) -> str:
        return str(self._get("description") or self._get("title") or "")

    @property
    def tags(self) -> list[str]:
        tags = list(self.sidecar.tags) if self.sidecar else list(self._get("tags", []) or [])
        return tags

    @property
    def groups(self) -> list[str]:
        if self.sidecar:
            return list(self.sidecar.groups)
        return list(self._get("groups", []) or [])

    @property
    def source(self) -> str:
        return self.sidecar.source if self.sidecar else ""

    @property
    def applies_to(self) -> list[str]:
        return list(self._get("applies_to", []) or [])


M = TypeVar("M", bound=BaseModel)


def meta_as[X: BaseModel](item: Item, cls: type[X]) -> X:
    """Typed access to a VALID item's meta (resolution only ever sees valid items)."""
    assert isinstance(item.meta, cls), f"{item.kind}:{item.name} has no {cls.__name__}"
    return item.meta


@dataclass
class Catalog:
    items: dict[tuple[str, str], Item] = field(default_factory=dict)
    collections: dict[str, CollectionDoc] = field(default_factory=dict)
    clients: dict[str, ClientDoc] = field(default_factory=dict)
    profiles: dict[str, ProfileDoc] = field(default_factory=dict)
    endpoints: dict[str, str] = field(default_factory=dict)
    problems: list[Issue] = field(default_factory=list)

    def of_kind(self, kind: str) -> list[Item]:
        return sorted((i for (k, _), i in self.items.items() if k == kind), key=lambda i: i.name)


def validate_endpoints(data: Any) -> dict[str, str]:
    if not isinstance(data, dict):
        raise ValueError("endpoints must be a mapping")
    out: dict[str, str] = {}
    for k, v in data.items():
        if not isinstance(k, str) or not (k[:1].isalpha() and k.replace("_", "a").isalnum() and k == k.lower() and len(k) <= 32):
            raise ValueError(f"bad endpoint name: {str(k)[:32]!r}")
        if not isinstance(v, str) or not v.startswith(("http://", "https://")) or len(v) > 300:
            raise ValueError(f"endpoint {k} must be an http(s) URL")
        out[k] = v
    return out


class ContentStore:
    def __init__(self, root: Path, *, max_file: int = 256 * 1024, max_skill: int = 4 * 1024 * 1024,
                 attestor: Attestor | None = None) -> None:
        self.attestor = attestor
        self.root = root
        self.max_file = max_file
        self.max_skill = max_skill

    # -- low level -----------------------------------------------------------------------------
    def _p(self, rel: str) -> Path:
        try:
            return resolve_under(self.root, rel)
        except PathError as exc:
            raise BadRequest(f"unsafe content path: {exc}", code="unsafe_path") from exc

    def _read(self, rel: str) -> str:
        p = self._p(rel)
        st = p.lstat()
        if not p.is_file() or p.is_symlink():
            raise NotFound(f"no such file: {rel}")
        if st.st_size > self.max_file:
            raise Unprocessable(f"{rel} exceeds {self.max_file} bytes", code="file_too_large")
        try:
            return p.read_text(encoding="utf-8")
        except UnicodeDecodeError as exc:
            raise Unprocessable(f"{rel} is not valid UTF-8") from exc

    def _write(self, rel: str, data: str | bytes) -> None:
        raw = data.encode("utf-8") if isinstance(data, str) else data
        if len(raw) > (self.max_skill if rel.startswith("skills/") else self.max_file):
            raise Unprocessable(f"{rel} is too large", code="file_too_large")
        atomic_write(self._p(rel), raw)

    def exists(self, rel: str) -> bool:
        with contextlib.suppress(BadRequest):
            return self._p(rel).exists()
        return False

    def _names(self, sub: str, suffix: str) -> list[str]:
        d = self.root / sub
        if not d.is_dir() or d.is_symlink():
            return []
        out = []
        for f in sorted(d.iterdir()):
            if f.is_symlink() or not f.is_file() or not f.name.endswith(suffix):
                continue
            stem = f.name[: -len(suffix)]
            if valid_id(stem):
                out.append(stem)
        return out

    # -- seeds -----------------------------------------------------------------------------------
    def seed(self) -> list[str]:
        created = []
        for d in SEED_DIRS:
            p = self.root / d
            p.mkdir(parents=True, exist_ok=True)
            keep = p / ".gitkeep"
            if d != "inbox" and not keep.exists():        # inbox holds proposals that are never committed blindly
                keep.write_text("")
                created.append(f"{d}/.gitkeep")
        if not (self.root / "hub.yaml").exists():
            self._write("hub.yaml", yamlsafe.dump(HubMeta().model_dump()))
            created.append("hub.yaml")
        if not (self.root / "endpoints.yaml").exists():
            self._write("endpoints.yaml", yamlsafe.dump(DEFAULT_ENDPOINTS))
            created.append("endpoints.yaml")
        return created

    # -- items: read -------------------------------------------------------------------------------
    def names(self, kind: str) -> list[str]:
        if kind == "skill":
            d = self.root / "skills"
            if not d.is_dir():
                return []
            return sorted(f.name for f in d.iterdir() if f.is_dir() and not f.is_symlink() and valid_id(f.name))
        if kind in _MD:
            return self._names(_MD[kind][0], ".md")
        if kind in _YAML:
            return self._names(_YAML[kind][0], ".yaml")
        raise BadRequest(f"unknown kind: {kind}")

    def list_items(self, kind: str) -> list[Item]:
        return [self.load(kind, n) for n in self.names(kind)]

    def load(self, kind: str, name: str) -> Item:
        """Never raises for bad content: the item carries `issues` instead (so one bad file cannot hide the rest)."""
        if not valid_id(name):
            raise BadRequest("invalid name", code="invalid_id")
        try:
            if kind == "skill":
                return self._load_skill(name)
            if kind in _MD:
                sub, model = _MD[kind]
                return self._load_md(kind, name, f"{sub}/{name}.md", model)
            if kind in _YAML:
                sub, model = _YAML[kind]
                return self._load_yaml(kind, name, f"{sub}/{name}.yaml", model)
        except (yamlsafe.YamlError, Unprocessable) as exc:
            return Item(kind, name, issues=[Issue(path="", message=str(exc))])
        raise BadRequest(f"unknown kind: {kind}")

    def _key_of(self, kind: str) -> str:
        return "id" if kind in ("instruction", "rule", "memory") else "name"

    def _mtime(self, rel: str) -> float:
        with contextlib.suppress(OSError):
            return self._p(rel).stat().st_mtime
        return 0.0

    def _load_md(self, kind: str, name: str, rel: str, model: type[BaseModel]) -> Item:
        if not self.exists(rel):
            raise NotFound(f"no such {kind}: {name}")
        meta_raw, body = yamlsafe.split_frontmatter(self._read(rel))
        item = Item(kind, name, body=body, updated=self._mtime(rel))
        try:
            item.meta = model.model_validate(meta_raw)
        except ValidationError as exc:
            item.issues = issues_from(exc)
            return item
        if getattr(item.meta, self._key_of(kind)) != name:
            item.issues.append(Issue(path=self._key_of(kind), message="does not match the file name"))
        return item

    def _load_yaml(self, kind: str, name: str, rel: str, model: type[BaseModel]) -> Item:
        if not self.exists(rel):
            raise NotFound(f"no such {kind}: {name}")
        item = Item(kind, name, updated=self._mtime(rel))
        try:
            item.meta = model.model_validate(yamlsafe.load(self._read(rel)))
        except ValidationError as exc:
            item.issues = issues_from(exc)
            return item
        if getattr(item.meta, self._key_of(kind)) != name:
            item.issues.append(Issue(path=self._key_of(kind), message="does not match the file name"))
        return item

    def _skill_files(self, name: str) -> dict[str, bytes]:
        base = self._p(f"skills/{name}")
        if not base.is_dir():
            raise NotFound(f"no such skill: {name}")
        files: dict[str, bytes] = {}
        total = 0
        for f in sorted(base.rglob("*")):
            if f.is_symlink() or not f.is_file():
                continue
            rel = f.relative_to(base).as_posix()
            if rel == "hub.yaml":
                continue
            try:
                check_relpath(rel)
            except PathError:
                continue
            size = f.stat().st_size
            total += size
            if total > self.max_skill or len(files) >= MAX_SKILL_FILES:
                raise Unprocessable("skill is too large", code="skill_too_large")
            files[rel] = f.read_bytes()
        return files

    def _load_skill(self, name: str) -> Item:
        files = self._skill_files(name)
        item = Item("skill", name, files=files, updated=max((self._mtime(f"skills/{name}/{r}") for r in files), default=0.0))
        raw = files.get("SKILL.md")
        if raw is None:
            item.issues.append(Issue(path="SKILL.md", message="missing"))
            return item
        if len(raw) > self.max_file:
            item.issues.append(Issue(path="SKILL.md", message="too large"))
            return item
        try:
            meta_raw, item.body = yamlsafe.split_frontmatter(raw.decode("utf-8"))
        except (UnicodeDecodeError, yamlsafe.YamlError) as exc:
            item.issues.append(Issue(path="SKILL.md", message=str(exc),
                                     fix_hint="Quote frontmatter values that contain ': ' or start with special characters, "
                                              'e.g. description: "Use when: ..."'))
            return item
        try:
            item.meta = SkillMeta.model_validate(meta_raw)
        except ValidationError as exc:
            item.issues = issues_from(exc)
            return item
        if item.meta.name != name:
            item.issues.append(Issue(path="name", message="does not match the directory name"))
        side = f"skills/{name}/hub.yaml"
        if self.exists(side):
            try:
                item.sidecar = Sidecar.model_validate(yamlsafe.load(self._read(side)) or {})
            except (ValidationError, yamlsafe.YamlError) as exc:
                item.issues.append(Issue(path="hub.yaml", message=str(exc)[:200]))
        else:
            item.sidecar = Sidecar()
        return item

    # -- items: write ------------------------------------------------------------------------------
    def put(self, kind: str, name: str, payload: dict[str, Any], *, must_exist: bool | None = None) -> Item:
        if not valid_id(name):
            raise BadRequest("invalid name", code="invalid_id")
        exists = name in self.names(kind)
        if must_exist is True and not exists:
            raise NotFound(f"no such {kind}: {name}")
        if must_exist is False and exists:
            raise Conflict(f"{kind} already exists: {name}", code="already_exists")
        data = dict(payload)
        try:
            if kind == "skill":
                self._put_skill(name, data)
            elif kind in _MD:
                sub, model = _MD[kind]
                body = str(data.pop("body", ""))
                key = self._key_of(kind)
                self._check_key(data, key, name)
                meta = model.model_validate(data)
                self._write(f"{sub}/{name}.md", yamlsafe.join_frontmatter(meta.model_dump(mode="json", exclude_none=True), body))
            elif kind in _YAML:
                sub, model = _YAML[kind]
                self._check_key(data, self._key_of(kind), name)
                if kind == "mcp":
                    self._attest_mcp(name, data, exists)
                meta = model.model_validate(data)
                self._write(f"{sub}/{name}.yaml", yamlsafe.dump(meta.model_dump(mode="json", exclude_none=True)))
            else:
                raise BadRequest(f"unknown kind: {kind}")
        except ValidationError as exc:
            raise Unprocessable("content failed validation", errors=[i.model_dump() for i in issues_from(exc)]) from exc
        return self.load(kind, name)

    def _attest_mcp(self, name: str, data: dict[str, Any], exists: bool) -> None:
        """`clean` is bound to a digest of what was scanned. Setting it on a NEW or previously-unscanned server attests to the
        current fields; re-sending `clean` for an edited server keeps the OLD digest, so the edit invalidates it."""
        if data.get("scan_status") != "clean":
            data.pop("scan_digest", None)
            return
        if data.get("scan_digest"):
            return
        prev = self.load("mcp", name) if exists else None
        old = getattr(prev.meta, "scan_digest", None) if prev and prev.meta and getattr(prev.meta, "scan_status", "") == "clean" else None
        if old:
            data["scan_digest"] = old
        elif self.attestor is not None:
            data["scan_digest"] = self.attestor.digest(data.get("command"), data.get("args"), data.get("url"),
                                                       data.get("pinned_ref"), data.get("transport"))
        else:
            data.pop("scan_digest", None)

    def attest_mcp(self, name: str) -> Item:
        """The operator has actually scanned this server: bind `clean` to its CURRENT fields (needs the attestation key)."""
        if self.attestor is None:
            raise BadRequest("no attestation key is configured")
        item = self.load("mcp", name)
        if item.meta is None:
            raise Unprocessable("mcp item is invalid; fix it before attesting")
        d = item.meta.model_dump(mode="json", exclude_none=True)
        d["scan_status"] = "clean"
        d["scan_digest"] = self.attestor.digest(d.get("command"), d.get("args"), d.get("url"), d.get("pinned_ref"), d.get("transport"))
        return self.put("mcp", name, d, must_exist=True)

    @staticmethod
    def _check_key(data: dict[str, Any], key: str, name: str) -> None:
        if data.get(key, name) != name:
            raise Unprocessable(f"`{key}` in the body must match the URL ({name})")
        data[key] = name

    def _put_skill(self, name: str, data: dict[str, Any]) -> None:
        body = str(data.pop("body", ""))
        files_text = data.pop("files", None)
        files_b64 = data.pop("files_b64", None)
        sidecar_in = {k: data.pop(k) for k in ("groups", "tags", "notes", "source", "provenance") if k in data}
        self._check_key(data, "name", name)
        meta = SkillMeta.model_validate(data)  # extras (allowed-tools, ...) pass through
        existing_side = Sidecar()
        if self.exists(f"skills/{name}/hub.yaml"):
            with contextlib.suppress(ValidationError, yamlsafe.YamlError):
                existing_side = Sidecar.model_validate(yamlsafe.load(self._read(f"skills/{name}/hub.yaml")) or {})
        side = Sidecar.model_validate({**existing_side.model_dump(), **sidecar_in})
        extra: dict[str, bytes] | None = None
        if files_text is not None or files_b64 is not None:
            extra = {}
            for rel, txt in (files_text or {}).items():
                extra[self._extra_path(rel)] = str(txt).encode("utf-8")
            for rel, b64 in (files_b64 or {}).items():
                try:
                    extra[self._extra_path(rel)] = base64.b64decode(str(b64), validate=True)
                except ValueError as exc:
                    raise Unprocessable(f"invalid base64 for {rel[:60]}") from exc
            if len(extra) > MAX_SKILL_FILES or sum(len(v) for v in extra.values()) > self.max_skill:
                raise Unprocessable("skill is too large", code="skill_too_large")
        md = yamlsafe.join_frontmatter(meta.model_dump(mode="json", exclude_none=True, by_alias=True), body)
        self._write(f"skills/{name}/SKILL.md", md)
        self._write(f"skills/{name}/hub.yaml", yamlsafe.dump(side.model_dump(mode="json")))
        if extra is not None:
            for rel, blob in extra.items():
                self._write(f"skills/{name}/{rel}", blob)
            base = self._p(f"skills/{name}")
            for f in sorted(base.rglob("*"), reverse=True):
                rel = f.relative_to(base).as_posix()
                if f.is_file() and not f.is_symlink() and rel not in extra and rel not in SKILL_RESERVED:
                    f.unlink()
                elif f.is_dir() and not any(f.iterdir()):
                    f.rmdir()

    @staticmethod
    def _extra_path(rel: str) -> str:
        try:
            check_relpath(rel)
        except PathError as exc:
            raise Unprocessable(f"bad file path {rel[:60]!r}: {exc}") from exc
        if rel in SKILL_RESERVED or rel.split("/")[0] in (".git",):
            raise Unprocessable(f"{rel} is reserved")
        return rel

    def set_sidecar(self, name: str, side: Sidecar) -> None:
        if name not in self.names("skill"):
            raise NotFound(f"no such skill: {name}")
        self._write(f"skills/{name}/hub.yaml", yamlsafe.dump(side.model_dump(mode="json")))

    def write_skill_files(self, name: str, files: dict[str, bytes], sidecar: Sidecar) -> None:
        """Import path: SKILL.md and extras verbatim (as the client had them), plus a sidecar."""
        for rel, blob in files.items():
            if rel != "SKILL.md":
                self._extra_path(rel)
            self._write(f"skills/{name}/{rel}", blob)
        self._write(f"skills/{name}/hub.yaml", yamlsafe.dump(sidecar.model_dump(mode="json")))

    def delete(self, kind: str, name: str) -> None:
        if name not in self.names(kind):
            raise NotFound(f"no such {kind}: {name}")
        if kind == "skill":
            d = self._p(f"skills/{name}")
            shutil.rmtree(d)
        else:
            sub, ext = (_MD[kind][0], ".md") if kind in _MD else (_YAML[kind][0], ".yaml")
            self._p(f"{sub}/{name}{ext}").unlink()
        self._drop_refs(kind, name)

    def remove_skill_dir(self, name: str) -> None:
        """Delete a skill's files WITHOUT touching collection/profile references (used when replacing on import)."""
        if name in self.names("skill"):
            shutil.rmtree(self._p(f"skills/{name}"))

    def _drop_refs(self, kind: str, name: str) -> None:
        for coll in self.list_collections():
            kept = [m for m in coll.members if not (m.kind == kind and m.name == name)]
            if len(kept) != len(coll.members):
                coll.members = kept
                self.put_collection(coll.id, coll.model_dump(mode="json", exclude={"id"}))
        plural = PLURAL[kind]
        for cid in self.names_of("profiles"):
            prof = self.get_profile(cid)
            changed = False
            for section in (prof.enable, prof.disable):
                lst: list[str] = getattr(section, plural)
                if name in lst:
                    setattr(section, plural, [x for x in lst if x != name])
                    changed = True
            if changed:
                self.put_profile(cid, prof)

    def duplicate_skill(self, name: str, new_name: str) -> Item:
        if not valid_id(new_name):
            raise BadRequest("invalid new name", code="invalid_id")
        if new_name in self.names("skill"):
            raise Conflict("target already exists", code="already_exists")
        src = self.load("skill", name)
        if src.meta is None:
            raise Unprocessable("source skill is invalid")
        files = dict(src.files)
        meta_raw, body = yamlsafe.split_frontmatter(files["SKILL.md"].decode("utf-8"))
        meta_raw["name"] = new_name
        files["SKILL.md"] = yamlsafe.join_frontmatter(meta_raw, body).encode("utf-8")
        side = (src.sidecar or Sidecar()).model_copy(update={"source": f"duplicate:{name}"})
        self.write_skill_files(new_name, files, side)
        return self.load("skill", new_name)

    # -- collections / clients / profiles ------------------------------------------------------------
    def names_of(self, sub: str) -> list[str]:
        return self._names(sub, ".yaml")

    def list_collections(self) -> list[CollectionDoc]:
        out = []
        for n in self.names_of("collections"):
            with contextlib.suppress(ValidationError, yamlsafe.YamlError, Unprocessable):
                out.append(self.get_collection(n))
        return sorted(out, key=lambda c: (c.order, c.id))

    def get_collection(self, cid: str) -> CollectionDoc:
        return self._get_doc("collections", cid, CollectionDoc, "collection")

    def put_collection(self, cid: str, payload: dict[str, Any]) -> CollectionDoc:
        return self._put_doc("collections", cid, payload, CollectionDoc)

    def delete_collection(self, cid: str) -> None:
        if cid not in self.names_of("collections"):
            raise NotFound(f"no such collection: {cid}")
        self._p(f"collections/{cid}.yaml").unlink()
        for pid in self.names_of("profiles"):
            prof = self.get_profile(pid)
            if cid in prof.collections:
                prof.collections = [c for c in prof.collections if c != cid]
                self.put_profile(pid, prof)

    def get_client(self, cid: str) -> ClientDoc:
        return self._get_doc("clients", cid, ClientDoc, "client")

    def put_client(self, cid: str, payload: dict[str, Any]) -> ClientDoc:
        return self._put_doc("clients", cid, payload, ClientDoc)

    def delete_client(self, cid: str) -> None:
        if cid not in self.names_of("clients"):
            raise NotFound(f"no such client: {cid}")
        self._p(f"clients/{cid}.yaml").unlink()
        if self.exists(f"profiles/{cid}.yaml"):
            self._p(f"profiles/{cid}.yaml").unlink()

    def get_profile(self, client: str) -> ProfileDoc:
        if not valid_id(client):
            raise BadRequest("invalid client id", code="invalid_id")
        if not self.exists(f"profiles/{client}.yaml"):
            return ProfileDoc()
        return self._get_doc("profiles", client, ProfileDoc, "profile")

    def put_profile(self, client: str, payload: dict[str, Any] | ProfileDoc) -> ProfileDoc:
        data = payload.model_dump(mode="json") if isinstance(payload, ProfileDoc) else payload
        return self._put_doc("profiles", client, data, ProfileDoc)

    def _get_doc(self, sub: str, name: str, model: type[M], label: str) -> M:
        if not valid_id(name):
            raise BadRequest("invalid id", code="invalid_id")
        if not self.exists(f"{sub}/{name}.yaml"):
            raise NotFound(f"no such {label}: {name}")
        try:
            doc = model.model_validate(yamlsafe.load(self._read(f"{sub}/{name}.yaml")) or {})
        except ValidationError as exc:
            raise Unprocessable(f"{label} {name} is invalid", errors=[i.model_dump() for i in issues_from(exc)]) from exc
        except yamlsafe.YamlError as exc:
            raise Unprocessable(f"{label} {name} is invalid: {exc}") from exc
        if hasattr(doc, "id") and doc.id != name:
            raise Unprocessable(f"{label} id does not match the file name")
        return doc

    def _put_doc(self, sub: str, name: str, payload: dict[str, Any], model: type[M]) -> M:
        if not valid_id(name):
            raise BadRequest("invalid id", code="invalid_id")
        data = dict(payload)
        if "id" in model.model_fields:
            if data.get("id", name) != name:
                raise Unprocessable("`id` in the body must match the URL")
            data["id"] = name
        try:
            doc = model.model_validate(data)
        except ValidationError as exc:
            raise Unprocessable("content failed validation", errors=[i.model_dump() for i in issues_from(exc)]) from exc
        self._write(f"{sub}/{name}.yaml", yamlsafe.dump(doc.model_dump(mode="json", exclude_none=True)))
        return doc

    # -- endpoints / whole catalog -------------------------------------------------------------------
    def endpoints(self) -> dict[str, str]:
        if not self.exists("endpoints.yaml"):
            return {}
        return validate_endpoints(yamlsafe.load(self._read("endpoints.yaml")) or {})

    def put_endpoints(self, data: dict[str, Any]) -> dict[str, str]:
        try:
            clean = validate_endpoints(data)
        except ValueError as exc:
            raise Unprocessable(str(exc)) from exc
        self._write("endpoints.yaml", yamlsafe.dump(clean))
        return clean

    def catalog(self) -> Catalog:
        cat = Catalog()
        for kind in KINDS:
            for name in self.names(kind):
                item = self.load(kind, name)
                cat.items[(kind, name)] = item
                cat.problems += [Issue(path=f"{PLURAL[kind]}/{name}:{i.path}", message=i.message) for i in item.issues]
        for n in self.names_of("collections"):
            try:
                cat.collections[n] = self.get_collection(n)
            except (Unprocessable, NotFound) as exc:
                cat.problems.append(Issue(path=f"collections/{n}", message=exc.detail))
        for n in self.names_of("clients"):
            try:
                cat.clients[n] = self.get_client(n)
            except (Unprocessable, NotFound) as exc:
                cat.problems.append(Issue(path=f"clients/{n}", message=exc.detail))
        for n in self.names_of("profiles"):
            try:
                cat.profiles[n] = self.get_profile(n)
            except Unprocessable as exc:
                cat.problems.append(Issue(path=f"profiles/{n}", message=exc.detail))
        try:
            cat.endpoints = self.endpoints()
        except (ValueError, yamlsafe.YamlError, Unprocessable) as exc:
            cat.problems.append(Issue(path="endpoints.yaml", message=str(exc)))
        if self.exists("hub.yaml"):
            try:
                HubMeta.model_validate(yamlsafe.load(self._read("hub.yaml")) or {})
            except (ValidationError, yamlsafe.YamlError) as exc:
                cat.problems.append(Issue(path="hub.yaml", message=str(exc)[:200]))
        return cat


def kind_from_plural(plural: str) -> str:
    if plural not in FROM_PLURAL:
        raise BadRequest(f"unknown collection of items: {plural}")
    return FROM_PLURAL[plural]


__all__ = ["CONCERNS", "Catalog", "ContentStore", "Item", "kind_from_plural", "validate_endpoints"]
