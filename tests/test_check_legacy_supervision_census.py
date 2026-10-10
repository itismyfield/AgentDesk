import hashlib
import re
import tempfile
import unittest
from collections import Counter
from pathlib import Path
from scripts import check_legacy_supervision_census as guard


class DormantCensusTests(unittest.TestCase):
    def inspect(self, source, name="src/consumer.rs"):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "fixture.rs"
            path.write_text(source)
            code = guard.classifier._production_text(path)
            return guard.audit({name: code}, entries={}, protected={})

    def test_external_install_and_function_value_fail_with_owner(self):
        errors = self.inspect("fn start() { let f = BootCohort::install_process; }")
        self.assertEqual(len(errors), 2)
        self.assertTrue(all("src/consumer.rs:1:start:" in error for error in errors))

    def test_comment_strings_and_raw_strings_are_not_callers(self):
        source = '\n'.join(['// BootCohort::install_process', '/* nested /* BootPublication */ */',
            'fn prose() { let text = "BootCohort::install_process";',
            'let raw = r###"{ #[cfg(test)] BootPublication"###; }'])
        self.assertEqual(self.inspect(source), [])

    def test_prose_cannot_hide_real_caller_afterward(self):
        source = 'fn start() { let raw = r###"{ #[cfg(test)]"###; BootCohort::install_process(); }'
        self.assertEqual(len(self.inspect(source)), 2)

    def test_test_name_and_test_attribute_do_not_hide_production(self):
        for source in ('fn tests() { BootCohort::install_process(); }',
                       '#[test] fn tests() { BootCohort::install_process(); }'):
            with self.subTest(source=source):
                self.assertEqual(len(self.inspect(source)), 2)

    def test_test_basename_cannot_hide_an_unregistered_production_caller(self):
        self.assertEqual(len(self.inspect('fn start() { BootCohort::install_process(); }', 'src/fake_tests.rs')), 2)

    def test_cfg_test_and_all_test_are_excluded(self):
        for attr in ('#[cfg(test)]', '#[cfg(all(test, unix))]'):
            with self.subTest(attr=attr):
                self.assertEqual(self.inspect(attr + ' fn fixture() { BootCohort::install_process(); }'), [])

    def test_cfg_any_and_not_test_are_production(self):
        for attr in ('#[cfg(any(test, feature = "live"))]', '#[cfg(not(test))]'):
            with self.subTest(attr=attr):
                self.assertEqual(len(self.inspect(attr + ' fn start() { BootCohort::install_process(); }')), 2)

    def test_alias_reexport_and_macro_fail_closed(self):
        for source in ('use crate::services::discord::boot_retirement as hidden;',
                       'pub use hidden::BootCohort as Other;',
                       'macro_rules! hidden { () => { BootCohort::install_process() } }'):
            with self.subTest(source=source):
                self.assertTrue(self.inspect(source))

    def test_registered_test_module_is_excluded(self):
        self.assertEqual(guard.audit({'src/registered.rs': 'BootCohort'}, entries={}, protected={}, skips={'src/registered.rs'}), [])

    def test_canonical_and_other_retired_symbols_are_distinct(self):
        self.assertEqual(self.inspect('fn unrelated() { RETIRED.insert(key); }'), [])
        errors = self.inspect('fn writer() { legacy_supervision::RETIRED.get_or_init(make); }')
        self.assertEqual(len(errors), 1)
        self.assertIn(':writer: external RETIRED', errors[0])

    def test_protected_body_and_helper_manifest_drift_fail(self):
        errors = guard.audit({'primitive.rs': 'fn extra() {}', 'canonical.rs': 'changed'},
            entries={'primitive.rs': ''}, protected={'canonical.rs': guard.digest('original')})
        self.assertEqual(len(errors), 2)
        self.assertIn('manifest drift', errors[0])
        self.assertIn('protected boot/RETIRED body', errors[1])

    def test_new_primitive_macro_or_hidden_file_fails(self):
        entries = {guard.R + '.rs': ''}
        errors = guard.audit({guard.R + '.rs': 'macro_rules! spawn {}'}, entries=entries, protected={})
        self.assertTrue(any('activation syntax' in error for error in errors))
        errors = guard.audit({guard.R + '/hidden.rs': 'fn new() {}'}, entries={}, protected={})
        self.assertTrue(any('unclassified primitive file' in error for error in errors))

    def test_ci_wires_both_gate_and_self_tests(self):
        code = Path('scripts/ci-script-checks.sh').read_text()
        self.assertIn('scripts/check_legacy_supervision_census.py', code)
        self.assertIn('tests.test_check_legacy_supervision_census', code)


