import * as P from "./protocol.js";
import { $ } from "./util.js";

export function setupToolbar(app) {
  $("depth").addEventListener("change", (event) => {
    app.sendBinary(
      new Uint8Array([P.MSG_SET_DEPTH, parseInt(event.target.value) || 5]),
    );
  });

  $("color-mode").addEventListener("change", (event) => {
    app.sendBinary(
      new Uint8Array([P.MSG_COLOR_MODE, parseInt(event.target.value) || 0]),
    );
  });

  let filterTimer = null;
  function sendFilter() {
    if (filterTimer) clearTimeout(filterTimer);
    filterTimer = setTimeout(() => {
      const extValue = $("filter-ext").value.trim();
      const extensions = extValue
        ? extValue
            .split(",")
            .map((s) => s.trim())
            .filter(Boolean)
        : [];
      const extPayload = [P.MSG_FILTER_EXT, extensions.length];
      for (const ext of extensions) {
        const bytes = P.textEncoder.encode(ext);
        extPayload.push(bytes.length, ...bytes);
      }
      app.sendBinary(new Uint8Array(extPayload));

      const minValue = parseFloat($("filter-min").value) || 0;
      const minUnit = parseInt($("filter-min-unit").value) || 1;
      const minBytes = Math.floor(minValue * minUnit);
      const sizeMsg = new DataView(new ArrayBuffer(17));
      sizeMsg.setUint8(0, P.MSG_FILTER_SIZE);
      sizeMsg.setBigUint64(1, BigInt(minBytes), true);
      sizeMsg.setBigUint64(9, 0n, true);
      app.sendBinary(new Uint8Array(sizeMsg.buffer));

      const namePattern = $("filter-name").value.trim();
      const nameBytes = P.textEncoder.encode(namePattern);
      const nameMsg = new DataView(new ArrayBuffer(3 + nameBytes.length));
      nameMsg.setUint8(0, P.MSG_FILTER_NAME);
      nameMsg.setUint16(1, nameBytes.length, true);
      new Uint8Array(nameMsg.buffer).set(nameBytes, 3);
      app.sendBinary(new Uint8Array(nameMsg.buffer));
    }, 300);
  }

  for (const id of ["filter-ext", "filter-name", "filter-min"])
    $(id).addEventListener("input", sendFilter);
  $("filter-min-unit").addEventListener("change", sendFilter);

  $("filter-clear").addEventListener("click", () => {
    $("filter-ext").value = $("filter-name").value = $("filter-min").value = "";
    app.sendBinary(new Uint8Array([P.MSG_CLEAR_FILTER]));
  });

  $("rescan").addEventListener("click", () =>
    app.sendBinary(new Uint8Array([P.MSG_RESCAN])),
  );
}
