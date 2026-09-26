"""Adapter registry. Every module in this package (except `base` and `_private` helpers) may expose `ADAPTER`: a
ClientAdapter subclass or instance. A broken or missing module is logged and skipped, never fatal, so one bad adapter
cannot take the hub down. Loading is lazy (a function, not import-time) to avoid import cycles with `base`."""
from __future__ import annotations

import importlib
import inspect
import logging
import pkgutil

from agent_hub.adapters.base import ClientAdapter

log = logging.getLogger("agent_hub.adapters")


class AdapterRegistry:
    def __init__(self) -> None:
        self._adapters: dict[str, ClientAdapter] = {}
        self.load_errors: dict[str, str] = {}

    def register(self, adapter: ClientAdapter) -> None:
        aid = getattr(adapter, "id", None)
        if not isinstance(aid, str) or not aid:
            raise ValueError("adapter has no id")
        self._adapters[aid] = adapter

    def get(self, adapter_id: str) -> ClientAdapter | None:
        return self._adapters.get(adapter_id)

    def ids(self) -> list[str]:
        return sorted(self._adapters)

    def all(self) -> list[ClientAdapter]:
        return [self._adapters[i] for i in self.ids()]

    def discover_package(self) -> AdapterRegistry:
        for mod in pkgutil.iter_modules(__path__):
            if mod.name == "base" or mod.name.startswith("_"):
                continue
            try:
                m = importlib.import_module(f"{__name__}.{mod.name}")
                obj = getattr(m, "ADAPTER", None)
                if obj is None:
                    self.load_errors[mod.name] = "module exposes no ADAPTER"
                    log.warning("adapter module %s exposes no ADAPTER; skipped", mod.name)
                    continue
                inst = obj() if inspect.isclass(obj) else obj
                if not isinstance(inst, ClientAdapter):
                    raise TypeError("ADAPTER is not a ClientAdapter")
                self.register(inst)
            except Exception as exc:  # noqa: BLE001 - tolerate any adapter failure, by design
                self.load_errors[mod.name] = f"{type(exc).__name__}: {str(exc)[:200]}"
                log.warning("adapter module %s failed to load: %s", mod.name, exc)
        return self


def load_registry() -> AdapterRegistry:
    return AdapterRegistry().discover_package()
