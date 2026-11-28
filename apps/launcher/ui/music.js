// The music in assets/song: starts at one minute in, fades in and loops.
// Whether it plays and how loud are the launcher's own settings (Settings,
// Audio tab, and the speaker button), remembered between starts.

const START_AT_S = 60;
const FADE_MS = 2500;
const PREFS_KEY = "launcher-music";

function loadPrefs() {
  try {
    const saved = JSON.parse(localStorage.getItem(PREFS_KEY) ?? "{}");
    return {
      music: saved.music === "off" ? "off" : "on",
      volume: Number.isInteger(saved.volume) ? Math.min(100, Math.max(0, saved.volume)) : 50,
    };
  } catch {
    return { music: "on", volume: 50 };
  }
}

export function createMusic(button) {
  const prefs = loadPrefs();
  let audio = null;
  let fade = 0;

  const level = () => prefs.volume / 100;

  function fadeTo(target, ms) {
    const from = audio.volume;
    const began = performance.now();
    const id = ++fade;
    const step = (now) => {
      if (id !== fade) return;
      const t = ms ? Math.min(1, (now - began) / ms) : 1;
      audio.volume = from + (target - from) * t;
      if (t < 1) requestAnimationFrame(step);
    };
    requestAnimationFrame(step);
  }

  function play() {
    if (!audio || prefs.music !== "on") return;
    audio.play().then(() => fadeTo(level(), FADE_MS)).catch(() => {
      // Refused before any click (a stricter browser): start at the first one.
      document.addEventListener("pointerdown", play, { once: true });
      document.addEventListener("keydown", play, { once: true });
    });
  }

  function showButton() {
    const off = prefs.music !== "on";
    button.classList.toggle("muted", off);
    button.setAttribute("aria-label", off ? "Play the music" : "Mute the music");
  }

  /** Applies `prefs` after a change: saves them, starts, stops or changes the level. */
  function apply() {
    localStorage.setItem(PREFS_KEY, JSON.stringify(prefs));
    showButton();
    if (!audio) return;
    if (prefs.music !== "on") {
      fade += 1;
      audio.pause();
    } else if (audio.paused) {
      audio.volume = 0;
      play();
    } else {
      fadeTo(level(), 200);
    }
  }

  button.addEventListener("click", () => {
    prefs.music = prefs.music === "on" ? "off" : "on";
    apply();
    button.dispatchEvent(new CustomEvent("musicchange", { bubbles: true }));
  });

  return {
    prefs,
    apply,
    /** Loads the first song and plays it if the music is on. */
    async start(songs) {
      if (!songs.length) return;
      // Read whole into memory: a file in memory can start at any second.
      const file = await fetch(encodeURI(songs[0])).then((r) => (r.ok ? r.blob() : null)).catch(() => null);
      if (!file) return;
      audio = new Audio(URL.createObjectURL(file));
      audio.loop = true;
      audio.volume = 0;
      audio.addEventListener("loadedmetadata", () => {
        if (audio.duration > START_AT_S) audio.currentTime = START_AT_S;
        play();
      }, { once: true });
      button.hidden = false;
      showButton();
    },
  };
}
