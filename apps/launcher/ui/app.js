import { createController } from "./controller.js";
import { createNav } from "./nav.js";
import { LAUNCHER_TAB, TABS, renderRows, renderTabs } from "./options.js";
import { drawScene } from "./scene.js";
import { startSlides } from "./slides.js";
import { createMusic } from "./music.js";

const tauri = window.__TAURI__;
const invoke = tauri
  ? tauri.core.invoke
  : async () => { throw "Open this page from the launcher to use it."; };
const $ = (id) => document.getElementById(id);

// The play script's defaults, until the saved choices are read.
const DEFAULTS = { console: "series-x", res: "1440", dlss: "quality", framegen: 0, fg_type: "auto", fps: 1000 };
const state = { status: null, values: { ...DEFAULTS }, loaded: false, tab: 0, view: "home" };

function say(text, isError = false) {
  $("message").textContent = text;
  $("message").classList.toggle("error", isError);
}

async function run(action, done) {
  try {
    await action();
    if (done !== undefined) say(done);
  } catch (e) {
    say(String(e), true);
  }
}

const nav = createNav({
  sidebar: $("sidebar"),
  mainView: () => $(state.view),
  onSideMove: (el) => { if (el.dataset.view) show(el.dataset.view); },
  onActivate: (el) => actions[el.dataset.action](),
  onTab: (step) => {
    if (state.view !== "settings") return;
    state.tab = (state.tab + step + TABS.length) % TABS.length;
    drawSettings();
    nav.main(0);
  },
});

const music = createMusic($("music"));
// The speaker button changes the Audio tab's values: show them again if it is open.
$("music").addEventListener("musicchange", () => {
  if (state.view === "settings" && state.tab === LAUNCHER_TAB) drawSettings();
});

const controller = createController({
  container: $("bindings"),
  nav,
  say,
  save: (keymap) => run(() => invoke("save_keymap", { keymap })),
});

function show(view) {
  state.view = view;
  for (const v of document.querySelectorAll(".view")) v.classList.toggle("active", v.id === view);
  for (const item of document.querySelectorAll(".side-item")) {
    item.classList.toggle("current", item.dataset.view === view);
  }
  if (view === "settings") drawSettings();
}

function drawSettings() {
  renderTabs($("tabs"), state.tab, (i) => { state.tab = i; drawSettings(); nav.main(0); });
  if (state.tab === LAUNCHER_TAB) {
    renderRows($("rows"), state.tab, music.prefs, () => music.apply());
    return;
  }
  renderRows($("rows"), state.tab, state.values, () =>
    run(() => invoke("save_options", { options: state.values }), "Saved. The F1 menu in the game shows the same."));
}

$("settings").addEventListener("navfocus", (e) => {
  if (e.target.dataset.help) $("help").textContent = e.target.dataset.help;
});

const CHECKS = {
  exe: (s) => s.exe_problem ? ["bad", s.exe_problem] : ["ok", "Dump found"],
  built: (s) => s.built
    ? ["ok", "Layer built"]
    : ["warn", "Layer not built yet: the first Play builds it (a few minutes)"],
  dlss: (s) => s.dlss_files ? ["ok", "NVIDIA DLSS files"] : ["warn", "NVIDIA DLSS files missing"],
  fsr: (s) => s.fsr_files ? ["ok", "AMD FSR 3 files"] : ["warn", "AMD FSR 3 files (optional)"],
};

function mark(el, level) {
  el.classList.remove("ok", "warn", "bad");
  el.classList.add(level);
}

function drawStatus(status) {
  state.status = status;
  if (!state.loaded) {
    state.values = { ...status.options };
    state.loaded = true;
    if (state.view === "settings") drawSettings();
  }
  controller.load(status.keymap);
  $("exe").textContent = status.exe ?? "Not chosen";
  for (const [name, check] of Object.entries(CHECKS)) {
    const [level, text] = check(status);
    const row = document.querySelector(`[data-check="${name}"]`);
    mark(row, level);
    row.querySelector(".row-label").textContent = text;
  }
  const problem = status.exe_problem;
  mark($("ready"), problem ? "bad" : "ok");
  $("ready").querySelector(".text").textContent = problem ? "Choose your dump in Setup" : "Ready to play";
  mark($("dlss-chip"), status.dlss_files ? "ok" : "warn");
  $("dlss-chip").textContent = status.dlss_files ? "DLSS ready" : "No DLSS files";
  $("version").textContent = "v" + status.version;
  $("about-version").textContent = "v" + status.version;
  document.querySelector(".play").classList.toggle("disabled", Boolean(problem));
}

