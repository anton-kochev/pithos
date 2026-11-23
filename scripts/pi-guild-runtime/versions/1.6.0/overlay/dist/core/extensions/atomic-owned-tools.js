import { beginToolRegistryWrite } from "./tool-registry-write.js";
const coordinators = new WeakMap();
function coordinator(runtime) {
    let state = coordinators.get(runtime);
    if (!state) {
        state = { epoch: 0, claims: new Set() };
        coordinators.set(runtime, state);
    }
    return state;
}
function detach(state, claim) {
    claim.live = false;
    state.claims.delete(claim);
}
function notify(claim, reason) {
    try {
        // A callback may return a promise despite its void signature. Contain both failure forms.
        void Promise.resolve(claim.retire(reason)).catch(() => { });
    }
    catch {
        /* Retirement is a fence, not a veto. */
    }
}
export function retireAtomicOwnedTools(runtime, reason) {
    const state = coordinator(runtime);
    state.epoch++;
    state.backend = undefined;
    const claims = [...state.claims];
    for (const claim of claims)
        detach(state, claim);
    for (const claim of claims)
        notify(claim, reason);
}
export function retireLegacyOwnedTool(runtime, name) {
    const state = coordinator(runtime);
    const claims = [...state.claims].filter((claim) => claim.names.has(name));
    if (!claims.length)
        return true;
    const epoch = ++state.epoch;
    for (const claim of claims)
        detach(state, claim);
    for (const claim of claims)
        notify(claim, "superseded");
    return state.epoch === epoch;
}
export function bindAtomicOwnedToolsBackend(runtime, backend) {
    coordinator(runtime).backend = backend;
}
// Trusted in-process metadata: copy values, not JSON, so TypeBox symbols/functions survive.
// Only schemas are recursively copied; other nested extension metadata remains shared.
function snapshotSchema(value, check, seen = new Map()) {
    if (!value || typeof value !== "object")
        return value;
    const previous = seen.get(value);
    if (previous)
        return previous;
    const prototype = Object.getPrototypeOf(value);
    check();
    const copy = Array.isArray(value) ? [] : Object.create(prototype);
    seen.set(value, copy);
    const keys = Reflect.ownKeys(value);
    check();
    for (const key of keys) {
        if (Array.isArray(value) && key === "length")
            continue;
        const field = Reflect.get(value, key);
        check();
        Object.defineProperty(copy, key, {
            value: snapshotSchema(field, check, seen),
            enumerable: true,
            writable: true,
            configurable: true,
        });
    }
    return copy;
}
function snapshotDefinition(definition, check) {
    const keys = new Set();
    const seen = new Set();
    let prototype = definition;
    while (prototype && prototype !== Object.prototype && !seen.has(prototype)) {
        seen.add(prototype);
        const ownKeys = Reflect.ownKeys(prototype);
        check();
        for (const key of ownKeys)
            keys.add(key);
        prototype = Object.getPrototypeOf(prototype);
        check();
    }
    const copy = {};
    for (const key of keys) {
        const value = Reflect.get(definition, key);
        check();
        Object.defineProperty(copy, key, { value, enumerable: true, writable: true, configurable: true });
    }
    copy.parameters = snapshotSchema(copy.parameters, check);
    if (copy.outputSchema)
        copy.outputSchema = snapshotSchema(copy.outputSchema, check);
    return copy;
}
/** Factory identity is captured by the loader, never supplied by the extension. */
export function createAtomicOwnedTools(runtime, owner, assertActive) {
    const state = coordinator(runtime);
    return {
        protocol: "pi-atomic-owned-tools-v1",
        bind(names, onRetire) {
            assertActive();
            const epoch = ++state.epoch;
            const backend = state.backend;
            if (!backend)
                throw new Error("Atomic owned tools require a bound core");
            const group = new Set(names);
            if (!group.size ||
                group.size !== names.length ||
                [...group].some((name) => typeof name !== "string" || !name)) {
                throw new Error("Invalid owned tool group");
            }
            if (backend.collides(owner, group))
                throw new Error("Owned tool ownership collision");
            if (state.epoch !== epoch || state.backend !== backend)
                throw new Error("Owned tool binding conflict");
            const previous = [...state.claims].filter((claim) => [...group].some((name) => claim.names.has(name)));
            if (previous.some((claim) => claim.owner !== owner ||
                claim.names.size !== group.size ||
                [...group].some((name) => !claim.names.has(name)))) {
                throw new Error("Owned tool ownership overlap");
            }
            for (const claim of previous)
                detach(state, claim);
            const claim = { owner, names: group, live: true, retire: onRetire };
            state.claims.add(claim);
            // Install the new claim before callouts: a nested binding supersedes this one, never vice versa.
            for (const old of previous)
                notify(old, "superseded");
            let disposed = false;
            const current = () => claim.live && state.backend === backend;
            // Retirement callouts may supersede this claim or invalidate its backend.
            if (!current()) {
                detach(state, claim);
                throw new Error("Owned tool binding conflict");
            }
            return {
                protocol: "pi-atomic-owned-tools-v1",
                isActive: current,
                dispose() {
                    if (disposed)
                        return { status: "committed" };
                    if (!current())
                        return { status: "aborted", reason: "stale" };
                    const epoch = state.epoch;
                    const revisionCurrent = beginToolRegistryWrite(runtime);
                    const batch = [];
                    try {
                        const check = () => {
                            if (!current() || state.epoch !== epoch || !revisionCurrent())
                                throw new Error("Superseded withdrawal");
                        };
                        for (const name of group) {
                            const entry = owner.tools.get(name);
                            if (entry) {
                                const definition = snapshotDefinition(entry.definition, check);
                                definition.exposure = "hidden";
                                batch.push({ definition, activation: "deactivate" });
                            }
                        }
                        check();
                    }
                    catch {
                        return {
                            status: "aborted",
                            reason: current() && state.epoch === epoch && revisionCurrent() ? "invalid" : "conflict",
                        };
                    }
                    const result = backend.update(owner, batch, () => current() && state.epoch === epoch);
                    if (result.status === "committed") {
                        disposed = true;
                        detach(state, claim);
                        state.epoch++;
                    }
                    return result;
                },
                update(batch) {
                    if (!current())
                        return { status: "aborted", reason: "stale" };
                    const revisionCurrent = beginToolRegistryWrite(runtime);
                    const updateEpoch = state.epoch;
                    const valid = () => current() && state.epoch === updateEpoch && revisionCurrent();
                    let preparing = false;
                    try {
                        const check = () => {
                            if (!valid())
                                throw new Error("Superseded owned tool update");
                        };
                        const snapshot = batch.map((item) => {
                            const definition = snapshotDefinition(item.definition, check);
                            const activation = item.activation;
                            check();
                            return { definition, activation };
                        });
                        if (snapshot.length !== group.size ||
                            new Set(snapshot.map((item) => item.definition.name)).size !== group.size ||
                            snapshot.some((item) => !group.has(item.definition.name))) {
                            return { status: "aborted", reason: "invalid" };
                        }
                        if (!valid())
                            return { status: "aborted", reason: "conflict" };
                        // Backend starts the session revision, so retain only the ownership fence after entry.
                        preparing = true;
                        return backend.update(owner, snapshot, () => current() && state.epoch === updateEpoch);
                    }
                    catch {
                        return {
                            status: "aborted",
                            reason: !current() || state.epoch !== updateEpoch || (!preparing && !revisionCurrent())
                                ? "conflict"
                                : preparing
                                    ? "prepare-failed"
                                    : "invalid",
                        };
                    }
                },
            };
        },
    };
}