N4D_INSTALLER = guard.D + 'watchers/lifecycle/o_only_install.rs'
N4D_ENTRY = guard.D + 'tmux_watcher/entry.rs'
N4D_DECODER = guard.D + 'tmux_watcher/utf8_chunk_decoder.rs'
N4D_HOST = guard.D + 'tmux_watcher/host_gate.rs'
N4D_API = frozenset(('install_retired_o_watcher', 'OOnlyInstallRequest',
                     'OOnlyInstallOutcome', 'OOnlyInstallReason'))
N4D_SYMBOL = re.compile(r'\b(?:' + '|'.join(sorted(N4D_API)) + r'|o_only_install)\b')
N4D_FACADES = {
    guard.D + 'watchers/lifecycle.rs': 'o_only_install',
    guard.D + 'tmux.rs': 'watcher_lifecycle',
    guard.D + 'mod.rs': 'tmux',
}
N4D_BASE_BODIES = {
    guard.D + 'watchers/lifecycle.rs': '85a5c0f5bca8c741a9f785480b0d8961e7a143f8a3f905de476f5c4b168c694c',
    guard.D + 'tmux.rs': '647291663530ddeceefd30d68335f5eed442655f0d84d9840dd7a7a339210fa8',
    guard.D + 'mod.rs': '209ef308e3c65424e4516e23ee88703cae3f995c56374d243f389a22d8a41bbd',
    guard.D + 'inflight.rs': '4c92518a09af9170b741592b2b7ee8037ff922572f099b5636f47e566bb24c36',
}
# Byte pins bind the forbidden operating contacts to main at 0dcd09a3f.
N4D_PROTECTED = {
    guard.D + 'runtime_bootstrap.rs': (1, '91ec3a1c9caa439dbcd368925b831f66cc0e9bf4de18f84b43cc4fa6b2294553'),
    guard.D + 'runtime_bootstrap': (46, '99d9edfe58d90ac490ad8aad23fc99216e031436dc278b86a3d0b18fa84e0c17'),
    guard.D + 'recovery_engine/restore_inflight*': (8, '0c2d2aa62a6696a5f805ecbeaf8fc0a00b8415cb28a8a81af8991eef1ac59d49'),
    guard.D + 'inflight/removal.rs': (1, '833143d1bf3d84ac8ba83c9fa493fe243916832be0de3e12f0ec603ac267d309'),
    guard.D + 'inflight/removal': (6, '4fbe936fe8a6ad8a4519b8d594a3d91cda746a736817873266a99813f5e45f99'),
    'src/services/tui_o/writer': (59, '771e5b5f98dd61893b1a94d32f1e9c2b4300655411844a50badc449db3078c35'),
    guard.D + 'health/watcher_respawn.rs': (1, 'd4cd7bb82bcb85c72a7bf2debf2428c71357f6e0be14c70ecf13ab75fcca86c7'),
    guard.D + 'health/watcher_respawn': (4, 'afbf84cbcd220d0f39e8a061ef17d3f6241850982fa5f695ad0ef5dc985bc99f'),
    guard.D + 'input_runtime': (33, '7f779518dae5702102bb9cf6f2713faa85f7423b7a3d71355370663d1d8d51e7'),
    guard.D + 'watchers/lifecycle/restore.rs': (1, 'bb59a225d162b717299d3c4b8376312d9f896542386b018d3f5b6b13b8fd3267'),
    guard.D + 'tmux_reaper.rs': (1, '78386b91814833bfcb443b5528499fd3a049ae73738cbdeeca81382a651b7bad'),
    guard.D + 'health/legacy_supervision.rs': (1, '099bf8a77e67ec9e11186d3da6cf5db177f20733c8fc187dac7b73a47d5d5a63'),
    guard.D + 'tmux_watcher/turn_identity.rs': (1, 'da1e56a952b7e177daf488ec1353e880713957b223192a3bc12b7434dc3d0d9d'),
    guard.D + 'tmux_watcher/terminal_preflight.rs': (1, '7c06a8c11b39dbda0729606a7aafc4df36800e4210afe6399a2bac8beddf49b3'),
    guard.D + 'tmux_watcher/pre_emit_guard.rs': (1, '4a4d7826022a2c409153103402864a042e102602224c9c2492004b60dba8641f'),
    guard.D + 'tmux_watcher/terminal_relay_plan.rs': (1, 'ad3300749d314f8546bf4a74c190facf972ab5c97a0501ecdd3f9af98643536b'),
    guard.D + 'tmux_watcher/terminal_commit_epilogue.rs': (1, '1e206316eb0b15e08cf5c52e927c188385f6d172c9a2ec3ad093c442ccf2d751'),
    guard.D + 'tmux_watcher/o_delegated_arm.rs': (1, '8f34db7d10828dd10fdc797fd30f1bae992261deae0a65a56aaac1e0a68e0759'),
    guard.D + 'tmux_watcher/post_stream_exit.rs': (1, '38b7e4b9599fa8f9a6f148e80981601f6e6d08d9866941698db29cfd2f19817d'),
    guard.D + 'tmux_watcher/cancel_handoff/completion.rs': (1, '2681333bfcd1d9699973f9271f491f580d49c3215b9a0e88a5b87b00c79ae17b'),
}
N4D_LEGACY_FUNCTIONS = {
    (N4D_ENTRY, 'restore_delivery_position'): 'c3382643eb416d782e8f13439750bf974d111336f2676d29a32d8848d6bd25b1',
    (N4D_DECODER, 'restore_stream_decoder'): '96e82890d6a83d3cf5cce5a36212b812760320b560b5ed283a1f9009fc0377b8',
    (N4D_HOST, 'channel_row'): 'c851c691ca85e79cae22897fee9ab277f709f971c6d16345581aa9c3643a74f9',
    (N4D_HOST, 'tmux_alive'): '19861e8c3c05e253143df4f6f38c652ce165fdd0bc5277d35da88320edc3f257',
    (N4D_HOST, 'row_probe'): 'e8a8f6474c77874ff167596fd3d422505cd383a6713c4d3257335cc484fef1d9',
    (N4D_HOST, 'marker_alive'): '63de0e12a5215bf0d1cbbd1de7cff5ae4da916f87475a65daab78ccce3a8ef6a',
    (N4D_HOST, 'host_alive'): '895787bb4abadaf78c8e5b2e16302a52e174c07a9999aa9094e548f4e67b2d34',
    (N4D_HOST, 'watcher_provider'): '61c84dd0972ee17eb7f9b873beb9141603d0cd4dfb4c98af7bef6c1c0ce9fd5a',
    (N4D_HOST, 'admits_teardown'): 'f3a32fa1766ae183aeac1629485bba55be88223b87dee2c5e94cef3d5588920b',
    (N4D_HOST, 'admits_automatic_kill'): 'a1a875880904a80ca8d37705d2b8fa40a35c4e1721c84cf00fdef15037c87599',
    (N4D_HOST, 'tmux_pane_dead'): '564cc18c034dc14a5819751bd3a63b41833ad5a3a5c9f2c3358c39caf71ab709',
    (N4D_HOST, 'tmux_dead_pane_present'): 'c81dcb9b330730afbd5a20855dc0e0bac66baad2c3dd56e3e0bc8e5a88c15d57',
    (N4D_HOST, 'background_agent_pending'): '182b6180143852577cc52f76f426ef3c3f06157f37b72dd394843452ffaf90ff',
}
N4D_ACTIVATION = re.compile(
    r'\b(?:static|LazyLock|OnceLock|macro_rules|include|ctor|inventory|linkme|'
    r'env|option_env|WATCHER_ABSENCE|register_\w*|rehydrate_\w*)\b')
