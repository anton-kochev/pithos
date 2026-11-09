/**
 * Internal admission state only, not persistence or AgentSession integration.
 * Host retains this coordinator; context consumers receive only capture()'s epoch-bound facade.
 * Epoch-retiring transitions must invalidateAll before callouts; native input uses revokePending. New capture is fresh host authority,
 * not permission to replay interrupted intent. Receipts never imply model read/usage.
 */
export function createParentContinuation(host) {
    let busy = false;
    let controls = 0;
    let parkedDispatch;
    let revokePending;
    let reservation = {};
    let epoch = {};
    const leases = new Set();
    const observers = new Map();
    let idleEdge = false;
    return {
        beginControl() {
            controls++;
            this.notifyIdle();
            let released = false;
            return () => {
                if (released)
                    return;
                released = true;
                controls--;
                if (controls === 0) {
                    const resume = parkedDispatch;
                    parkedDispatch = undefined;
                    resume?.();
                }
                this.notifyIdle();
            };
        },
        revokePending() {
            revokePending?.();
        },
        /** Notify on host eligibility transitions (including busy). Cleanup never emits idle itself. */
        notifyIdle() {
            const eligible = !busy && controls === 0 && host.isCurrent() && host.isEligibleIdle();
            if (!eligible) {
                idleEdge = false;
                return;
            }
            if (idleEdge)
                return;
            idleEdge = true;
            const snapshot = [...observers];
            queueMicrotask(() => {
                for (const [index, [owner, callback]] of snapshot.entries()) {
                    if (!leases.has(owner) ||
                        observers.get(owner) !== callback ||
                        busy ||
                        controls > 0 ||
                        !host.isCurrent() ||
                        !host.isEligibleIdle())
                        continue;
                    const before = reservation;
                    try {
                        Promise.resolve(callback()).catch(() => { });
                    }
                    catch {
                        /* Observers do not own cleanup. */
                    }
                    // A synchronous close or scheduling failure still consumes this notification turn.
                    if (reservation !== before) {
                        // Map order is the bounded round-robin cursor: move visited live owners to the tail.
                        // Read current callbacks so replacement/disposal during notification stays authoritative.
                        for (const [visited] of snapshot.slice(0, index + 1)) {
                            const current = observers.get(visited);
                            if (!current)
                                continue;
                            observers.delete(visited);
                            observers.set(visited, current);
                        }
                        break;
                    }
                }
            });
        },
        invalidateAll() {
            epoch = {};
            idleEdge = false;
            // Detach every old owner before any cancellation callback can reenter.
            const cleanup = [...leases].map((fence) => fence());
            for (const cancel of cleanup)
                cancel?.();
        },
        capture() {
            const captured = epoch;
            const available = host.isAuthorityAvailable?.() ?? true;
            return {
                open: () => {
                    if (!available ||
                        captured !== epoch ||
                        leases.size >= 16 ||
                        !host.isCurrent() ||
                        host.isAuthorityAvailable?.() === false)
                        return undefined;
                    let active = true;
                    let lastSequence = 0;
                    let closeDone;
                    const closed = new Promise((resolve) => {
                        closeDone = resolve;
                    });
                    let revoke;
                    const fence = () => {
                        if (!active)
                            return undefined;
                        active = false;
                        leases.delete(fence);
                        observers.delete(fence);
                        return () => {
                            if (revoke)
                                revoke();
                            else
                                closeDone();
                        };
                    };
                    leases.add(fence);
                    return {
                        closed,
                        onIdle(callback) {
                            if (active)
                                observers.set(fence, callback);
                        },
                        close() {
                            fence()?.();
                        },
                        tryAccept(sequence, content) {
                            if (!active || !host.isCurrent())
                                return { status: "stale" };
                            if (busy || controls > 0 || !host.isEligibleIdle())
                                return { status: "busy" };
                            if (!Number.isSafeInteger(sequence) ||
                                sequence <= lastSequence ||
                                typeof content !== "string" ||
                                content.length === 0 ||
                                content.length > 8192 ||
                                new TextEncoder().encode(content).length > 8192)
                                return { status: "invalid" };
                            lastSequence = sequence;
                            busy = true;
                            reservation = {};
                            idleEdge = false;
                            let resolve;
                            const finished = new Promise((done) => {
                                resolve = done;
                            });
                            let commitDone;
                            let startDone;
                            const committed = new Promise((done) => {
                                commitDone = done;
                            });
                            const startReceipt = new Promise((done) => {
                                startDone = done;
                            });
                            let commitState = "none";
                            let started = false;
                            let preparing = false;
                            let settled = false;
                            let requestRevoked = false;
                            const token = Object.freeze({});
                            const finish = (error = false) => {
                                if (settled)
                                    return;
                                settled = true;
                                parkedDispatch = undefined;
                                busy = false;
                                revoke = undefined;
                                revokePending = undefined;
                                commitDone(commitState);
                                startDone(started);
                                resolve({
                                    outcome: error
                                        ? "error"
                                        : !requestRevoked && active && host.isCurrent()
                                            ? "completed"
                                            : "revoked",
                                    commitState,
                                    started,
                                });
                                if (!active)
                                    closeDone();
                                // Reserved cancellation is synchronous: reporting it here could spin an idle
                                // observer that accepts then revokes. Native entry owns that idle transition.
                                if (preparing) {
                                    try {
                                        host.onDispatchFinished?.();
                                        host.onIdleCandidate?.();
                                    }
                                    catch {
                                        /* Host observation cannot change the terminal receipt. */
                                    }
                                }
                            };
                            const revokeRequest = () => {
                                if (settled || requestRevoked)
                                    return;
                                requestRevoked = true;
                                if (started) {
                                    try {
                                        host.cancel(token);
                                    }
                                    catch {
                                        /* Fencing must survive host cancellation errors. */
                                    }
                                }
                                if (!preparing)
                                    finish();
                            };
                            revoke = revokeRequest;
                            // Preparation is fenced, not aborted: the host must still settle dispatch/cleanup.
                            revokePending = () => {
                                if (!started)
                                    revokeRequest();
                            };
                            // Even an inline/reentrant scheduler cannot dispatch before tryAccept returns.
                            const run = () => {
                                queueMicrotask(() => {
                                    if (settled || preparing)
                                        return;
                                    preparing = true;
                                    let parked = false;
                                    Promise.resolve()
                                        .then(() => {
                                        if (requestRevoked || settled)
                                            return;
                                        if (controls > 0) {
                                            // No host work has begun: retain capacity without joining fictitious cleanup.
                                            preparing = false;
                                            parked = true;
                                            parkedDispatch = run;
                                            return;
                                        }
                                        if (!active || !host.isCurrent() || !host.isEligibleIdle()) {
                                            fence()?.();
                                            return;
                                        }
                                        return host.dispatch(Object.freeze({ sequence, content, token }), {
                                            canCommit: () => !requestRevoked &&
                                                active &&
                                                host.isCurrent() &&
                                                !settled &&
                                                commitState === "none",
                                            markCommitted: () => {
                                                if (requestRevoked ||
                                                    !active ||
                                                    !host.isCurrent() ||
                                                    settled ||
                                                    commitState !== "none")
                                                    return false;
                                                commitState = "committed";
                                                commitDone(commitState);
                                                return true;
                                            },
                                            markCommitUnknown: () => {
                                                if (!settled && commitState === "none") {
                                                    commitState = "unknown";
                                                    commitDone(commitState);
                                                }
                                            },
                                            markStarted: () => {
                                                if (requestRevoked ||
                                                    !active ||
                                                    !host.isCurrent() ||
                                                    settled ||
                                                    started ||
                                                    commitState !== "committed")
                                                    return false;
                                                started = true;
                                                startDone(true);
                                                return true;
                                            },
                                        });
                                    })
                                        .then(() => {
                                        if (!parked)
                                            finish();
                                    }, () => finish(true));
                                });
                            };
                            try {
                                (host.schedule ?? queueMicrotask)(run);
                            }
                            catch {
                                finish(true);
                            }
                            return {
                                status: "accepted",
                                receipt: { finished, committed, started: startReceipt, revoke: revokeRequest },
                            };
                        },
                    };
                },
            };
        },
    };
}
/** Internal lifecycle gate. Register sessions before building or binding extension contexts. */
export function createContinuationGate() {
    let transitions = 0;
    let inputs = 0;
    let shutdown = false;
    const members = new Set();
    const controls = new Set();
    return {
        isAuthorityAvailable: () => !shutdown && transitions === 0,
        isBusy: () => shutdown || transitions > 0 || inputs > 0 || controls.size > 0,
        beginControl() {
            const releases = new Map();
            controls.add(releases);
            for (const registration of members)
                releases.set(registration, registration.member.beginControl());
            return () => {
                if (!controls.delete(releases))
                    return;
                for (const release of releases.values())
                    release();
                releases.clear();
            };
        },
        beginInput() {
            inputs++;
            for (const { member } of [...members])
                member.revokePending();
            for (const { member } of [...members])
                member.notifyIdle();
            let released = false;
            return () => {
                if (released)
                    return;
                released = true;
                inputs--;
                for (const { member } of [...members])
                    member.notifyIdle();
            };
        },
        requestShutdown() {
            if (shutdown)
                return;
            shutdown = true;
            for (const { member } of [...members])
                member.invalidateAll();
        },
        register(member) {
            // Each registration owns its releases; an old detach cannot release a replacement.
            const registration = { member };
            members.add(registration);
            for (const releases of controls)
                releases.set(registration, member.beginControl());
            return () => {
                if (!members.delete(registration))
                    return;
                for (const releases of controls) {
                    const release = releases.get(registration);
                    releases.delete(registration);
                    release?.();
                }
            };
        },
        beginTransition() {
            transitions++;
            for (const { member } of [...members])
                member.invalidateAll();
            let released = false;
            return () => {
                if (released)
                    return;
                released = true;
                // Retire contexts created by lifecycle callbacks before exposing idle again.
                for (const { member } of [...members])
                    member.invalidateAll();
                transitions--;
                for (const { member } of [...members])
                    member.notifyIdle();
            };
        },
    };
}
