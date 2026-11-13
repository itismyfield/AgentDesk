# Org auth document ownership

Code baseline: `8896695e659467a2243fe7b09123e26ba8f033b8`.
The Before and After configs share the same transition relation: this is a pure ownership move.
The catalog here is the existing recovery catalog, not a second global cache.

| Read result | Runtime before / after | Auth before / after |
|---|---|---|
| missing / unavailable root | clear 1, no document | install 0 / clear 0, no document |
| malformed / I/O failure / invalid auth | clear 1, no document | install 0 / clear 0, error |
| valid auth, invalid recovery | clear 1, no document | install 0 / clear 0, auth projection |
| valid auth and recovery | install 1, projection | install 0 / clear 0, auth projection |

Read precedes recovery validation; runtime installs or clears before returning the projection.
An auth read never validates or changes recovery state. Missing differs from malformed in its Result.
No clock or TTL is involved. Concurrent runtime readers still publish in completion order through
agent_recovery's existing coordinator mutex (`durable.rs:211-216`); there is no new cache, lock or
atomic-snapshot claim. File read and later publication may race with a writer exactly as before.
The writer lock and unknown-extension document remain in provider_auth_profile::repository.
The entire writer document file and all mutation/lock function bodies are byte-identical to main.

The model checks both preexisting catalog states, two purposes and five read outcomes. It abstracts
filesystem reads as outcomes and does not prove filesystem atomicity or cross-reader freshness.
AuthInstalls, DropInstall and DropClear are negative controls. Before/After are preservation checks,
not a claim of a preexisting bug. Runtime traces are also tested at the actual effect call seam.

## Entry points before

| main 8896695e coordinate | call / declaration |
|---|---|
| `src/services/discord/org_schema.rs:99` | `pub(crate) fn load_org_schema() -> Option<OrgSchema> {` |
| `src/services/discord/org_schema.rs:100` | `match load_org_schema_for_auth() {` |
| `src/services/discord/org_schema.rs:101` | `Ok(Some(schema)) => match install_org_recovery(&schema) {` |
| `src/services/discord/org_schema.rs:105` | `crate::services::agent_recovery::clear_catalog();` |
| `src/services/discord/org_schema.rs:110` | `crate::services::agent_recovery::clear_catalog();` |
| `src/services/discord/org_schema.rs:114` | `crate::services::agent_recovery::clear_catalog();` |
| `src/services/discord/org_schema.rs:121` | `fn load_org_schema_for_auth() -> Result<Option<OrgSchema>, String> {` |
| `src/services/discord/org_schema.rs:125` | `crate::services::provider_auth_profile::repository::read_org_schema(&path)` |
| `src/services/discord/org_schema.rs:136` | `pub(crate) fn parse_org_schema(content: &str) -> Result<OrgSchema, String> {` |
| `src/services/discord/org_schema.rs:227` | `let Some(schema) = load_org_schema_for_auth()? else {` |
| `src/services/discord/org_schema.rs:328` | `let schema = load_org_schema_for_auth()?;` |
| `src/services/discord/org_schema.rs:394` | `let schema = load_org_schema_for_auth()?;` |
| `src/services/discord/org_schema.rs:470` | `let (loaded, name) = (load_org_schema_for_auth()?, provider.as_str());` |
| `src/services/discord/org_schema.rs:519` | `let schema = load_org_schema();` |
| `src/services/discord/org_schema.rs:549` | `fn install_org_recovery(` |
| `src/services/discord/org_schema.rs:587` | `crate::services::agent_recovery::install_catalog(catalog);` |
| `src/services/discord/org_schema.rs:597` | `let schema = load_org_schema()?;` |
| `src/services/discord/org_schema.rs:643` | `let schema = load_org_schema()?;` |
| `src/services/discord/org_schema.rs:698` | `let schema = load_org_schema()?;` |
| `src/services/discord/org_schema.rs:711` | `let schema = load_org_schema()?;` |
| `src/services/discord/org_schema.rs:730` | `let Some(schema) = load_org_schema() else {` |
| `src/services/discord/org_schema.rs:771` | `let schema = load_org_schema()?;` |
| `src/services/provider_auth_profile/queries.rs:12` | `load_org_schema()` |
| `src/services/provider_auth_profile/queries.rs:18` | `load_org_schema()` |
| `src/services/provider_auth_profile/queries.rs:24` | `load_org_schema()` |
| `src/services/provider_auth_profile/queries.rs:30` | `let Some(schema) = load_org_schema() else {` |
| `src/services/provider_auth_profile/repository.rs:32` | `let mut document = load_org_document(org_path)?;` |
| `src/services/provider_auth_profile/repository.rs:68` | `let mut document = load_org_document(org_path)?;` |
| `src/services/provider_auth_profile/repository.rs:96` | `let mut document = load_org_document(org_path)?;` |
| `src/services/provider_auth_profile/repository.rs:145` | `let mut document = load_org_document(org_path)?;` |
| `src/services/provider_auth_profile/repository.rs:208` | `let mut document = load_org_document(org_path)?;` |
| `src/services/provider_auth_profile/repository.rs:260` | `pub(crate) fn read_org_schema(path: &Path) -> Result<Option<OrgSchema>, String> {` |
| `src/services/provider_auth_profile/repository.rs:262` | `Ok(content) => parse_org_schema(&content)` |
| `src/services/provider_auth_profile/repository/document.rs:95` | `pub(crate) fn load_org_document(path: &Path) -> Result<OrgDocument, String> {` |