N4D_CONTACT_ACTIVATION = re.compile(
    r'\b(?:LazyLock|OnceLock|macro_rules|include|ctor|inventory|linkme|env|option_env|'
    r'WATCHER_ABSENCE|register_\w*|rehydrate_\w*)\b|\bstatic\s+(?:mut\s+)?\w+\s*:(?!:)')
N4D_CONTACTS = frozenset(N4D_FACADES) | frozenset((
    N4D_ENTRY, N4D_DECODER, N4D_HOST, guard.D + 'tmux_watcher.rs',
    guard.D + 'tmux_watcher/cancel_handoff.rs',
    guard.D + 'tmux_watcher/loop_poll_prologue.rs',
    guard.D + 'tmux_watcher/turn_stream_collector.rs',
    guard.D + 'tmux_watcher/turn_stream_collector/state.rs',
    guard.D + 'tmux_watcher/streaming_status_tick.rs',
    guard.D + 'tmux_watcher/streaming_status_tick/types.rs',
    guard.D + 'tmux_watcher/no_result_exits.rs', guard.D + 'inflight.rs',
    guard.D + 'session_relay_sink.rs', 'src/services/tui_prompt_dedupe.rs',
    'src/services/tui_prompt_dedupe/shadow_peek.rs',
))
N4D_BASE_ACTIVATION = {
    guard.D + 'tmux.rs': {'LazyLock': 1, 'staticMONITOR_AUTO_TURN_LEDGER_GENERATION:': 1, 'register_start': 1},
    guard.D + 'mod.rs': {'env': 1, 'staticCACHED:': 1, 'OnceLock': 2},
    guard.D + 'tmux_watcher/loop_poll_prologue.rs': {'macro_rules': 1},
    guard.D + 'tmux_watcher/turn_stream_collector.rs': {'macro_rules': 3},
    guard.D + 'tmux_watcher/streaming_status_tick.rs': {'macro_rules': 1},
    'src/services/tui_prompt_dedupe.rs': {
        'LazyLock': 5, 'staticSTATE:': 1, 'staticOBSERVED_PROMPTS:': 1,
        'staticEXTERNAL_INPUT_RELAY_LEASE_GENERATION:': 1,
        'staticSSH_DIRECT_OBSERVATION_GENERATION:': 1, 'register_injected_steer': 1},
    guard.D + 'session_relay_sink.rs': {'staticSESSION_BOUND_DISCORD_DELIVERY_ENABLED:': 1, 'macro_rules': 1},
}


