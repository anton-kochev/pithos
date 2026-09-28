# Broker Compose policy: observed TDD ledger

## Scope and behavior list (before implementation)

Pure Rust policy only: no filesystem, environment lookup, Docker, subprocesses,
network, rendering, journal integration, or activation. Existing journal work is
not part of this change. Only direct `saphyr-parser = "=0.0.6"` is added to Cargo;
its version is already resolved transitively. Rust edition 2024 / declared MSRV
1.85 are unchanged. Available toolchain: 1.96.0; CI's 1.92.0 is not installed.

- Parse a minimal literal image service into an owned model with private fields.
- Allow only root services and declared named volumes, and the service allowlist;
  reject wrong types, nulls, missing/empty services and unknown fields.
- Bound logical names and counts; reserve browser and pithos-app.
- Require image XOR build; explicit tag or SHA-256 digest image references.
- Accept short/mapping builds with lexical relative context/Dockerfile paths;
  reject traversal, absolute/remote/platform-ambiguous paths. No FS containment claim.
- Accept argv-only command/entrypoint, literal string environment maps; reject
  interpolation, environment inheritance and implicit .env access.
- Accept short dependencies only, declared and acyclic.
- Accept declared named-volume:absolute-target[:ro|rw] mounts only; reject binds,
  anonymous/external volumes, drivers/options and ambiguous/duplicate targets.
- Reject duplicate keys, aliases/anchors, merge keys, tags, complex/non-string keys
  and multiple documents in a streaming preflight before the saphyr loader.
- Enforce <=64 KiB, <=16 collection depth and <=4096 parser events before loading;
  bound individual collections/strings as well.
- Redact all input-derived strings in model Debug and error Display/Debug/source.
- Exercise a realistic API/database model, attack cases and exact limit boundaries.

## Evidence

Each behavior is added as a focused test before its implementation. Compile-only
API scaffolding is not an implementation, and compilation errors are not Red.
All Cargo commands use `CARGO_HOME=/tmp/pithos-cargo`. Red commands select a test
with `cargo test --locked --test broker_compose <test> -- --exact`; Green runs the
entire growing `cargo test --locked --test broker_compose` suite. Results follow
as observed, not anticipated.

| Test / slice | Observed Red | Green |
| --- | --- | --- |
| `minimal_image_service_is_an_owned_typed_model` | `unwrap()` got Structure from compile-only parse scaffold | 1 passed |
| `schema_is_an_allowlist_not_a_compose_passthrough` | unsupported service field accepted | 2 passed |
| `logical_service_names_and_count_are_bounded` | invalid name accepted | 3 passed |
| `ambiguous_yaml_is_rejected_before_model_loading` | duplicate image key accepted | 4 passed |
| `byte_depth_and_event_budgets_apply_before_loading` | 65537-byte document accepted | 5 passed |
| `images_are_literal_bounded_explicit_references` | empty image accepted | 6 passed |
| `build_is_exclusive_and_paths_are_lexically_project_relative` | valid build returned Structure | 7 passed |
| `command_and_entrypoint_are_bounded_literal_argv` | valid argv returned Structure | 8 passed |
| `environment_is_a_bounded_literal_string_map` | valid literal map returned Structure | 9 passed |
| `dependencies_are_short_declared_unique_and_acyclic` | valid diamond dependency graph returned Structure | 10 passed |
| `volume_declarations_have_only_bounded_logical_identities` | valid declarations returned Structure | 11 passed |
| `mounts_are_declared_named_volumes_with_unambiguous_absolute_targets` | valid named mounts returned Structure | 12 passed |
| `model_debug_and_all_error_channels_redact_input_values` | Debug leaked caller data | 13 passed |

Each Red above was a compiled assertion failure (exit 101), not a missing symbol
or intentionally ignored test. Getter scaffolds supplied only enough type shape
to compile the next test; the corresponding parse behavior still failed.

Intermediate corrections, not additional claimed Red cycles:

- The schema slice initially exposed saphyr's panicking indexing on a missing
  key. Replaced production YAML indexing with checked `as_mapping_get` access;
  the same tests then passed. Model-map indexing is confined to known-valid tests.
