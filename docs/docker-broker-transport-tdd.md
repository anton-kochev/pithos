# Docker broker container-to-host transport TDD ledger

## Literal inspected-gateway behavior list (2026-09-21 follow-up)

- Linux endpoint carries the twice-inspected bridge fingerprint and exact IPv4;
  callers cannot construct its gateway policy from arbitrary text or an address.
- After admission/preflight, immediately before durable Pi intent, inspect the
  frozen bridge again and reject any change/unavailability without publishing Pi
  intent or spawning Pi. Never bind a wildcard or use daemon `host-gateway`.
- Pi argv contains exactly one `--add-host host.docker.internal:<inspected IPv4>`;
  the durable Pi operation records that canonical bounded literal. Desktop and
  offline have no gateway or host mappings; probes continue with none.
- Reconciliation permits only the single exact persisted literal in
  `HostConfig.ExtraHosts`; absent, altered, duplicate, malformed and foreign
  mappings quarantine without removal or releasing evidence.
- Existing symbolic `host-gateway` Pi records lack a literal: manifest validation
  fails closed, and evidence must be retained. Do not auto-upgrade, adopt or
  remove the container. Recovery requires an operator to inspect the frozen
  daemon, container and historical evidence out of band and make an explicit
  migration/remediation decision; retrying with this version cannot authorize
  cleanup of the legacy container. No automatic conversion is safe because the
  daemon may have resolved the old symbolic mapping to a different address.

Test-first: the fake-Docker managed Pi assertion for literal argv and persisted
literal failed against the symbolic implementation before production edits;
`/tmp/pithos-runtime-tdd/literal-gateway/01-red/test.log` retains the assertion
failure. Focused green is in `02-green/focused.log`. A fake bridge transition
between preflight and intent, invalid legacy specs and mismatched Docker inspect
metadata are also covered. No real Docker daemon is available; these tests do
not establish native Docker or macOS integration acceptance.

## Authorization and behavior list

The user explicitly authorized this production transport on 2026-09-21:

- Docker Desktop macOS binds the broker listener only to `127.0.0.1` and
  advertises `host.docker.internal:<port>` to Pi.
- Native Linux inspects the frozen daemon's built-in `bridge`, requires one
  supported private IPv4 gateway/subnet, binds only that gateway (never a
  wildcard address), reinspects before admitting the endpoint, and advertises
  `host.docker.internal:<port>`.
- Initially Linux Pi received symbolic `host.docker.internal:host-gateway`;
  the literal follow-up above supersedes this; macOS and
  offline tests do not. Reconciliation requires the same exact policy and
  quarantines missing, duplicate, or foreign host mappings.
- Credential JSON and authenticated HTTP `Host` validation use the advertised
  authority, not the listener's local socket address.
- Malformed, unsupported, changed, ambiguous, public, loopback, multicast,
  unspecified, network-address, broadcast-address, or unbindable Linux bridge
  observations fail closed without falling back to wildcard binding.
- The frozen Docker executable/socket/config and daemon identity are rechecked
  around bridge inspection. No Docker socket or credential enters Pi.

## Test sequence

1. Add endpoint/runtime assertions proving loopback connection and advertised
   authority are distinct. Before production behavior, the status request using
   `host.docker.internal:<port>` must fail while the credential still contains
   `127.0.0.1:<port>`.
2. Add strict bridge parser tests covering the accepted observation and each
   rejected safety boundary.
3. Add fake-Docker inspect-bind-reinspect tests, including changed observations
   and exact-address bind failure. No wildcard fallback is allowed.
4. Add managed Pi argv and immutable reconciliation regressions for exact Linux
   host-gateway policy and for absent/foreign/duplicate mappings.
5. Run focused Green, format, Clippy, full offline suites, and independent review.

Real native Linux and Docker Desktop macOS acceptance remains mandatory before
claiming those platforms verified. The local environment has no Docker daemon;
fake-Docker tests are boundary evidence, not that acceptance.

## Observed implementation ledger

- The first endpoint test was written before the completed behavior and produced
  a real assertion Red: offline authority was `localhost:<port>` rather than the
  exact owned `127.0.0.1:<port>`. Full output is retained at
  `/tmp/pithos-runtime-tdd/transport/01-red.log`.
- `BrokerEndpoint` now inseparably owns its listener, bind address, advertised
  authority and `HostAccess`. Offline adoption accepts exact loopback only.
  Native Linux performs frozen `bridge` inspect, exact gateway bind and matching
  reinspection; macOS has a cfg-gated Docker Desktop loopback constructor.
- The Linux response uses a fixed Docker format template and bounded supervisor
  output, then deny-unknown serde at every object level. It accepts only the
  built-in local, non-internal, non-IPv6 bridge with default IPAM, no options,
  exactly one canonical RFC1918 IPv4 network/gateway pair, and no IP range or
  auxiliary addresses. The CIDR must be canonical, wholly private, and the
  gateway must be strictly between network and broadcast addresses.
- Fake-Docker tests cover accepted inspect-bind-reinspect, changed observations,
  exact unbindable addresses without fallback, malformed/unknown metadata,
  public, loopback, multicast, unspecified, network, broadcast, IPv6-only,
  extra-config and unsupported-IPAM cases.
- `BrokerRuntime::begin` consumes `BrokerEndpoint`; `RuntimeSetup` no longer has
  a transport selector. Status peers still connect to `local_addr`, while both
  credential endpoint and exact HTTP Host validation use the advertised
  authority. The runtime test exercises this distinction with the production
  platform constructor.
- The managed Pi slice now carries the endpoint's `HostAccess` into launch.
  The literal follow-up replaces Linux's former symbolic
  `--add-host host.docker.internal:host-gateway` before
  the image; Docker Desktop and offline launches append no host mapping. The
  bounded policy enum is persisted with the Pi resource, so recovery validates
  the policy selected at durable intent rather than current runtime state.
- Container inspection formerly accepted exactly one symbolic Linux mapping;
  the literal follow-up replaces this with exact persisted IPv4 matching. Linux
  missing, duplicate, foreign, or malformed `HostConfig.ExtraHosts`, and any
  nonempty Docker Desktop/offline value, are indeterminate and cannot authorize
  `docker container rm`. The fake Docker records `ExtraHosts` from launch argv
  and exercises normal Linux, Docker Desktop, offline, and mismatch cases.
- No token or endpoint is added to Pi argv or environment. The unconditional
  main gate was not changed.

The first managed-Pi policy test failed on the meaningful missing persisted
`host_access` assertion; output is retained at
`/tmp/pithos-runtime-tdd/transport/03-red.log`. Focused Green for that test is
retained at `/tmp/pithos-runtime-tdd/transport/04-green.log`.

Earlier focused Green output for transport and runtime is retained at
`/tmp/pithos-runtime-tdd/transport/02-green.log` (11 passed). Final focused
verification passed all 26 managed-Pi/runtime/transport tests serially, rustfmt,
and Clippy with `-D warnings`. Commands use `--locked --offline` with
`CARGO_HOME=/tmp/pithos-cargo` because the default Cargo cache is read-only in
this environment. Native Docker, macOS execution and MSRV execution were not
available and are not claimed.
