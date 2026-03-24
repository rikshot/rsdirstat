// Protocol message types (server → client)
const MSG_SCAN_START = 1;
const MSG_LAYOUT = 2;

// Protocol message types (client → server)
const MSG_VIEWPORT = 1;
const MSG_NAVIGATE = 2;
const MSG_REVEAL_DIR = 3;
const MSG_REVEAL_FILE = 4;
const MSG_RESCAN = 5;
const MSG_SET_DEPTH = 6;
const MSG_COLOR_MODE = 7;
const MSG_FILTER_EXT = 8;
const MSG_FILTER_SIZE = 9;
const MSG_FILTER_NAME = 10;
const MSG_CLEAR_FILTER = 11;

const textEncoder = new TextEncoder();

// DOM refs
const $ = (id) => document.getElementById(id);
const canvas = $("treemap");
let ctx = canvas.getContext("2d");
const breadcrumbBar = $("crumbs");
const tooltipEl = $("tooltip");
const statusEl = $("status");
const pathTextEl = $("path-text");
const pathSizeEl = $("path-size");
const ttName = tooltipEl.querySelector(".tt-name");
const ttSize = tooltipEl.querySelector(".tt-size");
const ttPct = tooltipEl.querySelector(".tt-pct");
const ttMtime = tooltipEl.querySelector(".tt-mtime");

// State
let layoutRects = [];
let viewRootSize = 0;
let breadcrumb = [];
let zoomAnim = null;
let pendingOldRects = null;
let dirty = true;
let hoveredRect = null;
let hoveredAncestors = [];
let lastMouseX = -1;
let lastMouseY = -1;
let lastBreadcrumbKey = "";
let dpr = devicePixelRatio || 1;
let canvasW = 0;
let canvasH = 0;
let scanDone = false;
let bufDirty = true;
let waitMode = new URLSearchParams(location.search).has("wait");
let scanStartTime = 0;
let scanTimer = null;
let ws = null;
let rafId = null;
let filterTimer = null;

// Constants
const ZOOM_DURATION = 300;
const BREADCRUMB_H = 32;
const TOOLBAR_H = 28;
const PATH_BAR_H = 24;
const GAP = 1;
const FONT = "-apple-system,BlinkMacSystemFont,sans-serif";

const bufCanvas = document.createElement("canvas");
const bufCtx = bufCanvas.getContext("2d");

// Utilities

function formatSize(bytes) {
  if (bytes < 0) bytes = 0;
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0;
  let v = bytes;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return i === 0
    ? `${v} ${units[i]}`
    : `${v.toFixed(v < 10 ? 2 : v < 100 ? 1 : 0)} ${units[i]}`;
}

const hslCache = {};
function hsl(hue, s, l) {
  const k = hue * 10000 + s * 100 + l;
  return (hslCache[k] ??= `hsl(${hue},${s}%,${l}%)`);
}

function applyColors(r) {
  const { hue: h } = r;
  r.color = hsl(h, 65, 50);
  r.colorDark = hsl(h, 62, 38);
  r.colorBorder = hsl(h, 60, 28);
  if (r.isContainer) {
    r.colorBg = hsl(h, 25, 13);
    r.colorHdr = hsl(h, 35, 20);
    r.colorHdrHover = hsl(h, 45, 30);
  }
}

function hitTest(r, mx, my) {
  return mx >= r.x && mx < r.x + r.w && my >= r.y && my < r.y + r.h;
}

// Layout & rendering

function resize() {
  const w = innerWidth;
  const h = innerHeight - BREADCRUMB_H - TOOLBAR_H - PATH_BAR_H;
  canvas.style.width = `${w}px`;
  canvas.style.height = `${h}px`;
  canvas.width = Math.round(w * dpr);
  canvas.height = Math.round(h * dpr);
  canvasW = w;
  canvasH = h;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  bufCanvas.width = canvas.width;
  bufCanvas.height = canvas.height;
  bufCtx.setTransform(dpr, 0, 0, dpr, 0, 0);
  bufDirty = dirty = true;
  scheduleTick();
  sendViewport();
}

function truncateLabel(label, maxWidth) {
  let tw = ctx.measureText(label).width;
  if (tw <= maxWidth) return { label, tw };
  const ch = Math.max(1, Math.floor((label.length * (maxWidth - 10)) / tw));
  label = `${label.slice(0, ch)}\u2026`;
  tw = ctx.measureText(label).width;
  return { label, tw };
}

