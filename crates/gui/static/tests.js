// Browser-compatible unit tests for JS logic.
// Driven by Rust/Playwright — each test is a function that throws on failure.
import {
  formatSize,
  hitTest,
  hsl,
  applyColors,
  findRect,
  findNavigableTarget,
} from "./util.js";
import {
  encodeViewport,
  encodeNavigate,
  encodeRevealDir,
  encodeRevealFile,
  encodeRescan,
  encodeSetDepth,
  encodeColorMode,
  encodeClearFilter,
  encodeFilterExt,
  encodeFilterSize,
  encodeScanPath,
  encodeFilterName,
  parseLayout,
} from "./protocol.js";

function eq(a, b, msg) {
  if (a !== b) throw new Error(`${msg}: expected ${b}, got ${a}`);
}

function ok(v, msg) {
  if (!v) throw new Error(msg);
}

function view(buf) {
  return new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
}

function makeRect(overrides) {
  return {
    id: 1n,
    parentId: 0n,
    x: 0,
    y: 0,
    w: 100,
    h: 100,
    isContainer: false,
    isFiles: false,
    isFile: false,
    ...overrides,
  };
}

// --- util tests ---

function test_formatSize_bytes() {
  eq(formatSize(0), "0 B", "zero");
  eq(formatSize(512), "512 B", "512");
}

function test_formatSize_units() {
  eq(formatSize(1024), "1.00 KiB", "KiB");
  eq(formatSize(1024 * 1024), "1.00 MiB", "MiB");
  eq(formatSize(1024 ** 3), "1.00 GiB", "GiB");
  eq(formatSize(1024 ** 4), "1.00 TiB", "TiB");
}

function test_formatSize_negative() {
  eq(formatSize(-1), "0 B", "negative");
}

function test_hitTest_inside() {
  const r = makeRect({ x: 10, y: 20, w: 50, h: 30 });
  ok(hitTest(r, 35, 35), "center");
  ok(hitTest(r, 10, 20), "top-left corner");
}

function test_hitTest_outside() {
  const r = makeRect({ x: 10, y: 20, w: 50, h: 30 });
  ok(!hitTest(r, 60, 50), "bottom-right edge exclusive");
  ok(!hitTest(r, 5, 35), "left");
  ok(!hitTest(r, 65, 35), "right");
}

function test_hsl() {
  eq(hsl(120, 50, 40), "hsl(120,50%,40%)", "format");
  ok(hsl(200, 60, 30) === hsl(200, 60, 30), "cached");
}

function test_applyColors_container() {
  const r = makeRect({ hue: 120, isContainer: true });
  applyColors(r);
  ok(r.colorDark, "dark");
  ok(r.colorBorder, "border");
  ok(r.colorBackground, "background");
  ok(r.colorHeader, "header");
}

function test_applyColors_nonContainer() {
  const r = makeRect({ hue: 120 });
  applyColors(r);
  ok(r.colorDark, "dark");
  eq(r.colorBackground, undefined, "no background");
}

function test_findRect_topmost() {
  const parent = makeRect({ id: 1n, x: 0, y: 0, w: 100, h: 100 });
  const child = makeRect({ id: 2n, x: 10, y: 10, w: 50, h: 50 });
  eq(findRect([parent, child], 30, 30), child, "returns topmost");
}

function test_findRect_miss() {
  const r = makeRect({ x: 0, y: 0, w: 10, h: 10 });
  eq(findRect([r], 50, 50), null, "miss");
  eq(findRect([], 0, 0), null, "empty");
}

function test_findNavigableTarget_deepest() {
  const parent = makeRect({ id: 1n, x: 0, y: 0, w: 200, h: 200, isContainer: true });
  const child = makeRect({ id: 2n, parentId: 1n, x: 10, y: 10, w: 80, h: 80 });
  eq(findNavigableTarget([parent, child], 30, 30), child, "deepest");
}

function test_findNavigableTarget_skips_files() {
  const dir = makeRect({ id: 1n, x: 0, y: 0, w: 100, h: 100 });
  const file = makeRect({ id: -1n, parentId: 1n, x: 10, y: 10, w: 50, h: 50, isFile: true });
  eq(findNavigableTarget([dir, file], 30, 30), dir, "skips file");
}

function test_findNavigableTarget_skips_aggregate() {
  const dir = makeRect({ id: 1n, x: 0, y: 0, w: 100, h: 100 });
  const agg = makeRect({ id: -2n, parentId: 1n, x: 10, y: 10, w: 50, h: 50, isFiles: true });
  eq(findNavigableTarget([dir, agg], 30, 30), dir, "skips aggregate");
}

function test_findNavigableTarget_skips_zero_id() {
  const r = makeRect({ id: 0n, x: 0, y: 0, w: 100, h: 100 });
  eq(findNavigableTarget([r], 50, 50), null, "zero id");
}

