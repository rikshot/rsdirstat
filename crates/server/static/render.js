import { formatSize } from "./util.js";

const GAP = 0.5;
const RADIUS = 3;

function insetRect(rect) {
  return {
    x: rect.x + GAP,
    y: rect.y + GAP,
    w: Math.max(0, rect.w - GAP * 2),
    h: Math.max(0, rect.h - GAP * 2),
  };
}

const FONT =
  "-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif";

const boldFonts = {};
const normalFonts = {};
for (let size = 6; size <= 16; size++) {
  boldFonts[size] = `600 ${size}px ${FONT}`;
  normalFonts[size] = `${size}px ${FONT}`;
}
let lastFont = "";

function truncateLabel(ctx, label, maxWidth) {
  let textWidth = ctx.measureText(label).width;
  if (textWidth <= maxWidth) return { label, textWidth };
  const charCount = Math.max(
    1,
    Math.floor((label.length * (maxWidth - 10)) / textWidth),
  );
  label = `${label.slice(0, charCount)}\u2026`;
  textWidth = ctx.measureText(label).width;
  return { label, textWidth };
}

export function drawSingleRect(ctx, rect, alpha) {
  const { x, y, w, h } = insetRect(rect);
  if (w < 0.5 || h < 0.5) return;

  ctx.globalAlpha = alpha;

  if (w < 4 || h < 4) {
    ctx.fillStyle = rect.isContainer ? rect.colorBackground : rect.colorDark;
    ctx.fillRect(x, y, w, h);
    ctx.globalAlpha = 1;
    return;
  }

  const radius = Math.min(RADIUS, w / 2, h / 2);

  if (rect.isContainer) {
    ctx.beginPath();
    ctx.roundRect(x, y, w, h, radius);
    ctx.fillStyle = rect.colorBackground;
    ctx.fill();

    if (rect.headerHeight > 0) {
      const visibleHeaderHeight = rect.headerHeight - GAP;
      ctx.beginPath();
      ctx.roundRect(x, y, w, visibleHeaderHeight, radius);
      ctx.fillStyle = rect.colorHeader;
      ctx.fill();

      const availableWidth = w - 8;
      if (availableWidth > 20 && visibleHeaderHeight > 8) {
        const fontSize = Math.min(12, Math.max(8, visibleHeaderHeight - 4));
        const font = boldFonts[fontSize];
        if (font !== lastFont) {
          ctx.font = font;
          lastFont = font;
        }
        ctx.fillStyle = "rgba(255,255,255,0.85)";
        ctx.textBaseline = "middle";
        const { label, textWidth } = truncateLabel(
          ctx,
          rect.name,
          availableWidth,
        );
        if (textWidth <= availableWidth) {
          ctx.fillText(label, x + 4, y + visibleHeaderHeight / 2);
          const sizeLabel = formatSize(rect.size);
          if (
            textWidth + ctx.measureText(`  ${sizeLabel}`).width <=
            availableWidth
          ) {
            ctx.fillStyle = "rgba(255,255,255,0.45)";
            ctx.fillText(
              `  ${sizeLabel}`,
              x + 4 + textWidth,
              y + visibleHeaderHeight / 2,
            );
          }
        }
      }
    }
  } else {
    ctx.beginPath();
    ctx.roundRect(x, y, w, h, radius);
    ctx.fillStyle = rect.colorDark;
    ctx.fill();
    ctx.strokeStyle = rect.colorBorder;
    ctx.lineWidth = 0.5;
    ctx.stroke();

    const availableWidth = w - 8;
    const availableHeight = h - 4;
    if (availableWidth > 28 && availableHeight > 13) {
      let fontSize = Math.round(
        Math.min(
          14,
          Math.max(
            9,
            Math.min(
              availableWidth / (rect.name.length * 0.6),
              availableHeight * 0.45,
            ),
          ),
        ),
      );
      const font = boldFonts[fontSize];
      if (font !== lastFont) {
        ctx.font = font;
        lastFont = font;
      }
      ctx.fillStyle = "rgba(255,255,255,0.92)";
      ctx.textBaseline = "top";
      const { label, textWidth } = truncateLabel(
        ctx,
        rect.name,
        availableWidth,
      );
      if (textWidth <= availableWidth) ctx.fillText(label, x + 4, y + 3);

      if (availableHeight > 26 && rect.size > 0) {
        const smallFontSize = Math.max(8, fontSize - 2);
        const smallFont = normalFonts[smallFontSize];
        if (smallFont !== lastFont) {
          ctx.font = smallFont;
          lastFont = smallFont;
        }
        ctx.fillStyle = "rgba(255,255,255,0.55)";
        const sizeLabel = formatSize(rect.size);
        if (ctx.measureText(sizeLabel).width <= availableWidth) {
          ctx.fillText(sizeLabel, x + 4, y + 3 + fontSize + 2);
        }
      }
    }
    ctx.globalAlpha = 1;
    return;
  }

  ctx.beginPath();
  ctx.roundRect(x, y, w, h, radius);
  ctx.strokeStyle = rect.colorBorder;
  ctx.lineWidth = 0.5;
  ctx.stroke();
  ctx.globalAlpha = 1;
}