async function refresh() {
  try {
    drawStatus(await invoke("status"));
  } catch (e) {
    say(String(e), true);
  }
}

let update = null;

function drawUpdate(info, error) {
  update = info;
  const row = $("update-state");
  const label = row.querySelector(".row-label");
  const behind = info?.behind ?? 0;
  if (error) {
    mark(row, "bad");
    label.textContent = error;
  } else if (!behind) {
    mark(row, "ok");
    label.textContent = "Up to date";
  } else {
    mark(row, info.blocked ? "warn" : "ok");
    label.textContent = `${behind} update${behind > 1 ? "s" : ""} available from the official repository`;
  }
  $("update-commits").replaceChildren(...(info?.commits ?? []).map((c) => {
    const li = document.createElement("li");
    li.textContent = c;
    return li;
  }));
  $("apply-update").hidden = !behind || Boolean(info?.blocked);
  $("update-help").textContent = info?.blocked
    ?? "The launcher closes, the new code is pulled and built in a console window, and the launcher starts again.";
  $("update-badge").hidden = !behind;
  $("update-badge").textContent = behind > 99 ? "99+" : String(behind);
}

async function checkUpdates() {
  const label = $("update-state").querySelector(".row-label");
  label.textContent = "Checking the official repository…";
  try {
    drawUpdate(await invoke("check_updates"));
  } catch (e) {
    drawUpdate(null, String(e));
  }
}

const actions = {
  "check-updates": () => checkUpdates(),
  "apply-update": () => {
    if (!update) return;
    run(() => invoke("apply_update", { remote: update.remote }), "Updating in the new window…");
  },
  play: () => {
    if (state.status?.exe_problem) {
      say(state.status.exe_problem, true);
      show("setup");
      nav.side([...document.querySelectorAll(".side-item")].findIndex((i) => i.dataset.view === "setup"));
      return;
    }
    run(() => invoke("play", { options: state.values }),
      "Starting. The game's console window shows the progress and the log's path.");
  },
  logs: () => run(() => invoke("open_logs"), ""),
  pick: () => run(async () => drawStatus(await invoke("pick_exe")), ""),
};

for (const el of document.querySelectorAll("[data-view]")) {
  el.addEventListener("click", () => {
    show(el.dataset.view);
    nav.main(0);
  });
}
for (const el of document.querySelectorAll("[data-action]")) {
  el.addEventListener("click", () => actions[el.dataset.action]());
}
for (const el of document.querySelectorAll("[data-link]")) {
  el.addEventListener("click", () => run(() => invoke("open_link", { link: el.dataset.link })));
}
for (const el of document.querySelectorAll("[data-fetch]")) {
  el.addEventListener("click", () => {
    if (el.classList.contains("ok")) return;
    run(() => invoke("fetch", { sdk: el.dataset.fetch }),
      "Downloading in the new window. Come back here when it is done.");
  });
}

// The window has no system title bar: the page's moves it and has its buttons.
$("titlebar").addEventListener("mousedown", (e) => {
  if (e.button === 0 && !e.target.closest("button")) invoke("drag").catch(() => {});
});
$("minimize").addEventListener("click", () => run(() => invoke("minimize")));
$("close").addEventListener("click", () => run(() => invoke("quit")));

// The images are not part of the repository; drawn stand-ins show without them.
for (const [img, standIn] of [["badge", null], ["wordmark", "wordmark-text"]]) {
  const shown = () => {
    $(img).hidden = false;
    if (standIn) $(standIn).hidden = true;
  };
  $(img).addEventListener("load", shown);
  if ($(img).complete && $(img).naturalWidth) shown();
}

drawScene($("scene"));
// Once the first picture has faded in, the drawn scene under it is no longer drawn.
startSlides($("slides"), () => { $("scene").hidden = true; });
controller.draw();
window.addEventListener("focus", refresh);
show("home");
nav.side(0);
refresh();
checkUpdates();
invoke("songs").then((songs) => music.start(songs)).catch(() => {});