function drawSingleRect(r, alpha) {
  const rx = r.x + GAP,
    ry = r.y + GAP;
  const rw = Math.max(0, r.w - GAP * 2);
  const rh = Math.max(0, r.h - GAP * 2);
  if (rw < 0.5 || rh < 0.5) return;

  ctx.globalAlpha = alpha;

  if (rw < 4 || rh < 4) {
    ctx.fillStyle = r.isContainer ? r.colorBg || r.colorDark : r.colorDark;
    ctx.fillRect(rx, ry, rw, rh);
    ctx.globalAlpha = 1;
    return;
  }

  if (r.isContainer) {
    ctx.fillStyle = r.colorBg;
    ctx.fillRect(rx, ry, rw, rh);

    if (r.headerH > 0) {
      const hh = r.headerH;
      ctx.fillStyle = r.colorHdr;
      ctx.fillRect(rx, ry, rw, hh);

      const pw = rw - 8;
      if (pw > 20 && hh > 8) {
        const fontSize = Math.min(12, Math.max(8, hh - 4));
        ctx.font = `600 ${fontSize}px ${FONT}`;
        ctx.fillStyle = "rgba(255,255,255,0.85)";
        ctx.textBaseline = "middle";
        const { label, tw } = truncateLabel(r.name, pw);
        if (tw <= pw) {
          ctx.fillText(label, rx + 4, ry + hh / 2);
          const sl = formatSize(r.size);
          if (tw + ctx.measureText(`  ${sl}`).width <= pw) {
            ctx.fillStyle = "rgba(255,255,255,0.45)";
            ctx.fillText(`  ${sl}`, rx + 4 + tw, ry + hh / 2);
          }
        }
      }
    }
  } else {
    ctx.fillStyle = r.colorDark;
    ctx.fillRect(rx, ry, rw, rh);

    const pw = rw - 8,
      ph = rh - 4;
    if (pw > 28 && ph > 13) {
      let fontSize = Math.round(
        Math.min(
          14,
          Math.max(9, Math.min(pw / (r.name.length * 0.6), ph * 0.45)),
        ),
      );
      ctx.font = `600 ${fontSize}px ${FONT}`;
      ctx.fillStyle = "rgba(255,255,255,0.92)";
      ctx.textBaseline = "top";
      const { label, tw } = truncateLabel(r.name, pw);
      if (tw <= pw) ctx.fillText(label, rx + 4, ry + 3);

      if (ph > 26 && r.size > 0) {
        const sf = Math.max(8, fontSize - 2);
        ctx.font = `${sf}px ${FONT}`;
        ctx.fillStyle = "rgba(255,255,255,0.55)";
        const sl = formatSize(r.size);
        if (ctx.measureText(sl).width <= pw)
          ctx.fillText(sl, rx + 4, ry + 3 + fontSize + 2);
      }
    }
  }

  ctx.strokeStyle = r.colorBorder;
  ctx.lineWidth = 0.5;
  ctx.strokeRect(rx, ry, rw, rh);
  ctx.globalAlpha = 1;
}

function drawRects(rects, alpha) {
  for (const r of rects) {
    if (r.w >= 1 && r.h >= 1) drawSingleRect(r, alpha);
  }
}

const easeOut = (t) => 1 - (1 - t) ** 3;

function interpolateRects(from, to, fromMap, t) {
  const result = [];
  const seen = new Set();

  for (const tr of to) {
    const fr = fromMap.get(tr.id);
    seen.add(tr.id);
    if (fr) {
      result.push({
        ...tr,
        x: fr.x + (tr.x - fr.x) * t,
        y: fr.y + (tr.y - fr.y) * t,
        w: fr.w + (tr.w - fr.w) * t,
        h: fr.h + (tr.h - fr.h) * t,
      });
    } else {
      result.push({
        ...tr,
        x: tr.x + tr.w * 0.5 * (1 - t),
        y: tr.y + tr.h * 0.5 * (1 - t),
        w: tr.w * t,
        h: tr.h * t,
      });
    }
  }

  for (const fr of from) {
    if (!seen.has(fr.id)) {
      const inv = 1 - t;
      result.push({
        ...fr,
        x: fr.x + fr.w * 0.5 * t,
        y: fr.y + fr.h * 0.5 * t,
        w: fr.w * inv,
        h: fr.h * inv,
      });
    }
  }
  return result;
}

