// The background, drawn as SVG at start: a sunset over a bay, towers with lit
// windows, a bridge, palms and the water's reflection. No image files.

const NS = "http://www.w3.org/2000/svg";
const W = 1600;
const H = 900;
const HORIZON = 600;

function el(name, attrs = {}, parent) {
  const node = document.createElementNS(NS, name);
  for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, v);
  parent?.appendChild(node);
  return node;
}

// The same picture at every start.
function random(seed) {
  let s = seed;
  return () => {
    s = (s * 1664525 + 1013904223) >>> 0;
    return s / 4294967296;
  };
}

function gradient(defs, id, stops, vertical = true) {
  const g = el("linearGradient", { id, x1: 0, y1: 0, x2: vertical ? 0 : 1, y2: vertical ? 1 : 0 }, defs);
  for (const [offset, color, opacity = 1] of stops) {
    el("stop", { offset, "stop-color": color, "stop-opacity": opacity }, g);
  }
}

function defs(svg) {
  const d = el("defs", {}, svg);
  gradient(d, "sky", [[0, "#12031f"], [0.3, "#3d0a5c"], [0.55, "#a0217d"], [0.8, "#ff5e7a"], [1, "#ffb35c"]]);
  gradient(d, "sun", [[0, "#fff1a8"], [0.45, "#ffb347"], [1, "#ff3d8b"]]);
  gradient(d, "water", [[0, "#3a0b4d"], [0.4, "#1d0630"], [1, "#09020f"]]);
  gradient(d, "glow", [[0, "#ff4fb2", 0.55], [1, "#ff4fb2", 0]]);
  const blur = el("filter", { id: "blur", x: "-10%", y: "-10%", width: "120%", height: "120%" }, d);
  el("feGaussianBlur", { stdDeviation: 3 }, blur);
  const soft = el("filter", { id: "soft", x: "-50%", y: "-50%", width: "200%", height: "200%" }, d);
  el("feGaussianBlur", { stdDeviation: 18 }, soft);
  // The sun's bands, cut out of its lower half.
  const mask = el("mask", { id: "bands" }, d);
  el("rect", { x: 0, y: 0, width: W, height: H, fill: "#fff" }, mask);
  for (let i = 0; i < 6; i++) {
    el("rect", { x: 0, y: 470 + i * 24, width: W, height: 3 + i * 2, fill: "#000" }, mask);
  }
}

function sky(svg, rnd) {
  el("rect", { width: W, height: HORIZON, fill: "url(#sky)" }, svg);
  const stars = el("g", { class: "stars" }, svg);
  for (let i = 0; i < 90; i++) {
    const star = el("circle", {
      cx: rnd() * W, cy: rnd() * 300, r: rnd() * 1.4 + 0.3, fill: "#fff", opacity: rnd() * 0.6 + 0.2,
    }, stars);
    // Through the style object: the page's policy refuses style attributes.
    star.style.animationDelay = `${(rnd() * 6).toFixed(2)}s`;
  }
  const sun = el("g", {}, svg);
  el("circle", { cx: 1180, cy: 520, r: 230, fill: "url(#glow)", filter: "url(#soft)" }, sun);
  el("circle", { cx: 1180, cy: 500, r: 150, fill: "url(#sun)", mask: "url(#bands)" }, sun);
  const clouds = el("g", { class: "clouds" }, svg);
  for (let i = 0; i < 9; i++) {
    const y = 200 + rnd() * 260;
    el("ellipse", {
      cx: rnd() * W, cy: y, rx: 120 + rnd() * 220, ry: 6 + rnd() * 10,
      fill: y > 380 ? "#ff8a7a" : "#c43a9a", opacity: 0.25 + rnd() * 0.25,
    }, clouds);
  }
}

function towers(parent, rnd, { from, to, base, min, max, color, lit }) {
  let x = from;
  while (x < to) {
    const w = 26 + rnd() * 46;
    const h = min + rnd() * (max - min);
    el("rect", { x, y: base - h, width: w, height: h, fill: color }, parent);
    if (rnd() < 0.3) el("rect", { x: x + w / 2 - 1, y: base - h - 24, width: 2, height: 24, fill: color }, parent);
    if (lit) {
      for (let wy = base - h + 8; wy < base - 6; wy += 9) {
        for (let wx = x + 5; wx < x + w - 5; wx += 7) {
          if (rnd() < 0.42) {
            el("rect", {
              x: wx, y: wy, width: 3, height: 4,
              fill: rnd() < 0.7 ? "#ffcf7a" : "#ff7ad9", opacity: 0.55 + rnd() * 0.45,
            }, parent);
          }
        }
      }
    }
    x += w + 2 + rnd() * 10;
  }
}

function city(svg, rnd) {
  const g = el("g", { id: "city" }, svg);
  towers(g, rnd, { from: 0, to: W, base: HORIZON, min: 40, max: 150, color: "#3b0f55", lit: false });
  towers(g, rnd, { from: 300, to: 900, base: HORIZON, min: 90, max: 330, color: "#1d0730", lit: true });
  towers(g, rnd, { from: 1240, to: W, base: HORIZON, min: 80, max: 280, color: "#1d0730", lit: true });
  bridge(g);
  return g;
}

