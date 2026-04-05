export const MSG_SCAN_START = 1;
export const MSG_LAYOUT = 2;

export const MSG_VIEWPORT = 1;
export const MSG_NAVIGATE = 2;
export const MSG_REVEAL_DIR = 3;
export const MSG_REVEAL_FILE = 4;
export const MSG_RESCAN = 5;
export const MSG_SET_DEPTH = 6;
export const MSG_COLOR_MODE = 7;
export const MSG_FILTER_EXT = 8;
export const MSG_FILTER_SIZE = 9;
export const MSG_FILTER_NAME = 10;
export const MSG_CLEAR_FILTER = 11;

export const textEncoder = new TextEncoder();
export const textDecoder = new TextDecoder();

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