The additional production decoder caller is Discord's effect-free registration read at
`src/services/discord/turn_presence/activity/unbound/live.rs:78`; it remains on the same parser
without recovery publication. All test and recovery-runtime call sites are retained in the lane's
`logs/entrypoints-before-all.txt` census.

## Entry points after

| implementation coordinate | call / declaration |
|---|---|
| `src/services/org_catalog.rs:17` | `pub(crate) fn read_auth_catalog_at(` |
| `src/services/org_catalog.rs:20` | `document::read_document_at(path).map(/document/ document.map(AuthCatalogSnapshot::from))` |
| `src/services/org_catalog.rs:23` | `pub(crate) fn load_runtime_auth_catalog() -> Option<AuthCatalogSnapshot> {` |
| `src/services/org_catalog.rs:26` | `document::load_runtime_document_at(path.as_deref()).map(AuthCatalogSnapshot::from)` |
| `src/services/org_catalog/document.rs:77` | `pub(crate) fn parse_org_schema(content: &str) -> Result<OrgSchema, String> {` |
| `src/services/org_catalog/document.rs:125` | `pub(in crate::services) fn read_document_at(path: &Path) -> Result<Option<OrgSchema>, String> {` |
| `src/services/org_catalog/document.rs:127` | `Ok(content) => parse_org_schema(&content)` |
| `src/services/org_catalog/document.rs:146` | `crate::services::agent_recovery::clear_catalog();` |
| `src/services/org_catalog/document.rs:150` | `pub(in crate::services) fn load_runtime_document_at(path: Option<&Path>) -> Option<OrgSchema> {` |
| `src/services/org_catalog/document.rs:151` | `load_runtime_with(path, &mut LiveRecovery)` |
| `src/services/org_catalog/document.rs:154` | `fn load_runtime_with(path: Option<&Path>, effects: &mut impl RecoveryEffects) -> Option<OrgSchema> {` |
| `src/services/org_catalog/document.rs:157` | `Ok(Some(schema)) => match install_org_recovery(&schema, effects) {` |
| `src/services/org_catalog/document.rs:177` | `fn install_org_recovery(` |
| `src/services/org_catalog/document.rs:221` | `effects.install(// crate::services::agent_recovery::install_catalog(catalog));` |
| `src/services/org_catalog/document.rs:225` | `pub(in crate::services) fn fallback_candidates(` |
| `src/services/discord/org_schema.rs:21` | `pub(crate) fn load_org_schema() -> Option<OrgSchema> {` |
| `src/services/discord/org_schema.rs:22` | `crate::services::org_catalog::load_runtime_document_at(org_schema_path().as_deref())` |
| `src/services/discord/org_schema.rs:25` | `fn load_org_schema_for_auth()` |
| `src/services/discord/org_schema.rs:30` | `crate::services::org_catalog::read_auth_catalog_at(&path)` |
| `src/services/discord/org_schema.rs:76` | `let Some(schema) = load_org_schema_for_auth()? else {` |
| `src/services/discord/org_schema.rs:190` | `let schema = load_org_schema_for_auth()?;` |
| `src/services/discord/org_schema.rs:256` | `let schema = load_org_schema_for_auth()?;` |
| `src/services/discord/org_schema.rs:264` | `let candidates = crate::services::org_catalog::fallback_candidates(` |
| `src/services/discord/org_schema.rs:340` | `let (loaded, name) = (load_org_schema_for_auth()?, provider.as_str());` |
| `src/services/discord/org_schema.rs:367` | `&& crate::services::org_catalog::fallback_candidates(` |
| `src/services/discord/org_schema.rs:397` | `let schema = load_org_schema();` |
| `src/services/discord/org_schema.rs:433` | `let schema = load_org_schema()?;` |
| `src/services/discord/org_schema.rs:479` | `let schema = load_org_schema()?;` |
| `src/services/discord/org_schema.rs:534` | `let schema = load_org_schema()?;` |
| `src/services/discord/org_schema.rs:547` | `let schema = load_org_schema()?;` |
| `src/services/discord/org_schema.rs:566` | `let Some(schema) = load_org_schema() else {` |
| `src/services/discord/org_schema.rs:607` | `let schema = load_org_schema()?;` |
| `src/services/discord/turn_presence/activity/unbound/live.rs:78` | `let schema = org_schema::parse_org_schema(text)?;` |
| `src/services/discord/org_writer.rs:44` | `let mut document = load_org_document(&org_path)?;` |
| `src/services/provider_auth_profile/queries.rs:12` | `load_runtime_auth_catalog()` |
| `src/services/provider_auth_profile/queries.rs:18` | `load_runtime_auth_catalog()` |
| `src/services/provider_auth_profile/queries.rs:24` | `load_runtime_auth_catalog()` |
| `src/services/provider_auth_profile/queries.rs:30` | `load_runtime_auth_catalog()` |
| `src/services/provider_auth_profile/repository/document.rs:95` | `pub(crate) fn load_org_document(path: &Path) -> Result<OrgDocument, String> {` |

