import { formatSize } from "./util.js";

export function setupPicker(app) {
  app.pickerElement.querySelector(".picker-refresh").addEventListener("click", () => {
    loadVolumes(app);
  });

  app.changeButton.addEventListener("click", () => {
    app.showPicker();
  });
}

export function loadVolumes(app) {
  const grid = app.pickerElement.querySelector(".picker-grid");
  grid.innerHTML = '<div class="picker-loading">Loading volumes...</div>';

  fetch("/volumes")
    .then((r) => r.json())
    .then((volumes) => {
      grid.innerHTML = "";
      if (volumes.length === 0) {
        grid.innerHTML = '<div class="picker-loading">No volumes found</div>';
        return;
      }
      for (const vol of volumes) {
        grid.appendChild(createVolumeCard(app, vol));
      }
    })
    .catch(() => {
      grid.innerHTML = '<div class="picker-loading">Failed to load volumes</div>';
    });
}

function createVolumeCard(app, vol) {
  const card = document.createElement("div");
  card.className = "volume-card";

  const usedPercent = vol.totalBytes > 0 ? (vol.usedBytes / vol.totalBytes) * 100 : 0;

  card.innerHTML = `
    <div class="volume-name">${escapeHtml(vol.name)}</div>
    <div class="volume-path">${escapeHtml(vol.mountPoint)}</div>
    <div class="volume-bar"><div class="volume-bar-fill" style="width:${usedPercent.toFixed(1)}%"></div></div>
    <div class="volume-sizes">${formatSize(vol.usedBytes)} used of ${formatSize(vol.totalBytes)}</div>
    ${vol.fsType ? `<div class="volume-fs">${escapeHtml(vol.fsType)}</div>` : ""}
  `;

  card.addEventListener("click", () => {
    app.scanPath(vol.mountPoint);
  });

  return card;
}

function escapeHtml(s) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}