- The preflight implementation initially failed compilation with E0382 (partial
  move of event tag). Borrowed the tag in the pattern; no cloning workaround.
- The 10000-deep flow fixture hits saphyr-parser's own checked scanner recursion
  guard during lookahead, before emitting the 17th collection. The initial test
  incorrectly required the policy's Limit error in this case. It now explicitly
  accepts either bounded rejection category (Yaml or Limit); the depth-16/17
  fixtures still assert the exact schema/Limit boundary. No parser error text is
  exposed or inspected to manufacture a classification.

Additional characterization, with no new production behavior (first run green,
16 total tests; not represented as Red-driven slices):

- `realistic_dotnet_api_and_database_preserve_only_the_approved_model`: .NET API
  build/argv/env, PostgreSQL image/env/persistent logical data and a second database
  client, shared read-only mount, explicit dependency and getter assertions.
- `escaped_duplicates_and_malformed_inputs_fail_closed`: duplicate nested and
  escaped keys, malformed YAML, collection tags, aliases inside argv, and syntax-
  looking characters preserved as literal data rather than regex-rejected.
- `exact_string_and_name_boundaries_are_accepted`: 40-byte names, 512-byte image,
  1024-byte build paths, and UTF-8 byte (not character) accounting for env values.

## Review fixes (additional observed Red–Green cycles)

Independent review found three parser differentials. Each fix was driven by a
compiled failing assertion before touching production code:

| Test | Observed Red | Green |
| --- | --- | --- |
| `raw_nul_cannot_hide_trailing_unvalidated_yaml` | `unwrap_err()` received an accepted valid-prefix model; raw NUL hid trailing bytes | Entire suite: 17 passed after rejecting raw NUL before the parser |
| `ambiguous_plain_core_scalars_require_quotes_for_literal_strings` | policy accepted unquoted `True` in a string-only environment value | Entire suite: 18 passed after style-aware rejection; quoted equivalents remain accepted |
| `overflowing_radix_integers_do_not_become_literal_strings` | policy accepted plain `0x8000000000000000` as a string after integer overflow | Entire suite: 19 passed after lexical hex/octal recognition independent of i64 range; quoted literals still accepted |

Commands used `CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test broker_compose <test> -- --exact`
for Red, then the entire Compose suite for Green. A second full Compose run after
rustfmt passed all 19 tests after the final numeric regression. Raw NUL is rejected anywhere in input, including
after a valid document. Non-lowercase YAML-core boolean/null spellings (`True`,
`TRUE`, `False`, `FALSE`, `Null`, `NULL`) must be quoted to denote literal strings;
they are deliberately rejected in plain style instead of relying on Saphyr's
incomplete core-type resolution. This is a conservative subset, not a YAML type
coercion feature. Plain `0x` hexadecimal and `0o` octal integer forms also require
quotes when used as literals, regardless of whether the integer fits in i64.

## Implemented contract and limits

Public entry point: `pithos::broker::compose::parse(&str) -> Result<Compose,
ComposeError>`. The owned Compose/Service/Build/Mount fields are private and have
read-only getters. No serializer or execution adapter is exposed. Debug omits all
input-derived strings even in nested public types. Errors carry static categories
only: source parser diagnostics, key names and values cannot leak. Explicit
getters do expose caller values, so consumers must not log arbitrary getter data.

- Root keys: required `services`, optional `volumes`, no others. Services: 1..=32;
  declarations: 0..=32. Service and volume names: `[a-z][a-z0-9-]{0,39}`, excluding
  `browser` and `pithos-app`. These are logical identities, not Docker IDs.
- Exactly one of image or build. Images: <=512 bytes, explicit tag (<=128 bytes)
  and/or lowercase 64-hex `sha256` digest; conservative lowercase repository
  components and optional numeric registry port. This is lexical policy, not
  registry existence, provenance, image safety or tag immutability verification.
- Build: short context string or mapping with required context and optional
  dockerfile (default `Dockerfile`, relative to context). Paths: <=1024 bytes,
  nonempty, relative, no parent/empty components, backslashes, colon, dollar sign,
  control characters or leading tilde. `.` context and `./api` are valid; a
  Dockerfile cannot end in `.`. Nothing is normalized, canonicalized or opened.
