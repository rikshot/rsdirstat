import init from "./gen/rsdirstat_wasm.js";

const wasmModuleUrl = new URL("./gen/rsdirstat_wasm_bg.wasm", import.meta.url);

init({ module_or_path: wasmModuleUrl }).catch((error) => {
  console.error("Failed to initialize rsdirstat wasm bundle", error);
});
