#!/usr/bin/env node
// Generate backend/TEST-MAP.md: one row per pytest node id (backend/parity/pytest-ids.txt, from `pytest --collect-only -q`
// in backend/ and knowledge/; parametrised ids that embed this machine's paths are stored with the checkout path as
// `<REPO>` and the home directory as `<HOME>`, so the list is the same in every worktree), its OWNER (the workstream that owns the code under test, PORTING.md module map) and the
// Rust test that covers it. Owners edit their rows by updating MAPPED below (or, after phase 1, TEST-MAP.md directly
// in their own rows); `node backend/parity/check-test-map.mjs` verifies the result.
//
//   node backend/parity/gen/gen_test_map.mjs > backend/TEST-MAP.md
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const PARITY = dirname(dirname(fileURLToPath(import.meta.url)));
const ids = readFileSync(join(PARITY, 'pytest-ids.txt'), 'utf8').split('\n').filter(Boolean);

const NA = {
  'backend/tests/adapters/test_turnstone_deliver.py': 'tests deploy/turnstone-deliver.py, a standalone stdlib-Python operator tool outside the backend that stays Python (PORTING.md "Out-of-repo consumers")',
  'knowledge/tests/test_broker_addon.py': 'tests deploy/knowledge/broker/addon_knowledge_broker.py, a mitmproxy addon that runs inside mitmproxy and stays Python',
};

