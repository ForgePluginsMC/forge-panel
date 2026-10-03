// Shared helpers for forge-panel pages.
async function api(method, path, body) {
  const opts = { method, headers: {} };
  if (body !== undefined) {
    opts.headers["Content-Type"] = "application/json";
    opts.body = JSON.stringify(body);
  }
  const res = await fetch(path, opts);
  if (res.status === 401) {
    window.location.href = "/login";
    throw new Error("unauthorized");
  }
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data.error || ("HTTP " + res.status));
  return data;
}

function esc(s) {
  return String(s == null ? "" : s)
    .replace(/&/g, "&amp;").replace(/</g, "&lt;")
    .replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

function fmtMB(mb) {
  if (mb == null) return "—";
  if (mb >= 1024) return (mb / 1024).toFixed(1) + " GB";
  return mb + " MB";
}

function fmtBytes(b) {
  if (b == null) return "—";
  if (b >= 1073741824) return (b / 1073741824).toFixed(1) + " GB";
  if (b >= 1048576) return (b / 1048576).toFixed(1) + " MB";
  if (b >= 1024) return (b / 1024).toFixed(1) + " KB";
  return b + " B";
}

function serverNameFromPath() {
  const m = window.location.pathname.match(/^\/server\/(.+)$/);
  return m ? decodeURIComponent(m[1]) : null;
}

async function logout() {
  await api("POST", "/api/logout").catch(() => {});
  window.location.href = "/login";
}

// ---- analog control-room widgets ----

// Needle gauge. Usage: makeGauge(el, {label, unit, min, max, redline}) -> {set(v), el}
function makeGauge(el, opts) {
  const { label, unit, min, max, redline } = Object.assign(
    { label: "", unit: "", min: 0, max: 100, redline: null }, opts || {});
  const W = 170, H = 100, cx = W / 2, cy = H - 8, R = 74;
  const NS = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(NS, "svg");
  svg.setAttribute("width", W); svg.setAttribute("height", H);
  svg.setAttribute("viewBox", `0 0 ${W} ${H}`);
  const pol = (ang, r) => {
    const a = (ang * Math.PI) / 180;
    return [cx + r * Math.cos(a), cy - r * Math.sin(a)];
  };
  const arc = (a0, a1, r) => {
    const [x0, y0] = pol(a0, r), [x1, y1] = pol(a1, r);
    return `M ${x0} ${y0} A ${r} ${r} 0 0 1 ${x1} ${y1}`;
  };
  // face
  const face = document.createElementNS(NS, "path");
  face.setAttribute("d", `M 4 ${cy} A ${cx - 4} ${cx - 4} 0 0 1 ${W - 4} ${cy} Z`);
  face.setAttribute("fill", "#070b14");
  face.setAttribute("stroke", "#1d2942"); face.setAttribute("stroke-width", "2");
  svg.appendChild(face);
  // redline zone
  if (redline != null && redline < max) {
    const a0 = 180 - (180 * (redline - min)) / (max - min), a1 = 0;
    const zone = document.createElementNS(NS, "path");
    zone.setAttribute("d", arc(a0, a1, R - 8));
    zone.setAttribute("stroke", "#ff2d78"); zone.setAttribute("stroke-width", "7");
    zone.setAttribute("fill", "none"); zone.setAttribute("opacity", "0.75");
    svg.appendChild(zone);
  }
  // ticks
  for (let i = 0; i <= 10; i++) {
    const ang = 180 - i * 18;
    const [x0, y0] = pol(ang, R - 14), [x1, y1] = pol(ang, R - 6);
    const t = document.createElementNS(NS, "line");
    t.setAttribute("x1", x0); t.setAttribute("y1", y0);
    t.setAttribute("x2", x1); t.setAttribute("y2", y1);
    t.setAttribute("stroke", "#00e5ff"); t.setAttribute("stroke-width", i % 5 === 0 ? 2.5 : 1.2);
    svg.appendChild(t);
  }
  // needle
  const needle = document.createElementNS(NS, "line");
  needle.setAttribute("x1", cx); needle.setAttribute("y1", cy);
  needle.setAttribute("x2", cx); needle.setAttribute("y2", cy - (R - 16));
  needle.setAttribute("stroke", "#ff7b2f"); needle.setAttribute("stroke-width", "3.5");
  needle.setAttribute("stroke-linecap", "round");
  svg.appendChild(needle);
  const hub = document.createElementNS(NS, "circle");
  hub.setAttribute("cx", cx); hub.setAttribute("cy", cy); hub.setAttribute("r", "8");
  hub.setAttribute("fill", "#0d1424"); hub.setAttribute("stroke", "#00e5ff"); hub.setAttribute("stroke-width", "1.5");
  svg.appendChild(hub);

  const wrap = document.createElement("div");
  wrap.className = "gauge-wrap";
  wrap.appendChild(svg);
  const lab = document.createElement("div"); lab.className = "g-label"; lab.textContent = label;
  const val = document.createElement("div"); val.className = "g-val"; val.textContent = "—";
  wrap.appendChild(val); wrap.appendChild(lab);
  el.appendChild(wrap);

  function set(v) {
    if (v == null || isNaN(v)) { val.textContent = "—"; return; }
    const frac = Math.max(0, Math.min(1, (v - min) / (max - min)));
    const deg = 180 * frac; // 0 = pointing left, 180 = pointing right
    // rotate needle: default points up (90deg); we need angle from vertical
    const rot = -90 + deg;
    needle.setAttribute("transform", `rotate(${rot} ${cx} ${cy})`);
    val.textContent = (Math.round(v * 10) / 10) + (unit ? " " + unit : "");
  }
  set(min);
  return { set, el: wrap };
}

// LED bar graph: ledBar(container, frac, n=12) — lights segments, color ramps green->yellow->red.
function ledBar(el, frac, n) {
  n = n || 12;
  el.classList.add("ledbar");
  el.innerHTML = "";
  const lit = Math.round(Math.max(0, Math.min(1, frac)) * n);
  for (let i = 0; i < n; i++) {
    const s = document.createElement("i");
    if (i < lit) {
      const f = i / n;
      s.className = f < 0.6 ? "lit-g" : f < 0.85 ? "lit-y" : "lit-r";
    }
    el.appendChild(s);
  }
}

// Status lamp html: lamp("green"|"red"|"yellow"|"amber"|"off", big?)
function lamp(color, big) {
  const on = color && color !== "off" ? " on-" + color : "";
  return `<span class="lamp${big ? " big" : ""}${on}"></span>`;
}

