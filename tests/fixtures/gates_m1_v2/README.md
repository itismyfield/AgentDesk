# Frozen production v2 oracle

The oracle is AgentDesk commit `5806d1e6038040218ec48b51bf3105fe36992a23`, with `IDENTITY_VERSION=2`. The bundled recipe runs its production `SourceCapture`, spool append, `UnitDeriver`, and Prepared/Posted ledger APIs. It records the first piece of a multi-piece body as Posted(101) and leaves later pieces unprepared. No Discord request is made.

Both provider bundles preserve the resulting source, era, init, spool, cursor, ledger and derived tuples. Codex includes body/tool records before and after `turn_aborted`, followed by a native turn and body. Both providers exercise Unicode/code-fence pieces and tool exclusions.

`generator-recipe.tar.gz.b64` is the UTF-8 base64 representation of the portable recipe archive. The decoded archive SHA-256 is `a088d522e05b85d48edeea14f5d37ba2c717ff67f1b3527f0fef75aa04ad410b`. Text encoding satisfies the repository size gate, which rejects binary artifacts before considering fixture exclusions; it does not change the archive or any recipe input bytes.

Set `M1_RECIPE_DIR` to an existing disposable directory chosen for this review. From the repository root, decode with this one-line Python command:

```sh
python3 -c 'import base64, pathlib, sys; pathlib.Path(sys.argv[2]).write_bytes(base64.b64decode(pathlib.Path(sys.argv[1]).read_bytes()))' tests/fixtures/gates_m1_v2/generator-recipe.tar.gz.b64 "$M1_RECIPE_DIR/generator-recipe.tar.gz"
```

Export `M1_ORACLE_REPO="$PWD"`, extract the decoded archive with `tar -xzf "$M1_RECIPE_DIR/generator-recipe.tar.gz" -C "$M1_RECIPE_DIR"`, then change to that directory. The archived README's initial archive-extraction step is now complete; continue its `extract.py`, `verify_provenance.py`, and locked cargo-run commands with the approved cargo shim. The archive contains the portable input program, module wrappers, pinned-code extractor, independent provenance verifier and dependency lock. Extraction obtains the old production files with `git show`; those files are not replaced by current v3 code. `provenance.json` records original git blob IDs and SHA-256 values, exact compiled extracts and recipe digests.

Generation may vary original path/device/inode and ledger observation times. The tests rebind SourceId path/device/inode and corresponding filenames, and reserialize init/cursor/header metadata for isolated files. Raw source/frame bytes, UnitKeys, piece indices, payloads, capture offsets/hashes and Posted outcomes stay frozen. They compare actual v3 replay/delivery, check already-delivered reposts are zero, and prove the nonempty v2 segment stays byte-identical from its rebound baseline across rollover interruptions.

These output-identity checks do not claim equivalence of intentional v3 turn-state changes or a live provider/Discord measurement.
