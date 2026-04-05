import * as P from "./protocol.js";
import { formatSize, hitTest } from "./util.js";

export function setupEvents(app) {
  app.canvas.addEventListener("mousemove", (event) => {
    if (app.zoomAnim) return;
    app.lastMouseX = event.clientX - app.canvasBounds.left;
    app.lastMouseY = event.clientY - app.canvasBounds.top;

    const previous = app.hoveredRect;
    app.recomputeHover();

    if (app.hoveredRect !== previous) {
      app.dirty = true;
      app.scheduleTick();
      if (app.hoveredRect) {
        app.tooltipElement.style.display = "block";
        app.tooltipName.textContent = app.hoveredRect.name;
        app.tooltipSize.textContent = formatSize(app.hoveredRect.size);
        const percent =
          app.viewRootSize > 0
            ? (app.hoveredRect.size / app.viewRootSize) * 100
            : 0;
        app.tooltipPercent.textContent = `${percent.toFixed(1)}%`;
        app.tooltipMtime.textContent =
          app.hoveredRect.mtime > 0
            ? new Date(app.hoveredRect.mtime * 1000).toLocaleDateString()
            : "";
      } else {
        app.tooltipElement.style.display = "none";
      }
    }

    if (app.hoveredRect) {
      let tipX = event.clientX + 14;
      let tipY = event.clientY + 14;
      if (tipX + app.tooltipElement.offsetWidth > innerWidth - 8)
        tipX = event.clientX - app.tooltipElement.offsetWidth - 8;
      if (tipY + app.tooltipElement.offsetHeight > innerHeight - 8)
        tipY = event.clientY - app.tooltipElement.offsetHeight - 8;
      app.tooltipElement.style.left = `${tipX}px`;
      app.tooltipElement.style.top = `${tipY}px`;
    }
  });

  app.canvas.addEventListener("mouseleave", () => {
    app.lastMouseX = app.lastMouseY = -1;
    app.clearHover();
    app.dirty = true;
    app.scheduleTick();
  });

  app.canvas.addEventListener("click", (event) => {
    if (app.zoomAnim) return;
    const target = app.findNavigableContainer(
      event.clientX - app.canvasBounds.left,
      event.clientY - app.canvasBounds.top,
    );
    if (target) app.navigateTo(target.id);
  });

  app.canvas.addEventListener("contextmenu", (event) => {
    event.preventDefault();
    if (!app.hoveredRect) return;
    if (app.hoveredRect.isFile) {
      app.sendBinary(
        P.encodeRevealFile(app.hoveredRect.parentId, app.hoveredRect.name),
      );
    } else {
      app.sendBinary(P.encodeRevealDir(app.hoveredRect.id));
    }
  });

  addEventListener("resize", () => {
    app.pixelRatio = devicePixelRatio || 1;
    app.resize();
  });
}