def n4d_facade_without_exports(path, code):
    expected = N4D_FACADES.get(path)
    if expected is None:
        return code, 0
    pattern = re.compile(
        r'(?:#\[[^\[\]]*\]\s*)*pub(?:\([^;{}]*\))?\s+use\s+'
        + r'(?:self::)?' + re.escape(expected) + r'\s*::\s*\{([^;{}]+)\}\s*;')
    exports = 0

    def remove_export(match):
        nonlocal exports
        names = [name.strip() for name in match.group(1).split(',') if name.strip()]
        if len(names) == len(N4D_API) and set(names) == N4D_API:
            exports += 1
            return '\n' * match.group().count('\n')
        return match.group()

    code = pattern.sub(remove_export, code)
    if path == guard.D + 'watchers/lifecycle.rs':
        code = re.sub(r'(?:#\[[^\[\]]*\]\s*)*mod\s+o_only_install\s*;', '', code)
    return code, exports


def n4d_audit(sources, require_surface=False, installer_test_only=False):
    errors = []
    references = []
    for path, code in sources.items():
        if path == N4D_INSTALLER:
            if installer_test_only:
                errors.append(f'{path}: test-only installer remains production-classified')
            for match in N4D_ACTIVATION.finditer(code):
                errors.append(f'{path}: unclassified activation syntax: {match.group()}')
            if require_surface:
                for symbol in N4D_API:
                    declaration = r'\b(?:fn|struct|enum)\s+' + re.escape(symbol) + r'\b'
                    if len(re.findall(declaration, code)) != 1:
                        errors.append(f'{path}: missing or duplicate primitive declaration: {symbol}')
            continue
        remaining, exports = n4d_facade_without_exports(path, code)
        expected_exports = 0 if installer_test_only else 1
        if require_surface and path in N4D_FACADES and exports != expected_exports:
            errors.append(f'{path}: narrow facade export count={exports}, expected={expected_exports}')
        for match in N4D_SYMBOL.finditer(remaining):
            owners = guard.FN.findall(remaining[:match.start()])
            line = remaining[:match.start()].count('\n') + 1
            references.append((path, match.group()))
            errors.append(f'{path}:{line}:{owners[-1] if owners else "<item>"}: '
                          f'external O install reference: {match.group()}')
    if require_surface and not installer_test_only and N4D_INSTALLER not in sources:
        errors.append('missing production O installer')
    census = {
        'n4d_install_application_references': len(references),
        'n4d_boot_hook_connections': sum('/runtime_bootstrap' in path for path, _ in references),
        'n4d_retry_wrapper_connections': sum('/health/watcher_respawn' in path for path, _ in references),
        'n4d_force_clean_connections': sum('/health/watcher_respawn' in path for path, _ in references),
        'n4d_new_absence_stores': sum(len(re.findall(r'\bWATCHER_ABSENCE\b', code))
                                     for path, code in sources.items() if path == N4D_INSTALLER),
    }
    return errors, census


