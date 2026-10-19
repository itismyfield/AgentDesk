#[test]
fn n4d2_dormant_census_counts_actual_production_callers() {
    let script = r#"
import re, sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from scripts import check_durable_frontier_writer_call_sites as c
from scripts import check_legacy_supervision_census as b
from scripts.rust_lex import StripState, strip_line
root = Path(sys.argv[1])
all_files = c._SKIP_PIN._lexical_rust_files(root, Path('src'))
basename = {p for p in all_files if c.is_test_file(p.name)}
resolved = c._SKIP_PIN._INVENTORY.test_only_module_files(production_files=[p for p in all_files if p not in basename], all_files=all_files)
skips = basename | resolved
sources = {p.relative_to(root).as_posix(): c._production_text(p) for p in all_files}
errors = b.audit(sources, skips={p.relative_to(root).as_posix() for p in skips})
assert not errors, errors
removed = re.compile(r'\b(?:restore_retired_o_watchers|RetiredOWatcherRestore|install_mapped_retired_o_watcher|o_only_restore|o_only_retry|record_retired_o_install_outcome|retired_o_install_due|o_install_reason|install_retired_o_watcher|claim_retired_o_watcher_for_host|o_only_install|OOnlyInstall(?:Request|Outcome|Reason))\b')
for path in all_files:
    state = StripState()
    code = '\n'.join(strip_line(line, state) for line in path.read_text().splitlines())
    assert not removed.search(code), f'{path}: removed retired symbol reintroduced'
for suffix in ['o_only_install.rs', 'o_only_install_tests.rs']:
    assert not (root / 'src/services/discord/watchers/lifecycle' / suffix).exists(), suffix
print('canonical_retired_production_writers=0')
print('removed_retired_install_restore_retry_symbols=0')
print('canonical_retired_condition_unchanged=true')
print('activation_ready=false')
"#;
    let output = std::process::Command::new("python3")
        .args(["-c", script, env!("CARGO_MANIFEST_DIR")])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report = String::from_utf8(output.stdout).unwrap();
    assert!(report.contains("canonical_retired_production_writers=0"));
    assert!(report.contains("removed_retired_install_restore_retry_symbols=0"));
    assert!(report.contains("canonical_retired_condition_unchanged=true"));
    assert!(report.contains("activation_ready=false"));
}
