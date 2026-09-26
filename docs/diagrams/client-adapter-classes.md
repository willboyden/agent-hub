# Client adapter classes

Mermaid. Names are from `backend/src/agent_hub/adapters/`; a variant module is a tiny
subclass exposing its own id.

```mermaid
classDiagram
  class ClientAdapter {
    <<abstract>>
    +str id
    +str display_name
    +caps() AdapterCaps
    +default_config() ClientConfig
    +render(ctx RenderContext) RenderResult
    +discover(cfg, fs FileSystem) DiscoveredContent
    +verify(cfg, expected, fs, run CommandRunner) list~Check~
  }
  class ClaudeCodeAdapter
  class OpenCodeAdapter
  class HermesAdapter
  class HermesComfyAdapter
  class GenericSpecAdapter {
    +validate_spec(spec) list~Diag~
    +dry_render(spec, sample) RenderResult
  }
  ClientAdapter <|-- ClaudeCodeAdapter
  ClientAdapter <|-- OpenCodeAdapter
  ClientAdapter <|-- HermesAdapter
  HermesAdapter <|-- HermesComfyAdapter
  ClientAdapter <|-- GenericSpecAdapter

  class FileSystem {
    <<port>>
    +read_bytes(path)
    +exists(path)
    +listdir(path)
    +is_dir(path)
  }
  class CommandRunner {
    <<port>>
    +run(argv, timeout) rc_out_err
  }
  class RenderContext {
    +ClientConfig client
    +skills agents instructions
    +mcp_servers memory
    +PermissionSet permissions
    +endpoints
  }
  class Artifact {
    +root path content mode
    +kind source_ids
    +merge managed_keys
  }
  class RenderResult {
    +list~Artifact~ artifacts
    +list~Diag~ diagnostics
  }
  class AdapterRegistry {
    +ids() list
    +get(id) ClientAdapter
    +load_errors
  }

  ClientAdapter ..> RenderContext : render input
  ClientAdapter ..> RenderResult : render output
  RenderResult "1" o-- "*" Artifact
  ClientAdapter ..> FileSystem : discover and verify
  ClientAdapter ..> CommandRunner : verify
  AdapterRegistry o-- ClientAdapter : one ADAPTER per module
```

Rules that hold for every adapter: `render` is pure; artifacts use relative safe paths under a named root; output is
deterministic; an agent with no tools is an error; unsupported or lossy mappings are diagnostics (`error` when the client
is `strict`); `verify` must not report `ok` from a copy alone.