- Command/entrypoint: optional string arrays, <=64 elements, <=4096 bytes each;
  empty array is distinct from omission; nonempty array needs a nonempty first
  element. Empty later arguments are retained. Strings are never shell-split.
- Environment: string map only, <=128 pairs; keys `[A-Za-z_][A-Za-z0-9_]{0,127}`,
  values <=4096 bytes. Empty/multiline strings are retained. Null/bool/number/list
  forms and host environment inheritance are rejected.
- Literal argv/env prohibit NUL and **all dollar signs**, including `$$`; there is
  no expansion or escape interpretation for Compose interpolation. YAML string
  decoding still occurs normally. This conservative subset does not accept
  dollar-containing passwords or shell variable references.
- Dependencies: short list <=32, valid distinct declared service names, no self
  reference or cycles (including disconnected cycles). Input order is retained;
  acceptance does not claim dependency readiness.
- Named volume declarations accept only `{}` or null/empty declaration shorthand;
  supplied root `volumes: null` is rejected. No external/name/driver/options/labels.
  Short mounts only: `logical-name:/absolute/target[:ro|rw]`, <=32 per service,
  target <=1024 bytes. Default is rw. Reject binds, anonymous volumes, undeclared
  sources, extra modes, root target, dot/parent/empty components, control chars,
  backslashes, dollar signs and duplicate targets. Multiple consumers and repeated
  sources at distinct targets are valid. Physical volume ownership/adoption and
  persistence are explicitly later work, not authorized by this model.
- Preflight: <=65536 input bytes, <=16 simultaneously open collection nodes,
  <=4096 events including stream/document/end events. Check bytes first, then pull
  parser events and stop on failure; only after preflight call saphyr's loader.
  Reject anchors/aliases, tags, merge keys (even quoted), duplicate decoded string
  keys, non-string/complex keys, and multiple documents. No regex approximation
  or loader-first resource check. Scanner work is additionally bounded by input
  bytes and the parser's own checked flow-nesting limit.

## Verification

Final observed commands (all Cargo commands prefixed with
`CARGO_HOME=/tmp/pithos-cargo`):

- `cargo test --locked --test broker_compose`: **PASS**, 16 tests, none ignored.
  This entire suite also passed after rustfmt, documentation and limit-constant
  refactoring. The 13 incremental Red/Green commands are recorded above.
- `cargo check --locked --all-targets`: **PASS**.
- `cargo clippy --locked --all-targets -- -D warnings`: **PASS** (run twice).
- `rustfmt --edition 2024 --config skip_children=true src/broker/compose.rs tests/broker_compose.rs`:
  applied only to owned Rust files. `cargo fmt --check`: **PASS** (run twice);
  no journal formatting or other source changes made by this task.
- `git diff --check`: **PASS**. Cargo review confirmed the sole task addition is
  the exact parser dependency plus root lock entry; existing parent additions of
  libc/serde/serde_json were preserved, with no dependency resolution changes.
- `cargo test --locked --no-fail-fast -- --test-threads=1`: **BLOCKED** by two
  existing CLI integration tests, `cli_creates_pithos_on_empty_input` and
  `cli_creates_pithos_on_y_input`. Both expect `pithos info` to exit 0 after config
  creation, but it exits 1 with `No such file or directory (os error 2)` because
  the Docker executable is absent (`command -v docker` produces no path). CI
  explicitly documents this Docker prerequisite. All other non-ignored tests
  passed, including all 16 Compose and 27 journal integration tests. One existing
  Docker-dependent unit test remains ignored. No substitute Docker binary, test
  suppression or unrelated CLI fix was introduced.

Verification status is **Blocked** for the full suite, not for Compose behavior.
Toolchain 1.85/MSRV, CI-pinned 1.92.0 and macOS were not available for validation;
only installed Rust 1.96.0 on native Linux aarch64 was used. No filesystem/Docker/
daemon acceptance is claimed: path symlinks, races, protected snapshots, physical
volume identity, image behavior and execution lowering remain outside this pure
policy task. No commits were made.
