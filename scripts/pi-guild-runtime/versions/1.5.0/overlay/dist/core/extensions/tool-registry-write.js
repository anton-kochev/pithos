// Internal only: validation failures are writes too. Keep this out of ExtensionAPI until
// the owned-tools coordinator can enforce ownership and retirement as well as revision.
const writers = new WeakMap();
export function bindToolRegistryWriter(runtime, begin) {
    writers.set(runtime, begin);
}
export function beginToolRegistryWrite(runtime) {
    return writers.get(runtime)?.() ?? (() => true);
}
