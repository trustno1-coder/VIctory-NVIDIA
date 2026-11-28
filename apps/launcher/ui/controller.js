// The controller panel: which keyboard key presses which control of the
// game's stand-in pad. The names match apps/launcher/src/keymap.rs.

const GROUPS = [
  ["Buttons", [
    ["a", "A", "a"], ["b", "B", "b"], ["x", "X", "x"], ["y", "Y", "y"],
    ["lb", "LB", "wide"], ["rb", "RB", "wide"], ["lt", "LT", "wide"], ["rt", "RT", "wide"],
    ["menu", "☰", ""], ["view", "⧉", ""], ["ls", "LS", "wide"], ["rs", "RS", "wide"],
  ]],
  ["D-pad", [["up", "↑", ""], ["down", "↓", ""], ["left", "←", ""], ["right", "→", ""]]],
  ["Left stick", [["ls_up", "↑", ""], ["ls_down", "↓", ""], ["ls_left", "←", ""], ["ls_right", "→", ""]]],
  ["Right stick", [["rs_up", "↑", ""], ["rs_down", "↓", ""], ["rs_left", "←", ""], ["rs_right", "→", ""]]],
];

const NAMES = {
  a: "A button", b: "B button", x: "X button", y: "Y button", lb: "Left bumper", rb: "Right bumper",
  lt: "Left trigger", rt: "Right trigger", menu: "Menu", view: "View", ls: "Left stick click",
  rs: "Right stick click", up: "Up", down: "Down", left: "Left", right: "Right",
  ls_up: "Forward", ls_down: "Back", ls_left: "Left", ls_right: "Right",
  rs_up: "Look up", rs_down: "Look down", rs_left: "Look left", rs_right: "Look right",
};

const DEFAULTS = {
  a: "Enter", b: "Backspace", x: "KeyX", y: "KeyY", menu: "Escape", view: "Tab",
  up: "ArrowUp", down: "ArrowDown", left: "ArrowLeft", right: "ArrowRight",
  lb: "KeyQ", rb: "KeyE", ls: "KeyF", rs: "KeyH", lt: "KeyZ", rt: "KeyC",
  ls_up: "KeyW", ls_down: "KeyS", ls_left: "KeyA", ls_right: "KeyD",
  rs_up: "KeyI", rs_down: "KeyK", rs_left: "KeyJ", rs_right: "KeyL",
};

const NAMED = {
  Backspace: "Backspace", Tab: "Tab", Enter: "Enter", CapsLock: "Caps Lock", Escape: "Esc", Space: "Space",
  PageUp: "Page Up", PageDown: "Page Down", End: "End", Home: "Home", Insert: "Insert", Delete: "Delete",
  ArrowLeft: "←", ArrowUp: "↑", ArrowRight: "→", ArrowDown: "↓",
  ShiftLeft: "Left Shift", ShiftRight: "Right Shift", ControlLeft: "Left Ctrl", ControlRight: "Right Ctrl",
  AltLeft: "Left Alt", AltRight: "Right Alt", Semicolon: ";", Equal: "=", Comma: ",", Minus: "-",
  Period: ".", Slash: "/", Backquote: "`", BracketLeft: "[", Backslash: "\\", BracketRight: "]", Quote: "'",
  NumpadAdd: "Num +", NumpadSubtract: "Num -", NumpadMultiply: "Num *", NumpadDivide: "Num /",
};

const LISTEN_MS = 6000;

/** The key's name on a US keyboard, or null for a key the game cannot use. */
export function keyName(code) {
  let m;
  if ((m = /^Key([A-Z])$/.exec(code))) return m[1];
  if ((m = /^Digit([0-9])$/.exec(code))) return m[1];
  if ((m = /^Numpad([0-9])$/.exec(code))) return "Num " + m[1];
  return NAMED[code] ?? null;
}

export function createController({ container, nav, say, save }) {
  let map = { ...DEFAULTS };
  let listening = null;

  function stopListening() {
    if (!listening) return;
    clearTimeout(listening.timer);
    listening.row.classList.remove("listening");
    listening = null;
    nav.capture(null);
    draw();
  }

  function assign(control, code) {
    const other = Object.keys(map).find((c) => c !== control && map[c] === code);
    if (other) {
      map[other] = map[control];
      say(`${NAMES[other]} now uses ${keyName(map[other])}.`);
    } else {
      say("Saved. It applies at the next start of the game.");
    }
    map[control] = code;
    save({ ...map });
  }

  function listen(row, control) {
    stopListening();
    row.classList.add("listening");
    row.querySelector(".keycap").textContent = "Press a key…";
    listening = { row, timer: setTimeout(stopListening, LISTEN_MS) };
    nav.capture((e) => {
      if (e && e.code && keyName(e.code)) assign(control, e.code);
      else if (e) say(`${e.code || e.key} cannot be used for a control (F1 and F3 are the game's own).`, true);
      stopListening();
    });
  }

  function row(control, glyph, style) {
    const el = document.createElement("button");
    el.className = "row" + (map[control] !== DEFAULTS[control] ? " changed" : "");
    el.dataset.nav = "";
    el.innerHTML = `<span class="glyph"></span><span class="row-label"></span><span class="keycap"></span>`;
    const g = el.querySelector(".glyph");
    g.textContent = glyph;
    if (style) g.classList.add(style);
    el.querySelector(".row-label").textContent = NAMES[control];
    el.querySelector(".keycap").textContent = keyName(map[control]) ?? map[control];
    el.addEventListener("click", () => listen(el, control));
    return el;
  }

  function draw() {
    const nodes = [];
    for (const [title, controls] of GROUPS) {
      const head = document.createElement("div");
      head.className = "section";
      head.textContent = title;
      nodes.push(head, ...controls.map(([c, glyph, style]) => row(c, glyph, style)));
    }
    const reset = document.createElement("button");
    reset.className = "row";
    reset.dataset.nav = "";
    reset.innerHTML = '<span class="row-label">Reset every key to its default</span><span class="row-action">Reset</span>';
    reset.addEventListener("click", () => {
      map = { ...DEFAULTS };
      save({ ...map });
      say("Every key is back to its default.");
      draw();
    });
    nodes.push(reset);
    const focused = [...container.querySelectorAll("[data-nav]")].findIndex((e) => e.classList.contains("focus"));
    container.replaceChildren(...nodes);
    if (focused >= 0) nav.main(focused);
  }

  return {
    load(saved) {
      map = { ...DEFAULTS, ...saved };
      if (!listening) draw();
    },
    draw,
  };
}