function startZoom(fromRects, toRects) {
  const fromMap = new Map(fromRects.map((r) => [r.id, r]));
  zoomAnim = {
    from: fromRects,
    to: toRects,
    fromMap,
    startTime: performance.now(),
    duration: ZOOM_DURATION,
  };
  dirty = true;
  scheduleTick();
}

function render() {
  if (zoomAnim) {
    ctx.clearRect(0, 0, canvasW, canvasH);
    ctx.fillStyle = "#1a1a2e";
    ctx.fillRect(0, 0, canvasW, canvasH);
    const elapsed = performance.now() - zoomAnim.startTime;
    const t = Math.min(1, elapsed / zoomAnim.duration);
    drawRects(
      interpolateRects(
        zoomAnim.from,
        zoomAnim.to,
        zoomAnim.fromMap,
        easeOut(t),
      ),
      1,
    );
    if (t >= 1) {
      zoomAnim = null;
      bufDirty = true;
    } else {
      dirty = true;
    }
  } else {
    if (bufDirty) {
      const saved = ctx,
        savedHover = hoveredRect;
      ctx = bufCtx;
      hoveredRect = null;
      ctx.clearRect(0, 0, canvasW, canvasH);
      ctx.fillStyle = "#1a1a2e";
      ctx.fillRect(0, 0, canvasW, canvasH);
      drawRects(layoutRects, 1);
      ctx = saved;
      hoveredRect = savedHover;
      bufDirty = false;
    }
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.drawImage(bufCanvas, 0, 0);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);

    for (const ar of hoveredAncestors) {
      const ax = ar.x + GAP,
        ay = ar.y + GAP;
      const aw = Math.max(0, ar.w - GAP * 2),
        ah = Math.max(0, ar.h - GAP * 2);
      if (aw > 0 && ah > 0) {
        if (ar.headerH > 0) {
          ctx.fillStyle = "rgba(255,255,255,0.05)";
          ctx.fillRect(ax, ay, aw, ar.headerH);
        }
        ctx.strokeStyle = "rgba(255,255,255,0.3)";
        ctx.lineWidth = 1;
        ctx.strokeRect(ax, ay, aw, ah);
      }
    }

    if (hoveredRect) {
      const { x, y, w, h, isContainer, headerH } = hoveredRect;
      const rx = x + GAP,
        ry = y + GAP,
        rw = Math.max(0, w - GAP * 2),
        rh = Math.max(0, h - GAP * 2);
      if (rw > 0 && rh > 0) {
        ctx.fillStyle = "rgba(255,255,255,0.08)";
        ctx.fillRect(rx, ry, rw, isContainer && headerH > 0 ? headerH : rh);
        ctx.strokeStyle = "rgba(255,255,255,0.7)";
        ctx.lineWidth = 1.5;
        ctx.strokeRect(rx, ry, rw, rh);
      }
    }
  }
}

// Breadcrumb & path

function buildBreadcrumb() {
  const key = breadcrumb.map((b) => b.id).join(",");
  if (key === lastBreadcrumbKey) return;
  lastBreadcrumbKey = key;
  breadcrumbBar.innerHTML = "";
  for (const [i, { id, name }] of breadcrumb.entries()) {
    if (i > 0) {
      const sep = document.createElement("span");
      sep.className = "sep";
      sep.textContent = "/";
      breadcrumbBar.append(sep);
    }
    const span = document.createElement("span");
    span.textContent = name || "/";
    if (i === breadcrumb.length - 1) {
      span.className = "current";
    } else {
      span.addEventListener("click", () => navigateTo(id));
    }
    breadcrumbBar.append(span);
  }
}

function buildHoverPath(found) {
  const parts = breadcrumb.map((b) => b.name || "/");
  const sorted = [...hoveredAncestors].sort((a, b) => b.w * b.h - a.w * a.h);
  for (const s of sorted) parts.push(s.name);
  parts.push(found.name);
  return parts.join("/").replace(/\/+/g, "/");
}

// Navigation

