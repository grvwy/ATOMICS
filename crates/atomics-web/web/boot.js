// Starts the ATOMICS web client. The page carries no inline script (the client's origin allows none),
// so trunk's loader tag names the hashed module and wasm in data attributes, and this file loads them.
const tag = document.querySelector('script[data-atomics-module]');
const { default: init } = await import(tag.dataset.atomicsModule);
await init({ module_or_path: tag.dataset.atomicsWasm });