def n4d_protected_digest(sources, prefix):
    matches = sorted(path for path in sources if (
        path.startswith(prefix[:-1]) if prefix.endswith('*') else
        path == prefix or path.startswith(prefix + '/')))
    digest = hashlib.sha256()
    for path in matches:
        digest.update(path.encode() + b'\0' + hashlib.sha256(sources[path]).digest())
    return len(matches), digest.hexdigest()


def n4d_function(code, name):
    declarations = list(re.finditer(r'\bfn\s+' + re.escape(name) + r'\s*\(', code))
    if len(declarations) != 1:
        raise AssertionError(f'expected exactly one function: {name}')
    match = declarations[0]
    body_start = code.index('{', match.end())
    depth, end = 1, body_start + 1
    while depth and end < len(code):
        depth += (code[end] == '{') - (code[end] == '}')
        end += 1
    if depth:
        raise AssertionError(f'unclosed function: {name}')
    return code[match.start():end]


def n4d_legacy_seed_function(code, name):
    code = re.sub(r'\s+', '', n4d_function(code, name))
    code = code.replace('legacy_mode:WatcherLegacyMode,', '')
    guards = {
        'restore_delivery_position': 'if!legacy_mode.is_legacy(){return(None,None,None,None);}',
        'restore_stream_decoder': 'if!ctx.legacy_mode.is_legacy(){returnOk((tool_state,None));}',
    }
    if code.count(guards[name]) != 1:
        raise AssertionError(f'missing or duplicate retirement seed guard: {name}')
    return code.replace(guards[name], '', 1)


