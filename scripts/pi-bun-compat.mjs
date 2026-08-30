// Bun compatibility preload for the Pi CLI.
//
// Pi's bundled CLI (dist/bundle/cli.js) calls
// `node:worker_threads.markAsUncloneable` through undici's webidl helpers while
// loading extensions. Bun 1.3.14 does not implement that function, so *any*
// extension — even a no-op one — crashes the session before its factory runs:
//
//     TypeError: webidl.util.markAsUncloneable is not a function
//
// The real function only tags a value so structured-clone refuses to copy it;
// nothing in Pi's extension path depends on that tagging actually happening, so
// a no-op restores startup without changing behaviour. Installed only when the
// runtime lacks its own implementation, so Bun releases that ship the native
// function (and Node, which has had it since 22.x) keep it untouched.
//
// Loaded via `bun --preload /opt/pi-bun-compat.mjs` — see
// `dockerfile::PI_LAUNCH_ARGV`. Baked into the base image by `Dockerfile.base`
// and re-copied into every per-project image by the generated Dockerfile, so a
// launcher newer than the base image still ships its own shim.
import workerThreads from "node:worker_threads";

if (typeof workerThreads.markAsUncloneable !== "function") {
  Object.defineProperty(workerThreads, "markAsUncloneable", {
    configurable: true,
    enumerable: true,
    writable: true,
    value: (_value) => {},
  });
}