function navigateTo(nodeId) {
  if (zoomAnim) return;
  hoveredRect = null;
  hoveredAncestors = [];
  tooltipEl.style.display = "none";
  pendingOldRects = layoutRects.map((r) => ({ ...r }));
  const b = new DataView(new ArrayBuffer(9));
  b.setUint8(0, MSG_NAVIGATE);
  b.setBigUint64(1, BigInt(nodeId), true);
  sendBinary(new Uint8Array(b.buffer));
}

function findRect(mx, my) {
  for (let i = layoutRects.length - 1; i >= 0; i--) {
    if (hitTest(layoutRects[i], mx, my)) return layoutRects[i];
  }
  return null;
}

function recomputeHover() {
  if (lastMouseX < 0) {
    hoveredRect = null;
    hoveredAncestors = [];
    return;
  }
  const found = findRect(lastMouseX, lastMouseY);
  hoveredRect = found;
  hoveredAncestors = found
    ? layoutRects.filter(
        (r) => r !== found && r.isContainer && hitTest(r, found.x, found.y),
      )
    : [];
  if (found) {
    pathTextEl.textContent = buildHoverPath(found);
    pathSizeEl.textContent = formatSize(found.size);
  } else {
    pathTextEl.textContent = pathSizeEl.textContent = "";
  }
}

// Event handlers

canvas.addEventListener("mousemove", (e) => {
  if (zoomAnim) return;
  const rect = canvas.getBoundingClientRect();
  lastMouseX = e.clientX - rect.left;
  lastMouseY = e.clientY - rect.top;

  const prev = hoveredRect;
  recomputeHover();

  if (hoveredRect !== prev) {
    dirty = true;
    scheduleTick();
    if (hoveredRect) {
      tooltipEl.style.display = "block";
      ttName.textContent = hoveredRect.name;
      ttSize.textContent = formatSize(hoveredRect.size);
      const pct =
        viewRootSize > 0 ? (hoveredRect.size / viewRootSize) * 100 : 0;
      ttPct.textContent = `${pct.toFixed(1)}% of parent`;
      ttMtime.textContent =
        hoveredRect.mtime > 0
          ? new Date(hoveredRect.mtime * 1000).toLocaleDateString()
          : "";
    } else {
      tooltipEl.style.display = "none";
    }
  }

  if (hoveredRect) {
    let tx = e.clientX + 14,
      ty = e.clientY + 14;
    if (tx + tooltipEl.offsetWidth > innerWidth - 8)
      tx = e.clientX - tooltipEl.offsetWidth - 8;
    if (ty + tooltipEl.offsetHeight > innerHeight - 8)
      ty = e.clientY - tooltipEl.offsetHeight - 8;
    tooltipEl.style.left = `${tx}px`;
    tooltipEl.style.top = `${ty}px`;
  }
});

canvas.addEventListener("mouseleave", () => {
  lastMouseX = lastMouseY = -1;
  hoveredRect = null;
  hoveredAncestors = [];
  tooltipEl.style.display = "none";
  pathTextEl.textContent = pathSizeEl.textContent = "";
  dirty = true;
  scheduleTick();
});

canvas.addEventListener("click", (e) => {
  if (zoomAnim) return;
  const rect = canvas.getBoundingClientRect();
  const mx = e.clientX - rect.left,
    my = e.clientY - rect.top;
  let target = null;
  for (const r of layoutRects) {
    if (
      hitTest(r, mx, my) &&
      !r.isFiles &&
      !r.isFile &&
      r.id > 0n &&
      r.isContainer
    )
      target = r;
  }
  if (target) navigateTo(target.id);
});

canvas.addEventListener("contextmenu", (e) => {
  e.preventDefault();
  if (!hoveredRect || !ws || ws.readyState !== 1) return;
  if (hoveredRect.isFile) {
    const nb = textEncoder.encode(hoveredRect.name);
    const b = new DataView(new ArrayBuffer(11 + nb.length));
    b.setUint8(0, MSG_REVEAL_FILE);
    b.setBigUint64(1, BigInt(hoveredRect.parentId), true);
    b.setUint16(9, nb.length, true);
    new Uint8Array(b.buffer).set(nb, 11);
    sendBinary(new Uint8Array(b.buffer));
  } else {
    const b = new DataView(new ArrayBuffer(9));
    b.setUint8(0, MSG_REVEAL_DIR);
    b.setBigUint64(1, BigInt(hoveredRect.id), true);
    sendBinary(new Uint8Array(b.buffer));
  }
});

// Render loop