function test_findNavigableTarget_leaf_dir_regression() {
  // Regression: leaf dirs (isContainer=false) must be navigable
  const parent = makeRect({ id: 1n, x: 0, y: 0, w: 200, h: 200, isContainer: true });
  const leaf = makeRect({ id: 2n, parentId: 1n, x: 0, y: 0, w: 150, h: 150, isContainer: false });
  const result = findNavigableTarget([parent, leaf], 50, 50);
  eq(result.id, 2n, "should select leaf dir, not parent");
}

// --- protocol tests ---

function test_encodeViewport() {
  const msg = encodeViewport(800, 600);
  const v = view(msg);
  eq(v.getUint8(0), 1, "tag");
  eq(v.getFloat32(1, true), 800, "width");
  eq(v.getFloat32(5, true), 600, "height");
  eq(msg.length, 9, "length");
}

function test_encodeNavigate() {
  const msg = encodeNavigate(42n);
  const v = view(msg);
  eq(v.getUint8(0), 2, "tag");
  eq(v.getBigUint64(1, true), 42n, "id");
}

function test_encodeRevealDir() {
  const v = view(encodeRevealDir(7n));
  eq(v.getUint8(0), 3, "tag");
  eq(v.getBigUint64(1, true), 7n, "id");
}

function test_encodeRevealFile() {
  const msg = encodeRevealFile(5n, "test.rs");
  const v = view(msg);
  eq(v.getUint8(0), 4, "tag");
  eq(v.getBigUint64(1, true), 5n, "parentId");
  eq(v.getUint16(9, true), 7, "name len");
  eq(new TextDecoder().decode(msg.slice(11)), "test.rs", "name");
}

function test_encodeRescan() {
  const msg = encodeRescan();
  eq(msg.length, 1, "length");
  eq(msg[0], 5, "tag");
}

function test_encodeSetDepth() {
  const msg = encodeSetDepth(3);
  eq(msg[0], 6, "tag");
  eq(msg[1], 3, "depth");
}

function test_encodeColorMode() {
  const msg = encodeColorMode(1);
  eq(msg[0], 7, "tag");
  eq(msg[1], 1, "mode");
}

function test_encodeClearFilter() {
  eq(encodeClearFilter()[0], 11, "tag");
}

function test_encodeFilterExt() {
  const msg = encodeFilterExt(["rs", "js"]);
  eq(msg[0], 8, "tag");
  eq(msg[1], 2, "count");
}

function test_encodeFilterSize() {
  const msg = encodeFilterSize(100, 999);
  const v = view(msg);
  eq(v.getUint8(0), 9, "tag");
  eq(v.getBigUint64(1, true), 100n, "min");
  eq(v.getBigUint64(9, true), 999n, "max");
}

function test_encodeFilterName() {
  const msg = encodeFilterName("main");
  const v = view(msg);
  eq(v.getUint8(0), 10, "tag");
  eq(new TextDecoder().decode(msg.slice(3)), "main", "pattern");
}

function test_encodeScanPath() {
  const msg = encodeScanPath("/home/user");
  const v = view(msg);
  eq(v.getUint8(0), 12, "tag");
  eq(new TextDecoder().decode(msg.slice(3)), "/home/user", "path");
}

function test_encodeScanPath_unicode() {
  const msg = encodeScanPath("/tmp/日本語");
  const v = view(msg);
  const len = v.getUint16(1, true);
  eq(new TextDecoder().decode(msg.slice(3, 3 + len)), "/tmp/日本語", "unicode path");
}

function test_parseLayout_empty() {
  const { v, offset, buffer } = buildLayoutBuffer({
    rootSize: 1000, dirCount: 5, scanDone: true, breadcrumb: [], rects: [],
  });
  const result = parseLayout(v, offset, buffer);
  eq(result.rootSize, 1000, "rootSize");
  eq(result.dirCount, 5, "dirCount");
  eq(result.scanDone, true, "scanDone");
  eq(result.breadcrumb.length, 0, "no breadcrumb");
  eq(result.rects.length, 0, "no rects");
}

function test_parseLayout_breadcrumb() {
  const { v, offset, buffer } = buildLayoutBuffer({
    rootSize: 5000, dirCount: 10, scanDone: false,
    breadcrumb: [{ id: 1, name: "root" }, { id: 2, name: "src" }],
    rects: [],
  });
  const result = parseLayout(v, offset, buffer);
  eq(result.breadcrumb.length, 2, "count");
  eq(result.breadcrumb[0].name, "root", "first name");
  eq(result.breadcrumb[1].name, "src", "second name");
}

function test_parseLayout_rect_fields() {
  const { v, offset, buffer } = buildLayoutBuffer({
    rootSize: 10000, dirCount: 1, scanDone: true, breadcrumb: [],
    rects: [{
      id: 42, parentId: 1, x: 10.5, y: 20.5, w: 100, h: 200,
      hue: 120, size: 5000, depth: 2,
      isContainer: true, isFiles: false, isFile: false,
      headerHeight: 18, mtime: 1700000000, name: "src",
    }],
  });
  const result = parseLayout(v, offset, buffer);
  const r = result.rects[0];
  eq(r.id, 42n, "id");
  eq(r.parentId, 1n, "parentId");
  eq(r.name, "src", "name");
  eq(r.depth, 2, "depth");
  eq(r.isContainer, true, "isContainer");
}

