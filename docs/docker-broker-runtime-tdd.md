# Connected broker runtime: behavior list before implementation

Goal: connect the existing components into one owned runtime, not another set of
unconnected APIs. The production acceptance gate is not a test bypass. Real
native Linux/macOS Docker verification and container-reachable transport policy
remain explicit release requirements.

## Execution decision

Use mutually exclusive legacy and broker execution lanes. The broker lane must
never install the legacy browser signal handler, call its global cleanup, reuse
legacy automatic home repair, or instantiate the blocking clipboard service.
This avoids requiring a risky rewrite of every unrelated legacy command before
broker work can be connected. The legacy lane keeps its existing handlers.

Initial connected acceptance scope: cached immutable Pi identity image, existing
compatible home, browser off, no clipboard bridge. Unsupported combinations must
fail explicitly, never silently fall back or claim the full v1 broker is done.

## Ordered behaviors

1. Owned SIGINT/SIGTERM guard requests one shared Shutdown without process exit
   or Docker work; explicit close joins its receiver. Repeated signals do not
   bypass cleanup. Final exits preserve 130/143.
2. A separate inherited-TTY child owner supports interactive Pi without putting
   it in a background process group, captures/restores terminal state, and uses
   bounded poll/reap/termination after shutdown, with no probe runtime deadline.
3. A private home-use interlock grants brokers exclusive use and legacy callers
   shared use. Persist outstanding-use evidence before any home-consuming call;
   abnormal outcomes leave evidence. Never delete stale evidence automatically.
   Protect legacy run, browser home helper and session migration entry points.
4. Owned account/home/credential probes persist journal and resource identity
   before spawning; inspect labels, immutable IDs, configuration and completion;
   remove only recorded owned IDs and reconcile effects before advancing.
5. Separate work cancellation from bounded, same-frozen-daemon cleanup authority.
   No automatic replay or treating an empty inspect after uncertain create as
   proof of absence. No image-declared anonymous-volume side effects.
6. Incremental status connections let the runtime fairly poll Pi, cancellation
   and network work. Close listener/connections before credential consumers.
7. Connect grant, signals, lease, actual probes, Pi TTY and status into one
   runtime acceptance scenario using real processes/signals/locks/PTYs/sockets
   with a fake Docker boundary. Credentials are removed only after confirmed
   consumer settlement; uncertainty retains records/credentials/home-use debt.
8. Preserve the unconditional production-readiness gate until transport policy
   and real Docker acceptance are satisfied. No environment activation bypass.

Each behavior needs an observed assertion failure before production changes.
Record full Red/Green output under `/tmp/pithos-runtime-tdd/`; compile errors,
already-green characterization and removal of existing guards are not TDD Red.
No automatic migration/chown, dependency additions or commits are authorized.