function scheduleTick() {
  rafId ??= requestAnimationFrame(tick);
}
function tick() {
  rafId = null;
  if (dirty || zoomAnim) {
    render();
    dirty = bufDirty;
  }
  if (zoomAnim || dirty) scheduleTick();
}

addEventListener("resize", () => {
  dpr = devicePixelRatio || 1;
  resize();
});

// WebSocket

function sendBinary(buf) {
  if (ws?.readyState === 1) ws.send(buf.buffer);
}

function sendViewport() {
  if (!ws || ws.readyState !== 1 || canvasW <= 0) return;
  const b = new DataView(new ArrayBuffer(9));
  b.setUint8(0, MSG_VIEWPORT);
  b.setFloat32(1, canvasW, true);
  b.setFloat32(5, canvasH, true);
  sendBinary(new Uint8Array(b.buffer));
}

function startScanTimer() {
  if (scanTimer) return;
  scanStartTime = performance.now();
  scanTimer = setInterval(() => {
    statusEl.textContent = `Scanning... ${((performance.now() - scanStartTime) / 1000).toFixed(1)}s`;
  }, 100);
}

function connect() {
  const wsProto = location.protocol === "https:" ? "wss:" : "ws:";
  const td = new TextDecoder();
  statusEl.textContent = "Connecting...";
  ws = new WebSocket(`${wsProto}//${location.host}/ws`);
  ws.binaryType = "arraybuffer";

  ws.onopen = () => {
    if (waitMode) {
      statusEl.textContent = "";
      const btn = document.createElement("button");
      btn.textContent = "Start Scan";
      btn.style.cssText =
        "background:var(--accent);color:var(--link);border:1px solid var(--link);border-radius:4px;padding:2px 12px;cursor:pointer;font-size:12px";
      btn.onclick = () => {
        btn.disabled = true;
        btn.textContent = "Starting\u2026";
        fetch("/start");
        waitMode = false;
      };
      statusEl.append(btn);
    } else {
      statusEl.textContent = "Connected. Waiting for scan...";
    }
    sendViewport();
  };

  ws.onmessage = ({ data: buf }) => {
    if (!(buf instanceof ArrayBuffer)) return;
    const dv = new DataView(buf);
    let off = 0;
    const type = dv.getUint8(off++);

    if (type === MSG_SCAN_START) {
      const pathLen = dv.getUint16(off, true);
      off += 2;
      off += pathLen;
      layoutRects = [];
      breadcrumb = [];
      viewRootSize = 0;
      pendingOldRects = null;
      bufDirty = dirty = true;
      scheduleTick();
      if (scanTimer) {
        clearInterval(scanTimer);
        scanTimer = null;
      }
      startScanTimer();
      buildBreadcrumb();
    } else if (type === MSG_LAYOUT) {
      const viewRoot = dv.getBigUint64(off, true);
      off += 8;
      const rootSize = Number(dv.getBigUint64(off, true));
      off += 8;
      const dirCount = dv.getUint32(off, true);
      off += 4;
      const scanDone2 = dv.getUint8(off++) !== 0;

      const bcCount = dv.getUint16(off, true);
      off += 2;
      const bc = Array.from({ length: bcCount }, () => {
        const id = dv.getBigUint64(off, true);
        off += 8;
        const nl = dv.getUint16(off, true);
        off += 2;
        const name = td.decode(new Uint8Array(buf, off, nl));
        off += nl;
        return { id, name };
      });

      const rc = dv.getUint32(off, true);
      off += 4;
      const rects = Array.from({ length: rc }, () => {
        const id = dv.getBigInt64(off, true);
        off += 8;
        const parentId = dv.getBigUint64(off, true);
        off += 8;
        const x = dv.getFloat32(off, true);
        off += 4;
        const y = dv.getFloat32(off, true);
        off += 4;
        const w = dv.getFloat32(off, true);
        off += 4;
        const h = dv.getFloat32(off, true);
        off += 4;
        const hue = dv.getUint16(off, true);
        off += 2;
        const size = Number(dv.getBigUint64(off, true));
        off += 8;
        const depth = dv.getUint8(off++);
        const flags = dv.getUint8(off++);
        const headerH = dv.getFloat32(off, true);
        off += 4;
        const mtime = Number(dv.getBigInt64(off, true));
        off += 8;
        const nl = dv.getUint16(off, true);
        off += 2;
        const name = td.decode(new Uint8Array(buf, off, nl));
        off += nl;
        const r = {
          id,
          parentId,
          x,
          y,
          w,
          h,
          name,
          hue,
          size,
          depth,
          isContainer: !!(flags & 1),
          isFiles: !!(flags & 2),
          isFile: !!(flags & 4),
          headerH,
          mtime,
        };
        applyColors(r);
        return r;
      });

      if (pendingOldRects) {
        startZoom(pendingOldRects, rects);
        pendingOldRects = null;
      }
      layoutRects = rects;
      viewRootSize = rootSize;
      breadcrumb = bc;
      buildBreadcrumb();
      recomputeHover();
      bufDirty = dirty = true;
      scheduleTick();

      $("tb-rescan").style.display = scanDone2 ? "" : "none";
      if (!scanDone2) {
        startScanTimer();
      } else if (!scanDone) {
        if (scanTimer) {
          clearInterval(scanTimer);
          scanTimer = null;
        }
        const elapsed = ((performance.now() - scanStartTime) / 1000).toFixed(1);
        statusEl.textContent = `${dirCount} dirs in ${elapsed}s \u2014 ${formatSize(rootSize)}`;
      } else {
        statusEl.textContent = `${dirCount} dirs \u2014 ${formatSize(rootSize)}`;
      }
      scanDone = scanDone2;
    }
  };

  ws.onclose = () => {
    statusEl.textContent = "Disconnected. Reconnecting in 3s...";
    ws = null;
    setTimeout(connect, 3000);
  };
  ws.onerror = () => {
    statusEl.textContent = "Connection error.";
  };
}

