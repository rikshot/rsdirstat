import { applyColors } from "./util.js";

export const MSG_SCAN_START = 1;
export const MSG_LAYOUT = 2;
export const MSG_PICKER_MODE = 3;

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
const MSG_SCAN_PATH = 12;

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder();

export function encodeViewport(width, height) {
  const msg = new DataView(new ArrayBuffer(9));
  msg.setUint8(0, MSG_VIEWPORT);
  msg.setFloat32(1, width, true);
  msg.setFloat32(5, height, true);
  return new Uint8Array(msg.buffer);
}

export function encodeNavigate(id) {
  const msg = new DataView(new ArrayBuffer(9));
  msg.setUint8(0, MSG_NAVIGATE);
  msg.setBigUint64(1, BigInt(id), true);
  return new Uint8Array(msg.buffer);
}

export function encodeRevealDir(id) {
  const msg = new DataView(new ArrayBuffer(9));
  msg.setUint8(0, MSG_REVEAL_DIR);
  msg.setBigUint64(1, BigInt(id), true);
  return new Uint8Array(msg.buffer);
}

export function encodeRevealFile(parentId, name) {
  const nameBytes = textEncoder.encode(name);
  const msg = new DataView(new ArrayBuffer(11 + nameBytes.length));
  msg.setUint8(0, MSG_REVEAL_FILE);
  msg.setBigUint64(1, BigInt(parentId), true);
  msg.setUint16(9, nameBytes.length, true);
  new Uint8Array(msg.buffer).set(nameBytes, 11);
  return new Uint8Array(msg.buffer);
}

export function encodeRescan() {
  return new Uint8Array([MSG_RESCAN]);
}

export function encodeSetDepth(depth) {
  return new Uint8Array([MSG_SET_DEPTH, depth]);
}

export function encodeColorMode(mode) {
  return new Uint8Array([MSG_COLOR_MODE, mode]);
}

export function encodeClearFilter() {
  return new Uint8Array([MSG_CLEAR_FILTER]);
}

export function encodeFilterExt(extensions) {
  const payload = [MSG_FILTER_EXT, extensions.length];
  for (const ext of extensions) {
    const bytes = textEncoder.encode(ext);
    payload.push(bytes.length, ...bytes);
  }
  return new Uint8Array(payload);
}

export function encodeFilterSize(min, max) {
  const msg = new DataView(new ArrayBuffer(17));
  msg.setUint8(0, MSG_FILTER_SIZE);
  msg.setBigUint64(1, BigInt(min), true);
  msg.setBigUint64(9, BigInt(max), true);
  return new Uint8Array(msg.buffer);
}

export function encodeScanPath(path) {
  const pathBytes = textEncoder.encode(path);
  const msg = new DataView(new ArrayBuffer(3 + pathBytes.length));
  msg.setUint8(0, MSG_SCAN_PATH);
  msg.setUint16(1, pathBytes.length, true);
  new Uint8Array(msg.buffer).set(pathBytes, 3);
  return new Uint8Array(msg.buffer);
}

export function encodeFilterName(pattern) {
  const nameBytes = textEncoder.encode(pattern);
  const msg = new DataView(new ArrayBuffer(3 + nameBytes.length));
  msg.setUint8(0, MSG_FILTER_NAME);
  msg.setUint16(1, nameBytes.length, true);
  new Uint8Array(msg.buffer).set(nameBytes, 3);
  return new Uint8Array(msg.buffer);
}

export function parseLayout(view, offset, buffer) {
  const rootSize = Number(view.getBigUint64(offset, true));
  offset += 8;
  const dirCount = view.getUint32(offset, true);
  offset += 4;
  const scanDone = view.getUint8(offset++) !== 0;

  const breadcrumbCount = view.getUint16(offset, true);
  offset += 2;
  const breadcrumb = [];
  for (let i = 0; i < breadcrumbCount; i++) {
    const id = view.getBigUint64(offset, true);
    offset += 8;
    const nameLen = view.getUint16(offset, true);
    offset += 2;
    const name = textDecoder.decode(new Uint8Array(buffer, offset, nameLen));
    offset += nameLen;
    breadcrumb.push({ id, name });
  }

  const rectCount = view.getUint32(offset, true);
  offset += 4;
  const rects = [];
  for (let i = 0; i < rectCount; i++) {
    const id = view.getBigInt64(offset, true);
    offset += 8;
    const parentId = view.getBigUint64(offset, true);
    offset += 8;
    const x = view.getFloat32(offset, true);
    offset += 4;
    const y = view.getFloat32(offset, true);
    offset += 4;
    const w = view.getFloat32(offset, true);
    offset += 4;
    const h = view.getFloat32(offset, true);
    offset += 4;
    const hue = view.getUint16(offset, true);
    offset += 2;
    const size = Number(view.getBigUint64(offset, true));
    offset += 8;
    const depth = view.getUint8(offset++);
    const flags = view.getUint8(offset++);
    const headerHeight = view.getFloat32(offset, true);
    offset += 4;
    const mtime = Number(view.getBigInt64(offset, true));
    offset += 8;
    const nameLen = view.getUint16(offset, true);
    offset += 2;
    const name = textDecoder.decode(new Uint8Array(buffer, offset, nameLen));
    offset += nameLen;
    const rect = {
      id,
      parentId,
      x,
      y,
      w,
      h,
      hue,
      size,
      depth,
      isContainer: !!(flags & 1),
      isFiles: !!(flags & 2),
      isFile: !!(flags & 4),
      headerHeight,
      mtime,
      name,
    };
    applyColors(rect);
    rects.push(rect);
  }

  return { rootSize, dirCount, scanDone, breadcrumb, rects };
}