function bridge(g) {
  const deck = 548;
  el("rect", { x: 880, y: deck, width: W - 880, height: 7, fill: "#22082f" }, g);
  for (let x = 900; x < W; x += 70) {
    el("rect", { x, y: deck, width: 9, height: HORIZON - deck, fill: "#22082f" }, g);
    el("circle", { cx: x + 35, cy: deck - 2, r: 2.2, fill: "#ffd27a", class: "lamp" }, g);
  }
  for (let x = 900; x < W; x += 140) {
    el("path", { d: `M${x} ${deck} Q${x + 70} ${deck - 46} ${x + 140} ${deck}`, stroke: "#5a1a6e", "stroke-width": 2, fill: "none" }, g);
  }
}

function water(svg, city) {
  el("rect", { y: HORIZON, width: W, height: H - HORIZON, fill: "url(#water)" }, svg);
  const mirror = el("g", { transform: `translate(0 ${2 * HORIZON}) scale(1 -1)`, opacity: 0.38, filter: "url(#blur)" }, svg);
  mirror.appendChild(city.cloneNode(true)).removeAttribute("id");
  el("ellipse", { cx: 1180, cy: 650, rx: 120, ry: 70, fill: "#ff9a5c", opacity: 0.35, filter: "url(#soft)" }, svg);
  const ripples = el("g", { class: "ripples" }, svg);
  for (let i = 0; i < 26; i++) {
    const y = HORIZON + 8 + i * i * 0.45;
    const w = 60 + i * 9;
    el("rect", { x: 1180 - w / 2, y, width: w, height: 2 + i * 0.12, fill: "#ffc070", opacity: 0.5 - i * 0.015 }, ripples);
  }
  el("rect", { y: HORIZON - 1, width: W, height: 2, fill: "#ff7ab8", opacity: 0.6 }, svg);
}

// A palm: a curved trunk and fronds that droop from its crown, each a rib
// with leaflets on both sides.
function palm(svg, rnd, { x, top, lean, scale, flip }) {
  const ink = "#0d0216";
  const g = el("g", { transform: `translate(${x} 0) scale(${flip ? -1 : 1} 1)` }, svg);
  const cx = lean;
  el("path", {
    d: `M-16 ${H} Q${cx * 0.3} ${(H + top) / 2} ${cx - 4} ${top} L${cx + 8} ${top + 4} Q${cx * 0.3 + 20} ${(H + top) / 2} 16 ${H} Z`,
    fill: ink,
  }, g);
  const leaves = el("g", { stroke: ink, "stroke-linecap": "round", fill: "none" }, g);
  for (let i = 0; i < 11; i++) {
    const angle = ((-180 + i * 18 + rnd() * 10) * Math.PI) / 180;
    const len = (170 + rnd() * 80) * scale;
    const tip = [cx + Math.cos(angle) * len, top + Math.sin(angle) * len * 0.35 + len * 0.55];
    const ctrl = [cx + Math.cos(angle) * len * 0.55, top + Math.sin(angle) * len * 0.55 - 30 * scale];
    const at = (t) => [0, 1].map((k) => (1 - t) ** 2 * [cx, top][k] + 2 * (1 - t) * t * ctrl[k] + t * t * tip[k]);
    el("path", { d: `M${cx} ${top} Q${ctrl[0]} ${ctrl[1]} ${tip[0]} ${tip[1]}`, "stroke-width": 5 * scale }, leaves);
    for (let t = 0.1; t < 0.96; t += 0.055) {
      const [px, py] = at(t);
      const [qx, qy] = at(t + 0.02);
      const dir = Math.atan2(qy - py, qx - px);
      const reach = 52 * scale * (1 - t * 0.7);
      for (const side of [-1, 1]) {
        const a = dir + side * 1.15;
        const ex = px + Math.cos(a) * reach;
        const ey = py + Math.sin(a) * reach + reach * 0.55;
        el("path", { d: `M${px} ${py} Q${(px + ex) / 2} ${(py + ey) / 2 - 6} ${ex} ${ey}`, "stroke-width": 4.5 * scale }, leaves);
      }
    }
  }
}

export function drawScene(host) {
  const rnd = random(7);
  const svg = el("svg", { viewBox: `0 0 ${W} ${H}`, preserveAspectRatio: "xMidYMid slice" });
  defs(svg);
  sky(svg, rnd);
  water(svg, city(svg, rnd));
  palm(svg, rnd, { x: 120, top: 150, lean: 130, scale: 1.25, flip: false });
  palm(svg, rnd, { x: 40, top: 260, lean: 40, scale: 0.9, flip: false });
  palm(svg, rnd, { x: 1560, top: 190, lean: 120, scale: 1.1, flip: true });
  host.replaceChildren(svg);
}