// Toolbar

$("tb-depth").addEventListener("change", function () {
  sendBinary(new Uint8Array([MSG_SET_DEPTH, parseInt(this.value) || 5]));
});
$("tb-color").addEventListener("change", function () {
  sendBinary(new Uint8Array([MSG_COLOR_MODE, parseInt(this.value) || 0]));
});

function sendFilter() {
  if (filterTimer) clearTimeout(filterTimer);
  filterTimer = setTimeout(() => {
    // Extensions
    const extVal = $("tb-filter-ext").value.trim();
    if (extVal) {
      const parts = extVal
        .split(",")
        .map((e) => textEncoder.encode(e.trim()))
        .filter((e) => e.length);
      let total = 2;
      for (const p of parts) total += 1 + p.length;
      const b = new Uint8Array(total);
      b[0] = MSG_FILTER_EXT;
      b[1] = parts.length;
      let o = 2;
      for (const p of parts) {
        b[o++] = p.length;
        b.set(p, o);
        o += p.length;
      }
      sendBinary(b);
    } else {
      sendBinary(new Uint8Array([MSG_FILTER_EXT, 0]));
    }
    // Size
    const minVal = parseFloat($("tb-filter-min").value) || 0;
    const minUnit = parseInt($("tb-filter-min-unit").value) || 1;
    const sdv = new DataView(new ArrayBuffer(17));
    sdv.setUint8(0, MSG_FILTER_SIZE);
    sdv.setBigUint64(1, BigInt(Math.floor(minVal * minUnit)), true);
    sdv.setBigUint64(9, 0n, true);
    sendBinary(new Uint8Array(sdv.buffer));
    // Name
    const nb = textEncoder.encode($("tb-filter-name").value.trim());
    const nd = new DataView(new ArrayBuffer(3 + nb.length));
    nd.setUint8(0, MSG_FILTER_NAME);
    nd.setUint16(1, nb.length, true);
    new Uint8Array(nd.buffer).set(nb, 3);
    sendBinary(new Uint8Array(nd.buffer));
  }, 300);
}

for (const id of ["tb-filter-ext", "tb-filter-name", "tb-filter-min"])
  $(id).addEventListener("input", sendFilter);
$("tb-filter-min-unit").addEventListener("change", sendFilter);
$("tb-filter-clear").addEventListener("click", () => {
  $("tb-filter-ext").value =
    $("tb-filter-name").value =
    $("tb-filter-min").value =
      "";
  sendBinary(new Uint8Array([MSG_CLEAR_FILTER]));
});
$("tb-rescan").addEventListener("click", () =>
  sendBinary(new Uint8Array([MSG_RESCAN])),
);

// Init
resize();
connect();
scheduleTick();
