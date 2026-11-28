// Moving through the page with the keyboard, a controller or the mouse.
// Two zones: the sidebar, and the items of the view it opened. Items carry
// `data-nav`; the focused one has the class `focus`.

const REPEAT_MS = 160;

export function createNav({ sidebar, mainView, onSideMove, onActivate, onTab }) {
  let zone = "side";
  const index = { side: 0, main: 0 };
  let capture = null;

  const items = (z) => {
    const root = z === "side" ? sidebar : mainView();
    // Hidden items (a button with nothing to do yet) are skipped.
    return root ? [...root.querySelectorAll("[data-nav]")].filter((el) => !el.hidden) : [];
  };

  function focus(z, i) {
    const all = items(z);
    if (!all.length) return false;
    zone = z;
    index[z] = (i + all.length) % all.length;
    for (const el of document.querySelectorAll("[data-nav].focus")) el.classList.remove("focus");
    all[index[z]].classList.add("focus");
    all[index[z]].scrollIntoView({ block: "nearest" });
    all[index[z]].dispatchEvent(new CustomEvent("navfocus", { bubbles: true }));
    return true;
  }

  const current = () => items(zone)[index[zone]];

  function press(action) {
    const el = current();
    if (zone === "side") {
      switch (action) {
        case "up":
        case "down":
          focus("side", index.side + (action === "up" ? -1 : 1));
          onSideMove(current());
          break;
        case "right":
        case "confirm":
          if (el?.dataset.action) onActivate(el);
          else focus("main", 0);
          break;
      }
      return;
    }
    switch (action) {
      case "up": focus("main", index.main - 1); break;
      case "down": focus("main", index.main + 1); break;
      case "left":
      case "right":
        el?.dispatchEvent(new CustomEvent("cycle", { detail: action === "left" ? -1 : 1 }));
        break;
      case "confirm": el?.click(); break;
      case "back": focus("side", index.side); break;
      case "prev-tab": onTab(-1); break;
      case "next-tab": onTab(1); break;
    }
  }

  const keys = {
    ArrowUp: "up", ArrowDown: "down", ArrowLeft: "left", ArrowRight: "right",
    Enter: "confirm", " ": "confirm", Escape: "back", Backspace: "back",
    q: "prev-tab", Q: "prev-tab", e: "next-tab", E: "next-tab", PageUp: "prev-tab", PageDown: "next-tab",
  };

  document.addEventListener("keydown", (e) => {
    if (capture) {
      e.preventDefault();
      capture(e);
      return;
    }
    const action = keys[e.key];
    if (!action) return;
    e.preventDefault();
    press(action);
  });

  document.addEventListener("mousemove", (e) => {
    const el = e.target.closest?.("[data-nav]");
    if (!el || el.classList.contains("focus")) return;
    for (const z of ["side", "main"]) {
      const at = items(z).indexOf(el);
      if (at >= 0) focus(z, at);
    }
  });

  pollGamepads((action) => {
    if (capture) {
      if (action === "back") capture(null);
      return;
    }
    press(action);
  });

  return {
    side(at) { focus("side", at); },
    main(at = 0) { if (!focus("main", at)) focus("side", index.side); },
    get zone() { return zone; },
    /** Sends every key to `handler` until it is called again with null. */
    capture(handler) { capture = handler; },
  };
}

// Standard mapping: 0 A, 1 B, 4 LB, 5 RB, 12-15 the D-pad; the left stick too.
function pollGamepads(press) {
  const buttons = { 0: "confirm", 1: "back", 4: "prev-tab", 5: "next-tab", 12: "up", 13: "down", 14: "left", 15: "right" };
  const moves = new Set(["up", "down", "left", "right"]);
  const held = new Map();
  function tick(now) {
    for (const pad of navigator.getGamepads?.() ?? []) {
      if (!pad) continue;
      const down = new Set();
      for (const [b, action] of Object.entries(buttons)) {
        if (pad.buttons[b]?.pressed) down.add(action);
      }
      const [x = 0, y = 0] = pad.axes;
      if (y < -0.6) down.add("up");
      if (y > 0.6) down.add("down");
      if (x < -0.6) down.add("left");
      if (x > 0.6) down.add("right");
      for (const id of [...held.keys()]) {
        const [p, action] = id.split(":");
        if (Number(p) === pad.index && !down.has(action)) held.delete(id);
      }
      for (const action of down) {
        const id = `${pad.index}:${action}`;
        const since = held.get(id);
        if (since === undefined) {
          held.set(id, now);
          press(action);
        } else if (moves.has(action) && now - since > REPEAT_MS * 2) {
          held.set(id, now - REPEAT_MS);
          press(action);
        }
      }
    }
    requestAnimationFrame(tick);
  }
  requestAnimationFrame(tick);
}
