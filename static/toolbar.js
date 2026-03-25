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
      const extensionValue = $("filter-ext").value.trim();
      if (extensionValue) {
        const parts = extensionValue
          .split(",")
          .map((extension) => P.textEncoder.encode(extension.trim()))
          .filter((encoded) => encoded.length);
        let totalLength = 2;
        for (const part of parts) totalLength += 1 + part.length;
        const payload = new Uint8Array(totalLength);
        payload[0] = P.MSG_FILTER_EXT;
        payload[1] = parts.length;
        let position = 2;
        for (const part of parts) {
          payload[position++] = part.length;
          payload.set(part, position);
          position += part.length;
        }
        app.sendBinary(payload);
      } else {
        app.sendBinary(new Uint8Array([P.MSG_FILTER_EXT, 0]));
      }
      const minValue = parseFloat($("filter-min").value) || 0;
      const minUnit = parseInt($("filter-min-unit").value) || 1;
      const sizeMessage = new DataView(new ArrayBuffer(17));
      sizeMessage.setUint8(0, P.MSG_FILTER_SIZE);
      sizeMessage.setBigUint64(1, BigInt(Math.floor(minValue * minUnit)), true);
      sizeMessage.setBigUint64(9, 0n, true);
      app.sendBinary(new Uint8Array(sizeMessage.buffer));
      const nameBytes = P.textEncoder.encode($("filter-name").value.trim());
      const nameMessage = new DataView(new ArrayBuffer(3 + nameBytes.length));
      nameMessage.setUint8(0, P.MSG_FILTER_NAME);
      nameMessage.setUint16(1, nameBytes.length, true);
      new Uint8Array(nameMessage.buffer).set(nameBytes, 3);
      app.sendBinary(new Uint8Array(nameMessage.buffer));
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
