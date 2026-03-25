import { textDecoder } from "./protocol.js";
import { applyColors } from "./util.js";

export function parseLayout(view, offset, buffer) {
  offset += 8; // skip viewRoot
  const rootSize = Number(view.getBigUint64(offset, true));
  offset += 8;
  const dirCount = view.getUint32(offset, true);
  offset += 4;
  const scanDone = view.getUint8(offset++) !== 0;

  const breadcrumbCount = view.getUint16(offset, true);
  offset += 2;
  const breadcrumb = Array.from({ length: breadcrumbCount }, () => {
    const id = view.getBigUint64(offset, true);
    offset += 8;
    const nameLength = view.getUint16(offset, true);
    offset += 2;
    const name = textDecoder.decode(new Uint8Array(buffer, offset, nameLength));
    offset += nameLength;
    return { id, name };
  });

  const rectCount = view.getUint32(offset, true);
  offset += 4;
  const rects = Array.from({ length: rectCount }, () => {
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
    const nameLength = view.getUint16(offset, true);
    offset += 2;
    const name = textDecoder.decode(new Uint8Array(buffer, offset, nameLength));
    offset += nameLength;
    const rect = {
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
      headerHeight,
      mtime,
    };
    applyColors(rect);
    return rect;
  });

  return { rootSize, dirCount, scanDone, breadcrumb, rects };
}
