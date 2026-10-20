//! Generation publication source guards, read from the runtime store they pin.

pub(super) fn production_once_cell_is_declared() {
    let production = include_str!("../../runtime_store.rs")
        .split_once("/// Production publishes")
        .unwrap()
        .1
        .split_once("struct GenerationFileLock")
        .unwrap()
        .0;
    let declaration = "static PROCESS_GENERATION: std::sync::OnceLock<";
    assert!(production.contains(declaration));
}

pub(super) fn production_binding_lexically_returns_the_stored_allocation_whole() {
    let source = include_str!("../../runtime_store.rs");
    let reader = source
        .split_once(
            "pub(in crate::services::discord) fn process_generation_binding() -> ProcessGenerationAllocation {",
        )
        .expect("process generation binding reader must remain present")
        .1
        .split_once("/// Preview the epoch that the replacement process will allocate")
        .expect("bounded reader end must remain present")
        .0;
    let production_read = reader
        .split_once("#[cfg(test)]")
        .expect("production and test reader branches must remain separate")
        .0;
    assert_eq!(
        production_read,
        "\n    #[cfg(not(test))]\n    if let Some(bound) = PROCESS_GENERATION.get() {\n        return *bound;\n    }\n    ",
        "the complete production reader prefix must exactly return the canonical whole allocation"
    );

    let process_reader = source
        .split_once("pub fn process_generation() -> u64 {")
        .expect("process generation accessor must remain present")
        .1
        .split_once(
            "pub(in crate::services::discord) fn process_generation_binding() -> ProcessGenerationAllocation {",
        )
        .expect("process generation accessor must remain bounded by the binding reader")
        .0
        .split_once("#[cfg(not(test))]")
        .expect("process generation production branch must remain present")
        .1
        .split_once("#[cfg(test)]")
        .expect("process generation production and test branches must remain separate")
        .0;
    assert_eq!(
        process_reader.trim(),
        "{\n        process_generation_binding().generation\n    }",
        "production row writers must return the published binding generation directly"
    );

    let allocator = source
        .split_once("pub(in crate::services::discord) fn allocate_process_generation_binding()")
        .expect("process generation allocator must remain present")
        .1
        .split_once("fn allocate_generation_epoch() -> ProcessGenerationAllocation {")
        .expect("process generation allocator must remain bounded by epoch allocation")
        .0
        .split_once("#[cfg(not(test))]")
        .expect("production allocator branch must remain present")
        .1
        .split_once("#[cfg(test)]")
        .expect("production and test allocator branches must remain separate")
        .0;
    assert_eq!(
        allocator.trim(),
        "{\n        allocate_once_with(\n            &PROCESS_GENERATION,\n            &PROCESS_GENERATION_ALLOCATION_TRANSACTION,\n            allocate_generation_epoch,\n        )\n    }",
        "the production allocator must exactly return the canonical whole-allocation publication"
    );

    let composer = source
        .split_once("fn allocate_generation_epoch() -> ProcessGenerationAllocation {")
        .expect("production allocation composer must remain present")
        .1
        .split_once("fn read_generation_counter(path: &Path) -> CounterRead {")
        .expect("production allocation composer must remain bounded")
        .0;
    assert_eq!(
        composer.trim(),
        "allocate_generation_epoch_with_io(GenerationIo {\n        path: generation_path(),\n        lock: lock_generation_path,\n        read: read_generation_counter,\n        write: atomic_write,\n        fsync: fsync_parent_dir,\n        flushes: PARENT_DIR_FSYNC_FLUSHES,\n    })\n}",
        "the production composer must be exactly the canonical allocation tail expression"
    );
}

pub(super) fn epoch_provenance_surface_makes_no_survival_claim() {
    let source = include_str!("../../runtime_store.rs");
    let surface = source
        .split_once("// <epoch-provenance-surface>")
        .unwrap()
        .1
        .split_once("// </epoch-provenance-surface>")
        .unwrap()
        .0
        .to_ascii_lowercase();
    for word in ["durable", "persistent", "persisted", "survive", "crash"] {
        assert!(!surface.contains(word), "forbidden={word}");
    }
}