const FILE_OWNER = [
  [/^backend\/tests\/adapters\//, 'WS-A'],
  [/test_(skilleval|skilleval_partial|observed_eval)\.py$/, 'WS-C'],
  [/test_(floor|floor_hardening|content_guard|targets|doctor|doctor_coverage)\.py$/, 'WS-F'],
  [/test_(apply|rollback|toctou|git|fs_merge|discovery_fs|live)\.py$/, 'WS-B2'],
  [/test_(content|organize|mcp_gate|audit_fixes)\.py$/, 'WS-B1'],
  [/test_audit_round2\.py$/, 'WS-F'],
  [/test_yamlsafe\.py$/, 'WS-Y'],
  [/test_(pathsafe|desktop|api|ops_hardening|cli_registry)\.py$/, 'CORE'],
  [/^knowledge\//, 'WS-D'],
];

// (file regex, test-name regex, owner): exceptions to the file default, first match wins.
const OVERRIDES = [
  [/test_api\.py$/, /^test_(knowledge_|upstream_401|admin_key_is_read_lazily|unsafe_key_files)/, 'WS-C'],
  [/test_api\.py$/, /^test_(end_to_end_over_http|content_requires_initialised_repo|preview_plan_cannot_be_applied|client_lock_info_and_verify_record|real_subprocess_serves_on_a_custom_port)/, 'WS-B2'],
  [/test_api\.py$/, /^test_(rendered_for_client|effective_rules_carry_source|import_over_http|suggestions_and_import_and_status_over_http|spec_yaml_accepted_and_bounded|validate_spec_without_generic_adapter_is_503|adapters_floor_settings_and_policy_check|hostile_ids_in_urls|viewer_can_read_but_not_write|client_token_scopes|health_is_open_everything_else_needs_a_key|trust_loopback_defaults_off_and_health_warns_when_on)/, 'WS-B1'],
  [/test_audit_round2\.py$/, /^test_(attestation_key|forged_or_legacy_digests)/, 'WS-B1'],
  [/test_audit_round2\.py$/, /^test_attest_mcp_cli/, 'WS-C'],
  [/test_audit_fixes\.py$/, /^test_streaming_runner/, 'WS-B2'],
  [/test_audit_fixes\.py$/, /^test_hermes_style_observe/, 'WS-A'],
  [/test_audit_fixes\.py$/, /^test_(cancel_kills|incomplete_and_empty_observation|unprintable_unencodable|forget_and_prune|select_is_capped|observed_gate_default)/, 'WS-C'],
  [/test_mcp_gate\.py$/, /^test_(sandbox_and_egress|mcp_allow_rules_wildcards)/, 'WS-F'],
  [/test_mcp_gate\.py$/, /^test_merged_files_verify/, 'WS-B2'],
  [/test_ops_hardening\.py$/, /^test_(discard|commit_stages|deletions_under|pathspecs|snapshot_reads)/, 'WS-B2'],
  [/test_cli_registry\.py$/, /^test_(cli_|parser_requires|hub_api_key_is_read)/, 'WS-C'],
];

// pytest "file::test" (params stripped) -> the Rust test that covers every parametrisation, ported in phase 1.
const SEC = 'backend/crates/agent-hub/tests/api_security.rs::';
const MAPPED = {
  ...Object.fromEntries(['test_bad_relpaths_rejected', 'test_good_relpaths', 'test_resolve_under_inside', 'test_relative_root_rejected',
    'test_symlink_component_escape_rejected', 'test_symlink_leaf_rejected_even_inside_root', 'test_dangling_symlink_rejected',
    'test_root_itself_may_be_a_symlink', 'test_expand_root', 'test_ids_use_fullmatch', 'test_id_trailing_newline_is_not_accepted']
    .map((n) => [`backend/tests/core/test_pathsafe.py::${n}`, `backend/crates/hub-core/tests/pathsafe.rs::${n}`])),
  ...Object.fromEntries(['test_host_allowlist_421', 'test_allowed_hosts_setting', 'test_cross_origin_and_null_origin_403',
    'test_csrf_header_required_without_bearer', 'test_loopback_trust_is_off_in_tests_and_never_via_proxy_headers',
    'test_security_headers_on_every_response_including_errors_and_static', 'test_no_cors', 'test_body_caps',
    'test_errors_are_problem_json_and_never_echo_input', 'test_every_router_carries_the_role_dependency',
    'test_route_enumeration_every_mutating_route_requires_admin', 'test_unauthenticated_and_bad_bearer_cannot_mutate_anything',
    'test_keys_are_hashed_and_secret_shown_once', 'test_bootstrap_admin_key_is_0600_and_works', 'test_audit_is_append_only_and_redacted',
    'test_metrics_labels_are_bounded', 'test_static_allowlist_and_spa_fallback', 'test_static_files_are_revalidated_on_every_load',
    'test_static_symlink_escape', 'test_no_key_is_401_on_every_route_but_health', 'test_client_token_cannot_reach_anything_but_inbox',
    'test_viewer_cannot_read_keys_audit_settings', 'test_unknown_token_scope_is_422_with_a_clear_message',
    'test_host_allowlist_follows_the_effective_port']
    .map((n) => [`backend/tests/core/test_api.py::${n}`, `${SEC}${n}`])),
  'backend/tests/core/test_ops_hardening.py::test_check_bind': 'backend/crates/agent-hub/src/config.rs::test_check_bind',
  'backend/tests/core/test_desktop.py::test_hub_url_maps_wildcard_binds_to_loopback': 'backend/crates/agent-hub/src/desktop.rs::test_hub_url_is_loopback_only',
  'backend/tests/core/test_desktop.py::test_hub_url_refuses_anything_off_the_box': 'backend/crates/agent-hub/src/desktop.rs::test_hub_url_is_loopback_only',
  'backend/tests/core/test_desktop.py::test_hub_url_refuses_an_out_of_range_port': 'backend/crates/agent-hub/src/desktop.rs::test_hub_url_is_loopback_only',
  'backend/tests/core/test_desktop.py::test_listen_uids_reads_every_listener_on_the_port': 'backend/crates/agent-hub/src/desktop.rs::test_listen_uids_parses_proc_net_tables',
};

function owner(id) {
  const [file, rest] = id.split('::');
  if (file.startsWith('knowledge/')) return 'WS-D';
  const name = rest.replace(/\[.*$/, '');
  for (const [f, n, o] of OVERRIDES) if (f.test(file) && n.test(name)) return o;
  for (const [f, o] of FILE_OWNER) if (f.test(file)) return o;
  return 'CORE';
}

const rows = [];
const counts = {};
for (const id of ids) {
  const file = id.split('::')[0];
  const base = id.replace(/\[.*$/, '');
  let o; let rust; let status;
  if (NA[file]) { o = '-'; rust = `N/A: ${NA[file]}`; status = 'N/A'; }
  else {
    o = owner(id);
    rust = MAPPED[base] ?? '';
    status = rust ? 'mapped' : 'pending';
  }
  counts[o] ??= { mapped: 0, pending: 0, 'N/A': 0 };
  counts[o][status] += 1;
  const col = status === 'N/A' ? rust : rust ? `\`${rust}\`` : '';
  rows.push(`| \`${id.replace(/\|/g, '\\|')}\` | ${o} | ${col} | ${status} |`);
}

const out = [];
out.push('# TEST-MAP: every pytest node id and its Rust counterpart');
out.push('');
out.push('Generated by `node backend/parity/gen/gen_test_map.mjs > backend/TEST-MAP.md` from `backend/parity/pytest-ids.txt` (1085 ids:');
out.push('993 backend + 92 knowledge, collected 2026-10-27 at c419f1e; machine paths inside parametrised ids are written as');
out.push('`<REPO>` (the checkout) and `<HOME>`). Checked by `node backend/parity/check-test-map.mjs`');
out.push('(every id exactly once; every `mapped` Rust test exists; `--owner WS-X` limits the unmapped count to one owner).');
out.push('');
out.push('Columns: **owner** = the workstream that owns the code under test (backend/PORTING.md module map); **rust test** =');
out.push('`<file>::<fn>` (names mirror the Python test; one Rust test may cover every parametrisation) or `N/A: <reason>`;');
out.push('**status** = mapped | pending | N/A. A module is not done while one of its rows is `pending`.');
out.push('');
out.push('| owner | mapped | pending | N/A |');
out.push('|---|---|---|---|');
for (const [o, c] of Object.entries(counts).sort()) out.push(`| ${o} | ${c.mapped} | ${c.pending} | ${c['N/A']} |`);
out.push('');
out.push('| pytest id | owner | rust test | status |');
out.push('|---|---|---|---|');
out.push(...rows);
console.log(out.join('\n'));
