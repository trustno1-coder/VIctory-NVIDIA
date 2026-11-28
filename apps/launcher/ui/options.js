// The settings the F1 menu offers, grouped in tabs, each with its help line,
// and the launcher's own (the Audio tab), which are kept apart from the game's.

export const TABS = ["Graphics", "Frame generation", "Performance", "Audio"];
/** The tab of the launcher's own settings. */
export const LAUNCHER_TAB = 3;

const VOLUMES = Array.from({ length: 11 }, (_, i) => [String(i * 10), `${i * 10} %`]);

export const OPTIONS = [
  {
    key: "console", tab: 0, label: "Console profile",
    choices: [["series-x", "Series X"], ["series-s", "Series S"]],
    help: "Series X renders at full size. Series S renders 1280×720 inside: faster, softer.",
  },
  {
    key: "res", tab: 0, label: "Resolution",
    choices: [["1440", "2560×1440"], ["1080to1440", "1080p → 1440p"], ["4k", "3840×2160"]],
    help: "The output size with the Series X profile. The Series S profile keeps its own.",
  },
  {
    key: "dlss", tab: 0, label: "DLSS",
    choices: [["off", "Off"], ["dlaa", "DLAA"], ["quality", "Quality"], ["balanced", "Balanced"],
      ["performance", "Performance"]],
    help: "NVIDIA's upscaling in place of the title's own. Needs the DLSS files (Setup).",
  },
  {
    key: "framegen", tab: 1, label: "Frame generation", number: true,
    choices: [["0", "Off"], ["2", "2×"], ["3", "3×"], ["4", "4×"], ["5", "5×"], ["6", "6×"]],
    help: "Frames shown for each one rendered. More than 2× needs an RTX 50; RTX 40 or later for any.",
  },
  {
    key: "fg_type", tab: 1, label: "Type",
    choices: [["auto", "Auto"], ["dlss", "DLSS"], ["fsr", "AMD FSR 3"]],
    help: "Auto uses DLSS frame generation where the card has it, else AMD FSR 3 (2× at most).",
  },
  {
    key: "fps", tab: 2, label: "Frame rate cap", number: true,
    choices: [["30", "30"], ["60", "60"], ["120", "120"], ["1000", "None"]],
    help: "30 is the title's own rate. None starts each frame as soon as the last one is done.",
  },
  {
    key: "music", tab: 3, label: "Music",
    choices: [["on", "On"], ["off", "Off"]],
    help: "The launcher's music. The speaker button at the top does the same.",
  },
  {
    key: "volume", tab: 3, label: "Volume", number: true, stops: true,
    choices: VOLUMES,
    help: "How loud the launcher's music plays. The game's own sound is not changed.",
  },
];

function label(option, value) {
  const found = option.choices.find(([v]) => v === String(value));
  return found ? found[1] : String(value);
}

/** Moves `option` one choice along in `values`: wrapping around, or stopping at
 * the ends for a scale such as the volume. */
export function cycle(option, values, step) {
  const count = option.choices.length;
  const at = Math.max(option.choices.findIndex(([v]) => v === String(values[option.key])), 0);
  const to = option.stops ? Math.min(count - 1, Math.max(0, at + step)) : (at + step + count) % count;
  const next = option.choices[to][0];
  values[option.key] = option.number ? Number(next) : next;
}

export function renderTabs(container, current, onPick) {
  container.replaceChildren(...TABS.map((name, i) => {
    const tab = document.createElement("button");
    tab.className = "tab" + (i === current ? " current" : "");
    tab.textContent = name;
    tab.addEventListener("click", () => onPick(i));
    return tab;
  }));
}

export function renderRows(container, tab, values, onChange) {
  const rows = OPTIONS.filter((o) => o.tab === tab).map((option) => {
    const row = document.createElement("button");
    row.className = "row";
    row.dataset.nav = "";
    row.dataset.help = option.help;
    row.innerHTML = `<span class="row-label"></span>
      <span class="cycler"><span class="arrow left">◀</span><span class="value"></span><span class="arrow right">▶</span></span>`;
    row.querySelector(".row-label").textContent = option.label;
    const value = row.querySelector(".value");
    const show = () => { value.textContent = label(option, values[option.key]); };
    const change = (step) => {
      cycle(option, values, step);
      show();
      onChange();
    };
    row.addEventListener("cycle", (e) => change(e.detail));
    row.addEventListener("click", (e) => change(e.target.closest(".left") ? -1 : 1));
    show();
    return row;
  });
  container.replaceChildren(...rows);
}