function test_parseLayout_flags() {
  const { v, offset, buffer } = buildLayoutBuffer({
    rootSize: 1000, dirCount: 1, scanDone: true, breadcrumb: [],
    rects: [
      { id: -1, parentId: 1, x: 0, y: 0, w: 50, h: 50, hue: 0, size: 100, depth: 0,
        isContainer: false, isFiles: false, isFile: true, name: "file.txt" },
      { id: -2, parentId: 1, x: 50, y: 0, w: 50, h: 50, hue: 0, size: 200, depth: 0,
        isContainer: false, isFiles: true, isFile: false, name: "(other)" },
    ],
  });
  const result = parseLayout(v, offset, buffer);
  eq(result.rects[0].isFile, true, "isFile");
  eq(result.rects[1].isFiles, true, "isFiles");
}

function buildLayoutBuffer({ rootSize, dirCount, scanDone, breadcrumb, rects }) {
  let size = 1 + 8 + 4 + 1 + 2;
  for (const bc of breadcrumb) size += 8 + 2 + new TextEncoder().encode(bc.name).length;
  size += 4;
  for (const r of rects) size += 8 + 8 + 16 + 2 + 8 + 1 + 1 + 4 + 8 + 2 + new TextEncoder().encode(r.name).length;

  const buffer = new ArrayBuffer(size);
  const v = new DataView(buffer);
  let off = 0;
  v.setUint8(off++, 2);
  v.setBigUint64(off, BigInt(rootSize), true); off += 8;
  v.setUint32(off, dirCount, true); off += 4;
  v.setUint8(off++, scanDone ? 1 : 0);
  v.setUint16(off, breadcrumb.length, true); off += 2;
  for (const bc of breadcrumb) {
    v.setBigUint64(off, BigInt(bc.id), true); off += 8;
    const nb = new TextEncoder().encode(bc.name);
    v.setUint16(off, nb.length, true); off += 2;
    new Uint8Array(buffer, off).set(nb); off += nb.length;
  }
  v.setUint32(off, rects.length, true); off += 4;
  for (const r of rects) {
    v.setBigInt64(off, BigInt(r.id), true); off += 8;
    v.setBigUint64(off, BigInt(r.parentId), true); off += 8;
    v.setFloat32(off, r.x, true); off += 4;
    v.setFloat32(off, r.y, true); off += 4;
    v.setFloat32(off, r.w, true); off += 4;
    v.setFloat32(off, r.h, true); off += 4;
    v.setUint16(off, r.hue, true); off += 2;
    v.setBigUint64(off, BigInt(r.size), true); off += 8;
    v.setUint8(off++, r.depth);
    v.setUint8(off++, (r.isContainer ? 1 : 0) | (r.isFiles ? 2 : 0) | (r.isFile ? 4 : 0));
    v.setFloat32(off, r.headerHeight || 0, true); off += 4;
    v.setBigInt64(off, BigInt(r.mtime || 0), true); off += 8;
    const nb = new TextEncoder().encode(r.name);
    v.setUint16(off, nb.length, true); off += 2;
    new Uint8Array(buffer, off).set(nb); off += nb.length;
  }
  return { v, offset: 1, buffer };
}

// --- runner ---

const tests = Object.entries({
  test_formatSize_bytes,
  test_formatSize_units,
  test_formatSize_negative,
  test_hitTest_inside,
  test_hitTest_outside,
  test_hsl,
  test_applyColors_container,
  test_applyColors_nonContainer,
  test_findRect_topmost,
  test_findRect_miss,
  test_findNavigableTarget_deepest,
  test_findNavigableTarget_skips_files,
  test_findNavigableTarget_skips_aggregate,
  test_findNavigableTarget_skips_zero_id,
  test_findNavigableTarget_leaf_dir_regression,
  test_encodeViewport,
  test_encodeNavigate,
  test_encodeRevealDir,
  test_encodeRevealFile,
  test_encodeRescan,
  test_encodeSetDepth,
  test_encodeColorMode,
  test_encodeClearFilter,
  test_encodeFilterExt,
  test_encodeFilterSize,
  test_encodeFilterName,
  test_encodeScanPath,
  test_encodeScanPath_unicode,
  test_parseLayout_empty,
  test_parseLayout_breadcrumb,
  test_parseLayout_rect_fields,
  test_parseLayout_flags,
});

export function runTests() {
  const failures = [];
  for (const [name, fn] of tests) {
    try {
      fn();
    } catch (e) {
      failures.push(`${name}: ${e.message}`);
    }
  }
  return { total: tests.length, failed: failures.length, failures };
}
