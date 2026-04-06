import * as P from "./protocol.js";
import { $, formatSize, hitTest } from "./util.js";
import {
  drawRects,
  drawHoverOverlay,
  interpolateRects,
  easeOut,
} from "./render.js";
import { parseLayout } from "./protocol.js";
import { setupEvents } from "./events.js";
import { setupToolbar } from "./toolbar.js";

const BACKGROUND = "#1a1a2e";
const BREADCRUMB_HEIGHT = 32;
const TOOLBAR_HEIGHT = 28;
const PATH_BAR_HEIGHT = 24;
const ZOOM_DURATION = 300;

class TreemapApp {
  constructor() {
    this.canvas = $("treemap");
    const contextOptions = { alpha: false };
    this.ctx = this.canvas.getContext("2d", contextOptions);
    this.breadcrumbBar = $("crumbs");
    this.tooltipElement = $("tooltip");
    this.statusElement = $("status");
    this.pathTextElement = $("path-text");
    this.pathSizeElement = $("path-size");
    this.tooltipName = this.tooltipElement.querySelector(".tip-name");
    this.tooltipSize = this.tooltipElement.querySelector(".tip-size");
    this.tooltipPercent = this.tooltipElement.querySelector(".tip-percent");
    this.tooltipMtime = this.tooltipElement.querySelector(".tip-mtime");
    this.bufferCanvas = document.createElement("canvas");
    this.bufferContext = this.bufferCanvas.getContext("2d", contextOptions);

    this.layoutRects = [];
    this.rectById = new Map();
    this.viewRootSize = 0;
    this.breadcrumb = [];
    this.zoomAnim = null;
    this.pendingOldRects = null;
    this.dirty = true;
    this.bufferDirty = true;
    this.pixelRatio = devicePixelRatio || 1;
    this.canvasWidth = 0;
    this.canvasHeight = 0;
    this.canvasBounds = this.canvas.getBoundingClientRect();
    this.rafId = null;
    this.hoveredRect = null;
    this.hoveredAncestors = [];
    this.lastMouseX = -1;
    this.lastMouseY = -1;
    this.lastBreadcrumbLength = -1;
    this.lastBreadcrumbTail = 0n;
    this.scanDone = false;
    this.waitMode = new URLSearchParams(location.search).has("wait");
    this.scanStartTime = 0;
    this.scanTimer = null;
    this.ws = null;

    setupEvents(this);
    setupToolbar(this);
    this.resize();
    this.connect();
    this.scheduleTick();
  }

  // Render

  scheduleTick() {
    this.rafId ??= requestAnimationFrame(() => this._tick());
  }

  _tick() {
    this.rafId = null;
    if (this.dirty || this.zoomAnim) {
      this._render();
      this.dirty = this.bufferDirty;
    }
    if (this.zoomAnim || this.dirty) this.scheduleTick();
  }

  _render() {
    const { ctx } = this;
    if (this.zoomAnim) {
      ctx.fillStyle = BACKGROUND;
      ctx.fillRect(0, 0, this.canvasWidth, this.canvasHeight);
      const progress = Math.min(
        1,
        (performance.now() - this.zoomAnim.startTime) / this.zoomAnim.duration,
      );
      drawRects(
        ctx,
        interpolateRects(
          this.zoomAnim.from,
          this.zoomAnim.to,
          this.zoomAnim.fromMap,
          easeOut(progress),
        ),
        1,
      );
      if (progress >= 1) {
        this.zoomAnim = null;
        this.bufferDirty = true;
      } else {
        this.dirty = true;
      }
    } else {
      if (this.bufferDirty) {
        this.bufferContext.fillStyle = BACKGROUND;
        this.bufferContext.fillRect(0, 0, this.canvasWidth, this.canvasHeight);
        drawRects(this.bufferContext, this.layoutRects, 1);
        this.bufferDirty = false;
      }
      ctx.setTransform(1, 0, 0, 1, 0, 0);
      ctx.drawImage(this.bufferCanvas, 0, 0);
      ctx.setTransform(this.pixelRatio, 0, 0, this.pixelRatio, 0, 0);
      drawHoverOverlay(ctx, this.hoveredRect, this.hoveredAncestors);
    }
  }

