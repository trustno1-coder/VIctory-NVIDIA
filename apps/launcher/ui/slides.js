// Background pictures from assets/background (1.jpg, 2.jpg, ...), shown one
// after another like a loading screen: each drifts and zooms slowly, then
// fades into the next. Without any, the drawn scene stays.

const HOLD_MS = 8000;
const FADE_MS = 1800;
const MAX = 30;

function load(src) {
  return new Promise((resolve) => {
    const img = new Image();
    img.onload = () => resolve(src);
    img.onerror = () => resolve(null);
    img.src = src;
  });
}

/** Calls `found` with each picture as soon as it loads, in order. */
async function find(found) {
  for (let n = 1; n <= MAX; n++) {
    const src = (await load(`assets/background/${n}.jpg`)) ?? (await load(`assets/background/${n}.png`));
    if (!src) return;
    found(src);
  }
}

// A slow move for one picture: from one corner's zoom towards another's.
function drift(layer) {
  const still = matchMedia("(prefers-reduced-motion: reduce)").matches;
  if (still) return;
  const side = () => (Math.random() < 0.5 ? -1 : 1) * (1 + Math.random() * 2);
  const [x, y] = [side(), side()];
  const zoomIn = Math.random() < 0.6;
  const [from, to] = zoomIn ? [1.06, 1.18] : [1.18, 1.06];
  layer.animate(
    [
      { transform: `scale(${from}) translate(${x}%, ${y}%)` },
      { transform: `scale(${to}) translate(${-x}%, ${-y}%)` },
    ],
    { duration: HOLD_MS + FADE_MS * 2, easing: "linear", fill: "forwards" },
  );
}

export function startSlides(host, onFirst) {
  const pictures = [];
  let layers = null;
  let turn = 0;
  const next = () => {
    if (pictures.length < 2 && turn > 0) return;
    const [shown, hidden] = [layers[turn % 2], layers[(turn + 1) % 2]];
    hidden.style.backgroundImage = `url("${pictures[turn % pictures.length]}")`;
    hidden.getAnimations().forEach((a) => a.cancel());
    drift(hidden);
    hidden.classList.add("visible");
    shown.classList.remove("visible");
    if (turn === 0) hidden.addEventListener("transitionend", () => onFirst?.(), { once: true });
    turn += 1;
  };
  find((src) => {
    pictures.push(src);
    if (layers) return;
    layers = [0, 1].map(() => {
      const layer = document.createElement("div");
      layer.className = "slide";
      host.appendChild(layer);
      return layer;
    });
    next();
    setInterval(next, HOLD_MS);
  });
}
