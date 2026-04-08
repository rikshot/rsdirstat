export const $ = (id) => document.getElementById(id);

export function formatSize(bytes) {
  if (bytes < 0) bytes = 0;
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let index = 0;
  let value = bytes;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index++;
  }
  return index === 0
    ? `${value} ${units[index]}`
    : `${value.toFixed(value < 10 ? 2 : value < 100 ? 1 : 0)} ${units[index]}`;
}

const hslCache = {};
export function hsl(hue, saturation, lightness) {
  const key = hue * 10000 + saturation * 100 + lightness;
  return (hslCache[key] ??= `hsl(${hue},${saturation}%,${lightness}%)`);
}

export function applyColors(rect) {
  const { hue } = rect;
  rect.colorDark = hsl(hue, 62, 38);
  rect.colorBorder = hsl(hue, 60, 28);
  if (rect.isContainer) {
    rect.colorBackground = hsl(hue, 25, 13);
    rect.colorHeader = hsl(hue, 35, 20);
  }
}

export function hitTest(rect, mouseX, mouseY) {
  return (
    mouseX >= rect.x &&
    mouseX < rect.x + rect.w &&
    mouseY >= rect.y &&
    mouseY < rect.y + rect.h
  );
}

export function findRect(rects, mouseX, mouseY) {
  for (let i = rects.length - 1; i >= 0; i--) {
    if (hitTest(rects[i], mouseX, mouseY)) return rects[i];
  }
  return null;
}

export function findNavigableTarget(rects, mouseX, mouseY) {
  let target = null;
  for (const rect of rects) {
    if (
      hitTest(rect, mouseX, mouseY) &&
      !rect.isFiles &&
      !rect.isFile &&
      rect.id > 0n
    ) {
      target = rect;
    }
  }
  return target;
}