class N4dDormantCensusTests(unittest.TestCase):
    def production(self, source):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / 'fixture.rs'
            path.write_text(source)
            return guard.classifier._production_text(path)

    def test_operational_call_function_value_and_alias_mutants_are_red(self):
        mutants = (
            'fn start() { install_retired_o_watcher(request); }',
            'fn start() { let f = install_retired_o_watcher; consume(f); }',
            'use crate::services::discord::install_retired_o_watcher as hidden; fn start() { hidden(request); }',
            'use crate::services::discord::tmux::watcher_lifecycle::o_only_install as hidden;',
            'macro_rules! launch { () => { install_retired_o_watcher(request) } }',
        )
        for mutant in mutants:
            with self.subTest(mutant=mutant):
                errors, census = n4d_audit({'src/consumer.rs': self.production(mutant)})
                self.assertTrue(errors)
                self.assertGreater(census['n4d_install_application_references'], 0)

    def test_prose_and_test_cfg_cannot_change_operational_classification(self):
        hidden = '// install_retired_o_watcher\n#[cfg(test)] fn test() { install_retired_o_watcher(request); }'
        self.assertEqual(n4d_audit({'src/consumer.rs': self.production(hidden)})[0], [])
        for cfg in ('#[cfg(any(test, feature = "live"))]', '#[cfg(not(test))]'):
            source = cfg + ' fn start() { let f = install_retired_o_watcher; }'
            self.assertTrue(n4d_audit({'src/consumer.rs': self.production(source)})[0])

    def test_only_explicit_facade_exports_and_primitive_references_are_allowed(self):
        names = ', '.join(sorted(N4D_API))
        for path, module in N4D_FACADES.items():
            for prefix in ('', 'self::'):
                with self.subTest(path=path, prefix=prefix):
                    source = f'pub use {prefix}{module}::{{{names}}};'
                    self.assertEqual(n4d_audit({path: self.production(source)})[0], [])
                    alias = source.replace('install_retired_o_watcher', 'install_retired_o_watcher as hidden')
                    self.assertTrue(n4d_audit({path: self.production(alias)})[0])
                    source += ' fn start() { let f = install_retired_o_watcher; }'
                    self.assertTrue(n4d_audit({path: self.production(source)})[0])
        primitive = 'pub async fn install_retired_o_watcher(request: OOnlyInstallRequest) -> OOnlyInstallOutcome {}'
        self.assertEqual(n4d_audit({N4D_INSTALLER: self.production(primitive)})[0], [])

    def test_test_only_install_surface_still_rejects_production_exports(self):
        names = ', '.join(sorted(N4D_API))
        sources = {path: self.production(f'#[cfg(test)] pub use {module}::{{{names}}};')
                   for path, module in N4D_FACADES.items()}
        self.assertEqual(n4d_audit(sources, require_surface=True,
                                  installer_test_only=True)[0], [])
        self.assertTrue(n4d_audit(sources, require_surface=True)[0])
        for path, module in N4D_FACADES.items():
            for cfg in ('', '#[cfg(any(test, feature = "live"))]', '#[cfg(not(test))]'):
                with self.subTest(path=path, cfg=cfg):
                    mutated = dict(sources)
                    mutated[path] = self.production(cfg + f' pub use {module}::{{{names}}};')
                    self.assertTrue(n4d_audit(mutated, require_surface=True,
                                             installer_test_only=True)[0])
        mutated = {**sources, N4D_INSTALLER: 'pub fn install_retired_o_watcher() {}'}
        self.assertTrue(n4d_audit(mutated, require_surface=True,
                                 installer_test_only=True)[0])

    def test_b1_primitive_manifest_remains_present_for_whole_file_skips(self):
        sources = {'primitive.rs': 'fn begin() {}', 'consumer.rs': 'fn start() {}'}
        entries = {'primitive.rs': 'begin'}
        self.assertEqual(guard.audit(sources, entries=entries, protected={}, skips={'primitive.rs'}), [])
        filtered = {path: code for path, code in sources.items() if path != 'primitive.rs'}
        self.assertTrue(guard.audit(filtered, entries=entries, protected={}))

    def test_automatic_activation_and_absence_mutants_are_red(self):
        for source in ('static INIT: LazyLock<()> = LazyLock::new(start);',
                       'fn new() { std::env::var("ADK_O_INSTALL"); }',
                       'fn new() { register_runtime(); }',
                       'fn new() { WATCHER_ABSENCE.insert(key); }',
                       '#[ctor] fn start() {}'):
            with self.subTest(source=source):
                self.assertTrue(n4d_audit({N4D_INSTALLER: self.production(source)})[0])

    def test_canonical_production_writer_cannot_hide_behind_test_name_or_cfg(self):
        canonical = guard.D + 'health/legacy_supervision.rs'
        production = 'fn is_retired() { RETIRED.get(); }'
        protected = {canonical: guard.digest(self.production(production))}
        fixture = production + '\n#[cfg(test)] fn mark() { RETIRED.get_or_init(make).write().insert(key); }'
        self.assertEqual(guard.audit({canonical: self.production(fixture)}, entries={}, protected=protected), [])
        for cfg in ('', '#[test]', '#[cfg(any(test, feature = "live"))]', '#[cfg(not(test))]'):
            fixture = production + '\n' + cfg + ' fn tests() { RETIRED.get_or_init(make).write().insert(key); }'
            errors = guard.audit({canonical: self.production(fixture)}, entries={}, protected=protected)
            self.assertTrue(any('protected boot/RETIRED body' in error for error in errors))

    def test_protected_digest_detects_modification_addition_and_deletion(self):
        original = {'src/boot/a.rs': b'fn start() {}'}
        expected = n4d_protected_digest(original, 'src/boot')
        for mutant in ({'src/boot/a.rs': b'fn start() { work(); }'},
                       {**original, 'src/boot/hidden.rs': b'fn init() {}'}, {}):
            self.assertNotEqual(n4d_protected_digest(mutant, 'src/boot'), expected)

    def test_legacy_seed_body_mutation_is_not_hidden_by_retired_guard(self):
        legacy = 'fn restore_delivery_position() { load_inflight_state(); }'
        guarded = 'fn restore_delivery_position() { if !legacy_mode.is_legacy() { return (None, None, None, None); } load_inflight_state(); }'
        self.assertEqual(guard.digest(n4d_legacy_seed_function(guarded, 'restore_delivery_position')),
                         guard.digest(legacy))
        mutant = guarded.replace('load_inflight_state();', 'return (None, None, None, None);')
        self.assertNotEqual(guard.digest(n4d_legacy_seed_function(mutant, 'restore_delivery_position')),
                            guard.digest(legacy))

    def test_n4d1a_dormant_census(self):
        root = Path(__file__).resolve().parents[1]
        files, skips = guard.classifier._scan_inputs(root, guard.classifier.PINNED_TEST_ONLY_MODULE_FILES)
        all_sources = {path.relative_to(root).as_posix(): guard.classifier._production_text(path)
                       for path in files}
        relative_skips = {path.relative_to(root).as_posix() for path in skips}
        self.assertEqual(guard.audit(all_sources, skips=relative_skips), [])
        sources = {path: code for path, code in all_sources.items() if path not in relative_skips}
        installer_test_only = N4D_INSTALLER in relative_skips
        errors, census = n4d_audit(sources, require_surface=True,
                                   installer_test_only=installer_test_only)
        self.assertEqual(errors, [])
        self.assertTrue(census)
        self.assertEqual(census, {name: 0 for name in census})
        for path in N4D_CONTACTS:
            measured = Counter(re.sub(r'\s+', '', value)
                               for value in N4D_CONTACT_ACTIVATION.findall(sources[path]))
            self.assertEqual(measured, Counter(N4D_BASE_ACTIVATION.get(path, {})), path)
        for path in (guard.D + 'runtime_bootstrap.rs',
                     guard.D + 'health/watcher_respawn/live_bridge_guard.rs'):
            for expression in ('install_retired_o_watcher(request)', 'install_retired_o_watcher'):
                mutated = dict(sources)
                mutated[path] += '\nfn forbidden_connection() { let f = ' + expression + '; }'
                errors, measured = n4d_audit(mutated, require_surface=True,
                                            installer_test_only=installer_test_only)
                self.assertTrue(errors)
                self.assertGreater(measured['n4d_install_application_references'], 0)
        for path in N4D_FACADES:
            code, _ = n4d_facade_without_exports(path, sources[path])
            self.assertEqual(guard.digest(code), N4D_BASE_BODIES[path], path)
        self.assertEqual(guard.digest(sources[guard.D + 'inflight.rs']),
                         N4D_BASE_BODIES[guard.D + 'inflight.rs'])
        selector = re.sub(r'\s+', '', n4d_function(sources[N4D_ENTRY], 'for_channel'))
        selector_body = selector[selector.index('{'):]
        self.assertRegex(selector_body,
            r'^\{ifcrate::services::discord::health::legacy_supervision::is_retired'
            r'\(provider\.as_str\(\),channel(?:_id)?\.get\(\)\)'
            r'\{Self::RetiredO\}else\{Self::Legacy\}\}$')
        for (path, name), expected in N4D_LEGACY_FUNCTIONS.items():
            function = (n4d_legacy_seed_function(sources[path], name)
                        if path in (N4D_ENTRY, N4D_DECODER) else n4d_function(sources[path], name))
            self.assertEqual(guard.digest(function), expected, f'{path}:{name}')
        raw = {path.relative_to(root).as_posix(): path.read_bytes() for path in files}
        for prefix, expected in N4D_PROTECTED.items():
            self.assertEqual(n4d_protected_digest(raw, prefix), expected, prefix)
