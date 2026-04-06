import * as P from "./protocol.js";
import { $ } from "./util.js";

export function setupToolbar(app) {
  $("depth").addEventListener("change", (event) => {
    app.sendBinary(P.encodeSetDepth(parseInt(event.target.value) || 5));
  });

  $("color-mode").addEventListener("change", (event) => {
    app.sendBinary(P.encodeColorMode(parseInt(event.target.value) || 0));
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
      app.sendBinary(P.encodeFilterExt(extensions));

      const minValue = parseFloat($("filter-min").value) || 0;
      const minUnit = parseInt($("filter-min-unit").value) || 1;
      app.sendBinary(P.encodeFilterSize(Math.floor(minValue * minUnit), 0));

      app.sendBinary(P.encodeFilterName($("filter-name").value.trim()));
    }, 300);
  }

  for (const id of ["filter-ext", "filter-name", "filter-min"])
    $(id).addEventListener("input", sendFilter);
  $("filter-min-unit").addEventListener("change", sendFilter);

  $("filter-clear").addEventListener("click", () => {
    $("filter-ext").value = $("filter-name").value = $("filter-min").value = "";
    app.sendBinary(P.encodeClearFilter());
  });

  $("rescan").addEventListener("click", () =>
    app.sendBinary(P.encodeRescan()),
  );
}