The profile writer's lossless OrgDocument remains owned by repository::document. Its existing
Discord org_writer compatibility reader is retained with visibility narrowed to services; auth
projections expose no document handle. The decoded
wire schema is available through a service-scoped compatibility export used by Discord only;
queries and launch receive AuthCatalogSnapshot / PrimaryProfileMap / ProfileBindingInput.
`load_runtime_auth_catalog` uses the same production runtime_root -> runtime_layout path as
Discord's runtime_store::agentdesk_root / org_schema_path (main lines 22, 63-68).

## Model results

Plain TLC 1.7.4 checked five configs (8 invocations): Before / After GREEN; AuthInstalls,
DropInstall and DropClear each violate precisely their named invariant. The checker validates
both expected failures and passes; no config was skipped. Raw output: lane `logs/tlc.log`.

## Layer accounting

Official module_layers measurement: W445 -> 439, G346 -> 344, physical occurrences 607 -> 601;
read failures 0, module errors 0. The new org_catalog adapter responsibility is classified in
layers.toml. layer_moves.toml records the actual org_schema -> org_catalog::document moves:
recovery policy references 8 -> 8 and fallback policy imports 2 -> 2. No accepted baseline grows.
The existing unrelated main unclassified modules remain warnings, as confirmed by a historical
source replay; they are not counted as R01 reductions or reclassified here.