  resize() {
    const width = innerWidth;
    const height =
      innerHeight - BREADCRUMB_HEIGHT - TOOLBAR_HEIGHT - PATH_BAR_HEIGHT;
    this.canvas.style.width = `${width}px`;
    this.canvas.style.height = `${height}px`;
    this.canvas.width = Math.round(width * this.pixelRatio);
    this.canvas.height = Math.round(height * this.pixelRatio);
    this.canvasWidth = width;
    this.canvasHeight = height;
    this.ctx.setTransform(this.pixelRatio, 0, 0, this.pixelRatio, 0, 0);
    this.bufferCanvas.width = this.canvas.width;
    this.bufferCanvas.height = this.canvas.height;
    this.bufferContext.setTransform(
      this.pixelRatio,
      0,
      0,
      this.pixelRatio,
      0,
      0,
    );
    this.canvasBounds = this.canvas.getBoundingClientRect();
    this.bufferDirty = this.dirty = true;
    this.scheduleTick();
    this.sendViewport();
  }

  // Breadcrumb

  buildBreadcrumb() {
    const length = this.breadcrumb.length;
    const tail = length > 0 ? this.breadcrumb[length - 1].id : 0n;
    if (
      length === this.lastBreadcrumbLength &&
      tail === this.lastBreadcrumbTail
    )
      return;
    this.lastBreadcrumbLength = length;
    this.lastBreadcrumbTail = tail;
    this.breadcrumbBar.innerHTML = "";
    for (const [index, { id, name }] of this.breadcrumb.entries()) {
      if (index > 0) {
        const separator = document.createElement("span");
        separator.className = "separator";
        separator.textContent = "/";
        this.breadcrumbBar.append(separator);
      }
      const span = document.createElement("span");
      span.textContent = name || "/";
      if (index === this.breadcrumb.length - 1) {
        span.className = "current";
      } else {
        span.addEventListener("click", () => this.navigateTo(id));
      }
      this.breadcrumbBar.append(span);
    }
  }

  buildHoverPath(found) {
    const parts = this.breadcrumb.map((entry) => entry.name || "/");
    for (let index = this.hoveredAncestors.length - 1; index >= 0; index--) {
      parts.push(this.hoveredAncestors[index].name);
    }
    parts.push(found.name);
    return parts.join("/").replace(/\/+/g, "/");
  }

  // Navigation & hover

  clearHover() {
    this.hoveredRect = null;
    this.hoveredAncestors = [];
    this.tooltipElement.style.display = "none";
    this.pathTextElement.textContent = this.pathSizeElement.textContent = "";
  }

  navigateTo(nodeId) {
    if (this.zoomAnim) return;
    this.clearHover();
    this.pendingOldRects = this.layoutRects.map((rect) => ({ ...rect }));
    this.sendBinary(P.encodeNavigate(nodeId));
  }

  findRect(mouseX, mouseY) {
    for (let index = this.layoutRects.length - 1; index >= 0; index--) {
      if (hitTest(this.layoutRects[index], mouseX, mouseY))
        return this.layoutRects[index];
    }
    return null;
  }

  findNavigableContainer(mouseX, mouseY) {
    let target = null;
    for (const rect of this.layoutRects) {
      if (
        hitTest(rect, mouseX, mouseY) &&
        !rect.isFiles &&
        !rect.isFile &&
        rect.id > 0n &&
        rect.isContainer
      ) {
        target = rect;
      }
    }
    return target;
  }

  recomputeHover() {
    if (this.lastMouseX < 0) {
      this.hoveredRect = null;
      this.hoveredAncestors = [];
      return;
    }
    const found = this.findRect(this.lastMouseX, this.lastMouseY);
    this.hoveredRect = found;
    this.hoveredAncestors = [];
    if (found) {
      let current = this.rectById.get(found.parentId);
      while (current) {
        if (current.isContainer) this.hoveredAncestors.push(current);
        current = this.rectById.get(current.parentId);
      }
    }
    if (found) {
      this.pathTextElement.textContent = this.buildHoverPath(found);
      this.pathSizeElement.textContent = formatSize(found.size);
    } else {
      this.pathTextElement.textContent = this.pathSizeElement.textContent = "";
    }
  }

  // WebSocket

  sendBinary(buffer) {
    if (this.ws?.readyState === 1) this.ws.send(buffer.buffer);
  }