export function drawRects(ctx, rects, alpha) {
  lastFont = "";
  for (const rect of rects) {
    if (rect.w >= 1 && rect.h >= 1) drawSingleRect(ctx, rect, alpha);
  }
}

export const easeOut = (progress) => 1 - (1 - progress) ** 3;

export function interpolateRects(from, to, fromMap, progress) {
  const result = [];
  const seen = new Set();

  for (const toRect of to) {
    const fromRect = fromMap.get(toRect.id);
    seen.add(toRect.id);
    if (fromRect) {
      result.push({
        ...toRect,
        x: fromRect.x + (toRect.x - fromRect.x) * progress,
        y: fromRect.y + (toRect.y - fromRect.y) * progress,
        w: fromRect.w + (toRect.w - fromRect.w) * progress,
        h: fromRect.h + (toRect.h - fromRect.h) * progress,
      });
    } else {
      result.push({
        ...toRect,
        x: toRect.x + toRect.w * 0.5 * (1 - progress),
        y: toRect.y + toRect.h * 0.5 * (1 - progress),
        w: toRect.w * progress,
        h: toRect.h * progress,
      });
    }
  }

  for (const fromRect of from) {
    if (!seen.has(fromRect.id)) {
      const inverse = 1 - progress;
      result.push({
        ...fromRect,
        x: fromRect.x + fromRect.w * 0.5 * progress,
        y: fromRect.y + fromRect.h * 0.5 * progress,
        w: fromRect.w * inverse,
        h: fromRect.h * inverse,
      });
    }
  }
  return result;
}

export function drawHoverOverlay(ctx, hoveredRect, hoveredAncestors) {
  for (const ancestor of hoveredAncestors) {
    const { x, y, w, h } = insetRect(ancestor);
    if (w > 0 && h > 0) {
      const radius = Math.min(RADIUS, w / 2, h / 2);
      if (ancestor.headerHeight > 0) {
        ctx.beginPath();
        ctx.roundRect(x, y, w, ancestor.headerHeight - GAP, radius);
        ctx.fillStyle = "rgba(255,255,255,0.05)";
        ctx.fill();
      }
      ctx.beginPath();
      ctx.roundRect(x, y, w, h, radius);
      ctx.strokeStyle = "rgba(255,255,255,0.3)";
      ctx.lineWidth = 1;
      ctx.stroke();
    }
  }

  if (hoveredRect) {
    const { x, y, w, h } = insetRect(hoveredRect);
    if (w > 0 && h > 0) {
      const radius = Math.min(RADIUS, w / 2, h / 2);
      const fillHeight =
        hoveredRect.isContainer && hoveredRect.headerHeight > 0
          ? hoveredRect.headerHeight - GAP
          : h;
      ctx.beginPath();
      ctx.roundRect(x, y, w, fillHeight, radius);
      ctx.fillStyle = "rgba(255,255,255,0.08)";
      ctx.fill();
      ctx.beginPath();
      ctx.roundRect(x, y, w, h, radius);
      ctx.strokeStyle = "rgba(255,255,255,0.7)";
      ctx.lineWidth = 1.5;
      ctx.stroke();
    }
  }
}