  sendViewport() {
    if (!this.ws || this.ws.readyState !== 1 || this.canvasWidth <= 0) return;
    this.sendBinary(P.encodeViewport(this.canvasWidth, this.canvasHeight));
  }

  startScanTimer() {
    if (this.scanTimer) return;
    this.scanStartTime = performance.now();
    this.scanTimer = setInterval(() => {
      this.statusElement.textContent = `Scanning... ${((performance.now() - this.scanStartTime) / 1000).toFixed(1)}s`;
    }, 100);
  }

  connect() {
    const wsProtocol = location.protocol === "https:" ? "wss:" : "ws:";
    this.statusElement.textContent = "Connecting...";
    this.ws = new WebSocket(`${wsProtocol}//${location.host}/ws`);
    this.ws.binaryType = "arraybuffer";

    this.ws.onopen = () => {
      if (this.waitMode) {
        this.statusElement.textContent = "";
        const button = document.createElement("button");
        button.textContent = "Start Scan";
        button.className = "action-button";
        button.onclick = () => {
          button.disabled = true;
          button.textContent = "Starting\u2026";
          fetch("/start");
          this.waitMode = false;
        };
        this.statusElement.append(button);
      } else {
        this.statusElement.textContent = "Connected. Waiting for scan...";
      }
      this.sendViewport();
    };

    this.ws.onmessage = ({ data: buffer }) => {
      if (!(buffer instanceof ArrayBuffer)) return;
      const view = new DataView(buffer);
      let offset = 0;
      const type = view.getUint8(offset++);

      if (type === P.MSG_SCAN_START) {
        this.layoutRects = [];
        this.breadcrumb = [];
        this.viewRootSize = 0;
        this.pendingOldRects = null;
        this.bufferDirty = this.dirty = true;
        this.scheduleTick();
        if (this.scanTimer) {
          clearInterval(this.scanTimer);
          this.scanTimer = null;
        }
        this.startScanTimer();
        this.buildBreadcrumb();
      } else if (type === P.MSG_LAYOUT) {
        this._handleLayout(parseLayout(view, offset, buffer));
      }
    };

    this.ws.onclose = () => {
      this.statusElement.textContent = "Disconnected. Reconnecting in 3s...";
      this.ws = null;
      setTimeout(() => this.connect(), 3000);
    };
    this.ws.onerror = () => {
      this.statusElement.textContent = "Connection error.";
    };
  }

  _handleLayout({
    rootSize,
    dirCount,
    scanDone: newScanDone,
    breadcrumb: newBreadcrumb,
    rects,
  }) {
    if (this.pendingOldRects) {
      const fromMap = new Map(
        this.pendingOldRects.map((rect) => [rect.id, rect]),
      );
      this.zoomAnim = {
        from: this.pendingOldRects,
        to: rects,
        fromMap,
        startTime: performance.now(),
        duration: ZOOM_DURATION,
      };
      this.dirty = true;
      this.scheduleTick();
      this.pendingOldRects = null;
    }
    this.layoutRects = rects;
    this.rectById = new Map(
      rects.filter((rect) => rect.id > 0n).map((rect) => [rect.id, rect]),
    );
    this.viewRootSize = rootSize;
    this.breadcrumb = newBreadcrumb;
    this.buildBreadcrumb();
    this.recomputeHover();
    this.bufferDirty = this.dirty = true;
    this.scheduleTick();

    $("rescan").classList.toggle("hidden", !newScanDone);
    if (!newScanDone) {
      this.startScanTimer();
    } else if (!this.scanDone) {
      if (this.scanTimer) {
        clearInterval(this.scanTimer);
        this.scanTimer = null;
      }
      const elapsed = ((performance.now() - this.scanStartTime) / 1000).toFixed(
        1,
      );
      this.statusElement.textContent = `${dirCount} dirs in ${elapsed}s \u2014 ${formatSize(rootSize)}`;
    } else {
      this.statusElement.textContent = `${dirCount} dirs \u2014 ${formatSize(rootSize)}`;
    }
    this.scanDone = newScanDone;
  }
}

if (document.readyState === "complete") {
  new TreemapApp();
} else {
  addEventListener("load", () => new TreemapApp());
}
