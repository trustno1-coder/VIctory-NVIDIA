#!/usr/bin/env python3
"""Summarize one VIctory run (frametime.csv + victory.log / play-*.log) as JSON + markdown.

Usage:
  python scripts/perf/summarize_run.py <run dir | victory.log | play-*.log | frametime.csv> [more ...]
         [--out DIR] [--label TEXT] [--stall-ms 200] [--heavy-draws 1000] [--warmup-s 30]
  python scripts/perf/summarize_run.py --compare A B [--stall-ms 200]    (A, B: run dirs or *.summary.json)
  python scripts/perf/summarize_run.py --latest [N]                      (the N newest runs of %LOCALAPPDATA%\\victory\\runs)

Only the standard library is used. Nothing here is estimated unless it says so: every figure is either read
from a file, or computed from file values with the definition printed in the output's "definitions" list.

Inputs and what they hold
  frametime.csv  one row per ~1 s window: frames, mean ms/frame, MAX ms/frame, CPU prepare ms, GPU ms/pct,
                 draws, MB copied/sent/fetched, RAM, CPU %. Header comments name CPU, GPU, driver, switches.
  victory.log    a frame line (~every 1-5 s: fps, ms/frame, preparing/GPU/copy-back ms, draws), per-frame
                 subsystem lines (submission preparing, descriptor sync, relay, card clock, frame activity),
                 `stall:` watchdog lines (with where the threads stood), shader cache hits/misses,
                 passswitch/gfxswitch lines (ablations switched in the F1 menu), launcher lines.
  play-*.log     the same log, first line is `# console=... build=... switches: ...`; no frametime.csv: the
                 time series is rebuilt from the frame lines (no per-window max, so no tail figures).
"""
import argparse, csv, glob, json, math, os, re, sys
from datetime import datetime, timezone

ANSI = re.compile(r'\x1b\[[0-9;]*m')
TS = re.compile(r'^(\d{4}-\d\d-\d\dT[\d:.]+)Z')
NUM = r'(-?[\d.]+)'


def iso_ms(s):
    return datetime.fromisoformat(s).replace(tzinfo=timezone.utc).timestamp() * 1000.0


def wquantile(pairs, q):
    """Weighted quantile of [(value, weight)]."""
    pairs = sorted(pairs)
    tot = sum(w for _, w in pairs)
    if tot <= 0:
        return None
    acc = 0.0
    for v, w in pairs:
        acc += w
        if acc >= q * tot:
            return v
    return pairs[-1][0]


def mean(xs):
    xs = [x for x in xs if x is not None]
    return sum(xs) / len(xs) if xs else None


def r(x, n=2):
    return None if x is None else round(x, n)


# ----------------------------------------------------------------------------------------------- inputs
def find_inputs(path):
    """-> (csv_path|None, log_path|None, name)."""
    if os.path.isdir(path):
        c = os.path.join(path, 'frametime.csv')
        l = os.path.join(path, 'victory.log')
        return (c if os.path.exists(c) else None, l if os.path.exists(l) else None, os.path.basename(path.rstrip('\\/')))
    if path.lower().endswith('.csv'):
        d = os.path.dirname(path)
        l = os.path.join(d, 'victory.log')
        return path, (l if os.path.exists(l) else None), (os.path.basename(d) if os.path.basename(d) != 'cache' else 'cache-frametime')
    return None, path, os.path.basename(path)


def read_csv(path):
    meta, rows = {}, []
    with open(path, encoding='utf-8', errors='replace') as f:
        lines = f.read().splitlines()
    body = []
    for ln in lines:
        if ln.startswith('#'):
            m = re.match(r'#\s*([\w ]+):\s*(.*)', ln)
            if m:
                meta[m.group(1).strip()] = m.group(2).strip()
        elif ln.strip():
            body.append(ln)
    rd = csv.DictReader(body)
    for d in rd:
        try:
            rows.append({k: float(v) for k, v in d.items() if v not in (None, '')})
        except ValueError:
            continue
    return meta, rows


FRAME_RE = re.compile(
    NUM + r' fps, ' + NUM + r' ms/frame \(' + NUM + r' preparing: copies ' + NUM + r' ms ' + NUM + r' MB, textures ' + NUM +
    r' ms ' + NUM + r' MB; ' + NUM + r' on the GPU; ' + NUM + r' copying back ' + NUM + r' MB\), ' + NUM +
    r' draws/frame, dropped ' + NUM + r' without a host pipeline \(' + NUM + r' being made\) and ' + NUM + r' indirect')

# name -> (regex, group names) ; every value is a per-frame figure of the window the line closes
SUBSYS = {
    'prep_split': (r'submission preparing per frame \(ms\): before the steps ' + NUM + r", the lists' steps " + NUM +
                   r', flush ' + NUM + r', snapshots ' + NUM + r', card copies ' + NUM,
                   ['before_steps_ms', 'lists_steps_ms', 'flush_ms', 'snapshots_ms', 'card_copies_ms']),
    'prep_sub': (r'before the steps split: sync_heaps ' + NUM + r', sweep ' + NUM + r', make_waiting ' + NUM +
                 r', asking the write-watch ' + NUM, ['sync_heaps_ms', 'sweep_ms', 'make_waiting_ms', 'write_watch_ms']),
    'activity': (r'frame activity ms/frame .*?: submission lock ' + NUM + r', watch/dependencies ' + NUM +
                 r', descriptor sync ' + NUM + r', pending views ' + NUM + r', GPU handoff ' + NUM + r', GPU settle ' + NUM +
                 r', relay ' + NUM + r', present ' + NUM + r', buffer views ' + NUM + r', texture views ' + NUM,
                 ['sub_lock_ms', 'watch_deps_ms', 'desc_sync_ms', 'pending_views_ms', 'gpu_handoff_ms', 'gpu_settle_ms',
                  'relay_ms', 'present_ms', 'buffer_views_ms', 'texture_views_ms']),
    'desc_sync': (r'descriptor sync per frame: ' + NUM + r' ms collecting pages, ' + NUM + r' ms scanning, ' + NUM +
                  r' ms mirroring', ['collect_ms', 'scan_ms', 'mirror_ms']),
    'desc_slots': (r'descriptor records the title copied into its heap as bytes: per frame ' + NUM +
                   r' changed heap pages, ' + NUM + r' slots looked at, ' + NUM + r' whose record was new and was mirrored',
                   ['heap_pages', 'slots_looked_at', 'slots_mirrored']),
    'desc_calls': (r'descriptors ' + NUM + r' ms \(' + NUM + r' calls.*?heap_buffers ' + NUM + r' ms',
                   ['descriptors_pass_ms', 'descriptors_pass_calls', 'heap_buffers_pass_ms']),
    'card_clock': (r"the card's clock per frame: nothing of ours queued " + NUM + r' ms \(the card about ' + NUM + r'% busy',
                   ['card_idle_ms', 'card_busy_pct_by_count']),
    'relay': (r'the relay per frame .*?: ' + NUM + r' flights landed in ' + NUM + r' landings.*?waited ' + NUM +
              r' ms for the card with nothing to land and ' + NUM + r' ms for fetches.*?and was ' + NUM + r' ms landing',
              ['flights', 'landings', 'relay_wait_card_ms', 'relay_wait_fetch_ms', 'relay_landing_ms']),
    'sent_ahead': (r'submissions sent ahead per frame: ' + NUM + r' in ' + NUM + r' settles .*?' + NUM +
                   r' ms waiting for the card', ['submissions_sent_ahead', 'settles', 'settle_wait_ms']),
    'buf_refresh': (r'buffer refresh per frame: ' + NUM + r' MB requested in ' + NUM + r' requests, ' + NUM +
                    r' MB after merging in ' + NUM + r' spans', ['refresh_req_mb', 'refresh_requests', 'refresh_merged_mb', 'refresh_spans']),
    'card_copies': (r'host copies in the card\'s memory: .*?per frame: ' + NUM + r' MB sent up in ' + NUM +
                    r' ranges, ' + NUM + r' pieces compared on the card, ' + NUM + r' of them fetched \(' + NUM + r' MB\)',
                    ['card_sent_up_mb', 'card_ranges', 'card_pieces_compared', 'card_pieces_fetched', 'card_fetched_mb']),
    'gpuspan': (r"the card's own timestamps per frame, over " + NUM + r' frames: inside sends: title lists ' + NUM +
                r' ms in ' + NUM + r' sends.*?between sends with nothing to run ' + NUM + r' ms \(' + NUM +
                r' gaps, the longest ' + NUM + r' ms\); first send to last ' + NUM,
                ['span_frames', 'gpu_title_lists_ms', 'sends', 'gpu_gap_ms', 'gaps', 'longest_gap_ms', 'first_to_last_ms']),
    'dstorage': (r'dstorage: last ' + NUM + r' s requests=' + NUM + r' mb_per_s=' + NUM, ['window_s', 'requests', 'mb_per_s']),
}
SUBSYS_C = {k: (re.compile(v[0]), v[1]) for k, v in SUBSYS.items()}
STALL_RE = re.compile(r'stall: the frame has not advanced; inside: (.*?) frame=(\d+) stalled_ms=(\d+) presented=(\d+) '
                      r'submissions_started=(\d+) submissions_finished=(\d+)')
ACT_COUNTERS = re.compile(r'presented=(\d+) submissions_started=(\d+) submissions_finished=(\d+)')
CACHE_RE = re.compile(r'shaders from the cache and translated so far hits=(\d+) misses=(\d+)')
TRANS_RE = re.compile(r'shader translated stage="(\w+)".*? ms=' + NUM)
PASS_RE = re.compile(r'(passswitch|gfxswitch): (.*)')
PASSOFF_RE = re.compile(r'emulator passes off(?: \([^)]*\))?: ([^;]*?)(?:;|$)')
GFXON_RE = re.compile(r'graphics methods on(?: \([^)]*\))?: ([^;]*?)(?:;|$)')
PIPE_MAKING_RE = re.compile(r'frame=(\d+) stage')


def read_log(path):
    with open(path, encoding='utf-8', errors='replace') as f:
        txt = ANSI.sub('', f.read())
    L = {'header': None, 'launcher': [], 'frames': [], 'sub': {k: [] for k in SUBSYS}, 'stalls': [], 'counters': [],
         'cache': None, 'translations': 0, 'translate_ms': 0.0, 'passes': [], 'console': None, 'first_ts': None,
         'last_ts': None, 'passes_off_last': None, 'gfx_on_last': None, 'warn': 0, 'errors': 0, 'flags': {}}
    for i, ln in enumerate(txt.split('\n')):
        if i == 0 and ln.startswith('# console='):
            L['header'] = ln
        ln = ln.lstrip('﻿')
        if ln.startswith('[launcher]'):
            L['launcher'].append(ln[10:].strip())
            continue
        if ln.startswith('console '):
            L['console'] = ln.split(None, 1)[1].strip()
        m = TS.match(ln)
        if not m:
            continue
        t = iso_ms(m.group(1))
        if L['first_ts'] is None:
            L['first_ts'] = t
        L['last_ts'] = t
        if ' WARN ' in ln:
            L['warn'] += 1
        elif ' ERROR ' in ln:
            L['errors'] += 1
        if 'shader translated' in ln:
            L['translations'] += 1
            mm = TRANS_RE.search(ln)
            if mm:
                L['translate_ms'] += float(mm.group(2))
            continue
        if ' fps, ' in ln and 'ms/frame (' in ln:
            mm = FRAME_RE.search(ln)
            if mm:
                v = [float(x) for x in mm.groups()]
                L['frames'].append({'t': t, 'fps': v[0], 'ms': v[1], 'prep_ms': v[2], 'copies_ms': v[3], 'copies_mb': v[4],
                                    'tex_ms': v[5], 'tex_mb': v[6], 'gpu_ms': v[7], 'copyback_ms': v[8], 'copyback_mb': v[9],
                                    'draws': v[10], 'dropped': v[11], 'making': v[12], 'indirect': v[13]})
            continue
        if 'stall:' in ln:
            mm = STALL_RE.search(ln)
            if mm:
                L['stalls'].append({'t': t, 'inside': mm.group(1), 'frame': int(mm.group(2)), 'stalled_ms': int(mm.group(3)),
                                    'presented': int(mm.group(4))})
            continue
        if 'frame activity' in ln:
            mm = ACT_COUNTERS.search(ln)
            if mm:
                L['counters'].append({'t': t, 'presented': int(mm.group(1)), 'started': int(mm.group(2)), 'finished': int(mm.group(3))})
        mm = CACHE_RE.search(ln)
        if mm:
            L['cache'] = (int(mm.group(1)), int(mm.group(2)))
        mm = PASSOFF_RE.search(ln)
        if mm:
            L['passes_off_last'] = mm.group(1).strip()
        mm = GFXON_RE.search(ln)
        if mm:
            L['gfx_on_last'] = mm.group(1).strip()
        mm = PASS_RE.search(ln)
        if mm:
            L['passes'].append({'t': t, 'kind': mm.group(1), 'text': mm.group(2)[:200]})
        for k, (rx, names) in SUBSYS_C.items():
            if k == 'desc_calls' and 'emulator passes off' not in ln:
                continue
            mm = rx.search(ln)
            if mm:
                d = {n: float(x) for n, x in zip(names, mm.groups())}
                d['t'] = t
                L['sub'][k].append(d)
                break
    # a few one-off facts
    for key, pat in (('dlss', r'DLSS Super Resolution is supported'), ('streamline_loaded', r'Streamline loaded'),
                     ('framegen_on', r'frame ?gen\w* (?:is )?(?:on|enabled)'), ('auto_a', r'VICTORY_AUTO_A')):
        L['flags'][key] = bool(re.search(pat, txt, re.I))
    mm = re.search(r'dlss[^\n]*?(mode|quality)=\"?(\w+)', txt, re.I)
    return L


# ----------------------------------------------------------------------------------------------- analysis
def windows_from_csv(rows):
    """One record per ~1 s window; dt from consecutive t_ms. First row has no dt (kept for counts only)."""
    w = []
    prev_t = None
    for x in rows:
        n = x.get('frames', 0.0)
        dt = (x['t_ms'] - prev_t) if prev_t is not None else None
        prev_t = x['t_ms']
        w.append({'t': x['t_ms'], 'dt': dt, 'n': n, 'avg': x.get('ms_per_frame'), 'max': x.get('frame_ms_max'),
                  'draws': x.get('draws', 0.0), 'making': x.get('making', 0.0), 'row': x})
    return w


def windows_from_log(frames):
    w = []
    prev = None
    for f in frames:
        dt = (f['t'] - prev) if prev is not None else None
        prev = f['t']
        n = f['fps'] * dt / 1000.0 if dt else 0.0
        w.append({'t': f['t'], 'dt': dt, 'n': n, 'avg': f['ms'], 'max': None, 'draws': f['draws'], 'making': f['making'], 'row': f})
    return w


def reconstruct(wins):
    """Per window of n frames with mean a and max m: one frame at m, n-1 at (n*a-m)/(n-1).
    Exact in the window's frame count, sum of time and maximum; only a model of the spread in between."""
    out = []
    for x in wins:
        n = int(round(x['n']))
        if n <= 0 or x['avg'] is None:
            continue
        if x['max'] is None or n == 1:
            out.append((x['avg'] if x['max'] is None else x['max'], n if n > 0 else 1) if n > 1 or x['max'] is None else (x['max'], 1))
            continue
        rest = (n * x['avg'] - x['max']) / (n - 1)
        if rest <= 0:
            out.append((x['avg'], n))
        else:
            out.append((x['max'], 1))
            out.append((rest, n - 1))
    return out


def dist(wins, tag):
    """Frame-time statistics of a set of windows."""
    wins = [x for x in wins if x['n'] and x['avg']]
    if not wins:
        return None
    frames = sum(x['n'] for x in wins)
    time_ms = sum(x['n'] * x['avg'] for x in wins)
    plain = [(x['avg'], x['n']) for x in wins]
    rec = reconstruct(wins)
    d = {'windows': len(wins), 'frames': int(round(frames)), 'seconds': r(time_ms / 1000, 1),
         'fps_mean': r(1000 * frames / time_ms) if time_ms else None,
         'ms_mean': r(time_ms / frames),
         'ms_median_windowavg': r(wquantile(plain, 0.5)),
         'ms_p90_windowavg': r(wquantile(plain, 0.9)),
         'ms_p99_windowavg': r(wquantile(plain, 0.99))}
    if all(x['max'] is not None for x in wins):
        d['ms_median_reconstructed'] = r(wquantile(rec, 0.5))
        d['ms_p90_reconstructed'] = r(wquantile(rec, 0.90))
        d['ms_p99_reconstructed'] = r(wquantile(rec, 0.99))
        d['ms_worst_frame'] = r(max(x['max'] for x in wins))
        p99 = d['ms_p99_reconstructed']
        d['fps_1pct_low_p99'] = r(1000 / p99, 1) if p99 else None
        # mean fps of the slowest 1 % of frames (reconstructed)
        tot = sum(w for _, w in rec)
        k, acc, sl = max(1, int(tot * 0.01)), 0, []
        for v, w in sorted(rec, reverse=True):
            take = min(w, k - acc)
            sl += [v] * take
            acc += take
            if acc >= k:
                break
        d['fps_1pct_low_mean_of_slowest'] = r(1000 / mean(sl), 1) if sl else None
        wm = sorted(x['max'] for x in wins)
        d['window_max_ms_p50'] = r(wm[len(wm) // 2])
        d['window_max_ms_p90'] = r(wm[int(len(wm) * 0.9) - 1 if len(wm) > 1 else 0])
    d['tag'] = tag
    return d


def stalls_csv(wins, thr, ref=None, rel=3.0):
    """A stall window = a window whose slowest frame is >= thr ms (CSV keeps only one max per window, so the
    stalled time is a LOWER bound: sum of those maxima)."""
    thr = max(thr, rel * ref) if ref else thr
    if wins and all(x['max'] is None for x in wins):
        return {'threshold_ms': r(thr, 1), 'stall_windows': None, 'stalled_ms_lower_bound': None, 'worst_ms': None,
                'windows_total': len(wins), 'share_of_windows': None, 'over_1000ms_windows': None}
    s = [x for x in wins if x['max'] is not None and x['max'] >= thr]
    return {'threshold_ms': r(thr, 1), 'stall_windows': len(s), 'stalled_ms_lower_bound': r(sum(x['max'] for x in s), 0),
            'worst_ms': r(max((x['max'] for x in s), default=0), 0),
            'windows_total': len(wins),
            'share_of_windows': r(len(s) / len(wins), 3) if wins else None,
            'over_1000ms_windows': sum(1 for x in s if x['max'] >= 1000)}


def phases(wins, heavy_draws, warm_s):
    """Boot -> first heavy scene -> stabilised. World entry = first of 3 consecutive windows with mean draws/frame
    >= heavy_draws. Stabilised = first window after entry+warm_s from which the next 10 windows all have
    window-mean ms within +-25 % of the median of the second half of the heavy windows."""
    if not wins:
        return {}
    t0 = wins[0]['t'] - (wins[1]['dt'] if len(wins) > 1 and wins[1]['dt'] else 1000)
    out = {'t0_epoch_ms': wins[0]['t']}
    entry = None
    for i in range(len(wins) - 2):
        if all(wins[j]['draws'] >= heavy_draws for j in (i, i + 1, i + 2)):
            entry = i
            break
    # first window with any scene (draws >= 100, 3 in a row)
    scene = None
    for i in range(len(wins) - 2):
        if all(wins[j]['draws'] >= 100 for j in (i, i + 1, i + 2)):
            scene = i
            break
    # first window after which no pipelines are being made
    out['first_window_pipelines_being_made'] = wins[0]['making']
    mk = [i for i, x in enumerate(wins) if x['making'] > 0]
    out['seconds_until_pipelines_made_done'] = r((wins[mk[-1]]['t'] - t0) / 1000, 1) if mk else 0.0
    out['seconds_to_scene_draws_ge_100'] = r((wins[scene]['t'] - t0) / 1000, 1) if scene is not None else None
    out['seconds_to_heavy_scene'] = r((wins[entry]['t'] - t0) / 1000, 1) if entry is not None else None
    out['heavy_draws_threshold'] = heavy_draws
    out['stabilised_at_s'] = None
    out['stabilise_rule'] = (f'first window >= {warm_s:.0f}s after heavy-scene entry whose next 10 windows are all within +-25% '
                             'of the median ms/frame of the second half of the heavy windows')
    if entry is not None:
        heavy = [x for x in wins[entry:] if x['draws'] >= heavy_draws and x['avg']]
        if len(heavy) >= 12:
            half = sorted(x['avg'] for x in heavy[len(heavy) // 2:])
            ref = half[len(half) // 2]
            out['ref_ms_second_half'] = r(ref)
            for i in range(entry, len(wins) - 10):
                if wins[i]['t'] - wins[entry]['t'] < warm_s * 1000:
                    continue
                if all(wins[j]['avg'] and abs(wins[j]['avg'] - ref) <= 0.25 * ref for j in range(i, i + 10)):
                    out['stabilised_at_s'] = r((wins[i]['t'] - t0) / 1000, 1)
                    out['stabilised_after_entry_s'] = r((wins[i]['t'] - wins[entry]['t']) / 1000, 1)
                    break
    out['_entry_index'] = entry
    out['_entry_t'] = wins[entry]['t'] if entry is not None else None
    out['_t0'] = t0
    return out


def csv_columns(wins, sel):
    """Frame-weighted means of the CSV columns over the selected windows."""
    sel = [x for x in sel if x['n']]
    if not sel:
        return {}
    tw = sum(x['n'] for x in sel)
    cols = ['cpu_prepare_ms', 'gpu_ms', 'gpu_pct', 'copies_ms', 'copies_mb', 'textures_ms', 'textures_mb', 'copyback_ms',
            'copyback_mb', 'draws', 'dropped', 'indirect', 'heap_passes', 'heap_ms', 'texture_refresh_ms', 'card_passes',
            'card_compares', 'card_sent_mb', 'card_ranges', 'card_pieces', 'card_fetched', 'card_fetched_mb', 'card_waits',
            'card_wait_ms']
    out = {}
    for c in cols:
        vals = [(x['row'].get(c), x['n']) for x in sel if c in x['row']]
        if vals:
            out[c] = r(sum(v * w for v, w in vals) / sum(w for _, w in vals), 3)
    # gpu_pct is 0 when the GPU timestamps are not on; report the share of windows that have it
    g = [x['row'].get('gpu_pct', 0) for x in sel]
    out['_gpu_pct_windows_nonzero'] = sum(1 for v in g if v > 0)
    out['_windows'] = len(sel)
    out['ram_mb_last'] = r(sel[-1]['row'].get('ram_mb'), 0)
    out['ram_mb_max'] = r(max(x['row'].get('ram_mb', 0) for x in sel), 0)
    out['cpu_pct_mean'] = r(mean([x['row'].get('cpu_pct') for x in sel]), 1)
    return out


def sub_means(L, t_from):
    """Mean of each per-frame log line in the heavy phase (simple mean of lines, one per window)."""
    out = {}
    for k, lines in L['sub'].items():
        sel = [d for d in lines if t_from is None or d['t'] >= t_from]
        if not sel:
            continue
        keys = [n for n in sel[0] if n != 't']
        out[k] = {n: r(mean([d[n] for d in sel]), 3) for n in keys}
        out[k]['_lines'] = len(sel)
    return out


def classify_stalls(L):
    """Group the watchdog lines: lines of the same frame number belong to one stall; its length is the largest
    stalled_ms; its cause the `inside:` text (what the threads stood in at the first line)."""
    groups = []
    for s in L['stalls']:
        if groups and groups[-1]['frame'] == s['frame']:
            g = groups[-1]
            g['stalled_ms'] = max(g['stalled_ms'], s['stalled_ms'])
            g['lines'] += 1
            continue
        groups.append(dict(s, lines=1))
    by = {}
    for g in groups:
        key = re.sub(r'\s*\(last entered \d+ ms ago\)', '', g['inside'])
        key = re.sub(r' x\d+', '', key)[:140]
        d = by.setdefault(key, {'count': 0, 'ms': 0, 'worst_ms': 0})
        d['count'] += 1
        d['ms'] += g['stalled_ms']
        d['worst_ms'] = max(d['worst_ms'], g['stalled_ms'])
    return {'watchdog_stalls': len(groups), 'watchdog_ms_total': sum(g['stalled_ms'] for g in groups),
            'watchdog_worst_ms': max((g['stalled_ms'] for g in groups), default=0),
            'by_inside': dict(sorted(by.items(), key=lambda kv: -kv[1]['ms']))}


def label_run(name, meta, L):
    """What the files say about the configuration. Anything not stated is 'unknown'; nothing is inferred."""
    sw = {}
    src = None
    if meta.get('switches'):
        src = meta['switches']
    elif L and L.get('header'):
        src = L['header'].split('switches:', 1)[1] if 'switches:' in L['header'] else ''
    if src:
        for tok in re.findall(r'(VICTORY_[A-Z0-9_]+)=(\S*)', src):
            sw[tok[0]] = tok[1]
    if L:
        for ln in L['launcher']:
            m = re.match(r'(VICTORY_[A-Z0-9_]+)=(.*)', ln)
            if m:
                sw.setdefault(m.group(1), m.group(2))
    lab = {}
    console = None
    if L and L.get('header'):
        m = re.match(r'# console=(\S+) build=(\S+)', L['header'])
        if m:
            console, lab['build'] = m.group(1), m.group(2)
    if L and L.get('console') and not console:
        console = 'series-x' if 'Series X' in L['console'] else ('series-s' if 'Series S' in L['console'] else L['console'])
    if L:
        for ln in L['launcher']:
            m = re.search(r'profile=(\w+)', ln)
            if m:
                lab['build'] = m.group(1)
    lab['console_profile'] = console or 'unknown'
    lab['dlss'] = ('on:' + sw.get('VICTORY_DLSS_MODE', '?')) if sw.get('VICTORY_DLSS') == '1' else (
        'off' if sw or L else 'unknown')
    if sw.get('VICTORY_DLSS') != '1' and L and L['flags'].get('streamline_loaded'):
        lab['dlss'] = 'streamline loaded (mode not in switch list)'
    fx = sw.get('VICTORY_FRAMEGEN_X')
    lab['framegen'] = ('off (ready)' if (sw.get('VICTORY_FRAMEGEN') == '1' and fx in ('0', None)) else
                       f'on x{fx}' if sw.get('VICTORY_FRAMEGEN') == '1' else 'off/unknown')
    lab['resolution_switch'] = ('native-1440p' if sw.get('VICTORY_NATIVE_1440P') == '1' else
                                '1080p-render' if sw.get('VICTORY_RENDER_1080P') == '1' else 'default/unknown')
    lab['panel'] = sw.get('VICTORY_PANEL', 'unset')
    lab['auto_a_switch'] = sw.get('VICTORY_AUTO_A', 'unset')
    lab['relay'] = sw.get('VICTORY_RELAY', 'unset')
    if L:
        lab['shader_cache'] = ({'hits': L['cache'][0], 'misses': L['cache'][1]} if L['cache'] else 'no cache line in log')
        lab['shader_translations_in_log'] = L['translations']
        lab['passes_off_final'] = L['passes_off_last'] or 'unknown'
        lab['gfx_methods_on_final'] = L['gfx_on_last'] or 'unknown'
        lab['ablation_events_in_log'] = len(L['passes'])
    lab['cache_state_hint'] = None
    if L and L['cache']:
        h, m = L['cache']
        lab['cache_state_hint'] = (('shader-translation cache warm (misses=0, hits=%d)' % h) if m == 0 else
                                   ('shader-translation cache COLD (all %d translated this run)' % m) if h == 0 else
                                   ('shader-translation cache partly cold (misses=%d of %d)' % (m, h + m)))
    lab['n_switches_listed'] = len(sw)
    return lab, sw


def log_counters(L, t_from):
    c = [x for x in L['counters'] if t_from is None or x['t'] >= t_from]
    if len(c) < 2:
        return {}
    dp = c[-1]['presented'] - c[0]['presented']
    ds = c[-1]['started'] - c[0]['started']
    return {'submissions_per_presented_frame': r(ds / dp, 2) if dp else None, 'presented_in_span': dp, 'submissions_in_span': ds}


def summarise(path, a):
    csv_path, log_path, name = find_inputs(path)
    meta, rows, L = {}, [], None
    if csv_path:
        meta, rows = read_csv(csv_path)
    if log_path:
        L = read_log(log_path)
    S = {'name': name, 'sources': {'csv': csv_path, 'log': log_path}, 'hardware': {k: meta.get(k) for k in ('cpu', 'gpu', 'driver', 'ram', 'os', 'display', 'version')}}
    if rows:
        wins = windows_from_csv(rows)
        S['time_series'] = 'frametime.csv (1 s windows, exact per-window mean and max)'
    elif L and L['frames']:
        wins = windows_from_log(L['frames'])
        S['time_series'] = 'log frame lines (windows ~1-5 s, no per-window max: tail figures unavailable)'
    else:
        S['error'] = 'no usable frame data'
        return S
    S['label'] = label_run(name, meta, L)[0]
    S['label']['custom'] = a.label
    S['t_start_utc'] = datetime.fromtimestamp(wins[0]['t'] / 1000, timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')
    ph = phases(wins, a.heavy_draws, a.warmup_s)
    entry_i, entry_t = ph.pop('_entry_index'), ph.pop('_entry_t')
    ph.pop('_t0')
    S['phases'] = ph
    body = wins[1:] if len(wins) > 1 else wins  # first window has no dt
    S['session'] = dist(body, 'whole session (boot, menu, load, play)')
    S['session']['duration_s'] = r((wins[-1]['t'] - wins[0]['t']) / 1000, 1)
    heavy = [x for x in wins if x['draws'] >= a.heavy_draws]
    S['heavy_scene'] = dist(heavy, f'windows with mean draws/frame >= {a.heavy_draws}')
    ref_h = (S['heavy_scene'] or {}).get('ms_median_reconstructed', (S['heavy_scene'] or {}).get('ms_median_windowavg'))
    ref_s = S['session'].get('ms_median_reconstructed', S['session']['ms_median_windowavg'])
    S['stalls_csv_windows'] = stalls_csv(body, a.stall_ms, ref_s, a.stall_rel)
    if heavy:
        S['heavy_scene']['stalls'] = stalls_csv(heavy, a.stall_ms, ref_h, a.stall_rel)
    if entry_i is not None:
        after = [x for x in wins[entry_i:] if x['t'] - wins[entry_i]['t'] >= a.warmup_s * 1000]
        S['steady'] = dist(after, f'after heavy-scene entry + {a.warmup_s:.0f} s (all windows)')
        if S['steady']:
            S['steady']['stalls'] = stalls_csv(after, a.stall_ms, ref_h, a.stall_rel)
    if rows:
        S['columns_heavy'] = csv_columns(wins, heavy)
        S['columns_session'] = csv_columns(wins, body)
        # fraction of heavy windows with a GPU number: gpu_ms is only filled when GPU spans are on
    if L:
        S['log'] = {'seconds_logged': r((L['last_ts'] - L['first_ts']) / 1000, 1), 'warn_lines': L['warn'], 'error_lines': L['errors'],
                    'shader_translations': L['translations'], 'shader_translate_ms_total': r(L['translate_ms'], 0),
                    'stall_attribution': classify_stalls(L)}
        t_from = entry_t if entry_t is not None else None
        if L['frames']:
            fl = [f for f in L['frames'] if t_from is None or f['t'] >= t_from]
            if fl:
                S['log']['frame_lines_heavy'] = {k: r(mean([f[k] for f in fl]), 2) for k in
                                                 ('fps', 'ms', 'prep_ms', 'copies_ms', 'copies_mb', 'tex_ms', 'tex_mb', 'gpu_ms',
                                                  'copyback_ms', 'copyback_mb', 'draws', 'dropped')}
                S['log']['frame_lines_heavy']['_lines'] = len(fl)
        S['log']['per_frame_subsystems_heavy'] = sub_means(L, t_from)
        S['log']['counters'] = log_counters(L, t_from)
        S['log']['dstorage_mb_per_s_peak'] = r(max((d['mb_per_s'] for d in L['sub']['dstorage']), default=0), 1)
    S['definitions'] = [
        f'stall window = CSV window whose slowest frame >= max({a.stall_ms:g} ms, {a.stall_rel:g} x the median ms/frame of its span) (threshold shown per span); stalled time is a lower bound (one max per window).',
        'watchdog stall = a `stall:` log line group (same frame number); length = largest stalled_ms; cause = `inside:` text.',
        'ms_*_windowavg: quantiles of the windows\' mean ms/frame weighted by frames (hides tails).',
        'ms_*_reconstructed: per window one frame at the logged max and the rest at the remaining mean; exact in frame count, '
        'total time and window max, a model in between. 1% low = 1000/P99 (fps_1pct_low_p99) and mean fps of the slowest 1% frames.',
        f'heavy scene = windows with mean draws/frame >= {a.heavy_draws}; world entry = 3 consecutive such windows; "stabilised" rule in phases.',
        'per_frame_subsystems_heavy = plain mean over log lines at/after world entry; each line is one window\'s per-frame figure; '
        'subsystem ms are summed over threads (they can exceed the frame time).',
        'gpu_ms/gpu_pct (CSV) are 0 unless GPU timestamp spans are on; see columns_heavy._gpu_pct_windows_nonzero.',
    ]
    return S


# ----------------------------------------------------------------------------------------------- output
def fmt(v):
    return '-' if v is None else (f'{v:g}' if isinstance(v, (int, float)) else str(v))


def md(S):
    o = []
    if 'error' in S:
        return f"# {S['name']}\n\n{S['error']}\n"
    lab = S['label']
    o.append(f"# Run summary: {S['name']}\n")
    o.append(f"- start (UTC): {S['t_start_utc']}   duration: {fmt(S['session']['duration_s'])} s   time series: {S['time_series']}")
    o.append(f"- sources: csv={S['sources']['csv']}  log={S['sources']['log']}")
    hw = S['hardware']
    if any(hw.values()):
        o.append(f"- hardware: {fmt(hw['cpu'])} | {fmt(hw['gpu'])} | driver {fmt(hw['driver'])} | RAM {fmt(hw['ram'])} | {fmt(hw['os'])} | display {fmt(hw['display'])}")
    o.append(f"- profile: {lab['console_profile']}  build: {lab.get('build','unknown')}  DLSS: {lab['dlss']}  frame-gen: {lab['framegen']}  "
             f"resolution switch: {lab['resolution_switch']}  panel: {lab['panel']}  auto-A switch: {lab['auto_a_switch']}")
    o.append(f"- shader cache: {fmt(lab.get('cache_state_hint'))}; passes off at end: {fmt(lab.get('passes_off_final'))}; graphics methods on at end: {fmt(lab.get('gfx_methods_on_final'))}; switch events in log: {fmt(lab.get('ablation_events_in_log'))}")
    o.append('')
    o.append('## Frame time\n')
    o.append('| span | windows | frames | s | fps mean | ms mean | ms median | ms P90 | ms P99 | worst ms | 1% low fps (P99) |')
    o.append('|---|---|---|---|---|---|---|---|---|---|---|')
    for key in ('session', 'heavy_scene', 'steady'):
        d = S.get(key)
        if not d:
            continue
        med = d.get('ms_median_reconstructed', d['ms_median_windowavg'])
        p90 = d.get('ms_p90_reconstructed', d['ms_p90_windowavg'])
        p99 = d.get('ms_p99_reconstructed', d['ms_p99_windowavg'])
        o.append(f"| {d['tag']} | {d['windows']} | {d['frames']} | {fmt(d['seconds'])} | {fmt(d['fps_mean'])} | {fmt(d['ms_mean'])} | "
                 f"{fmt(med)} | {fmt(p90)} | {fmt(p99)} | {fmt(d.get('ms_worst_frame'))} | {fmt(d.get('fps_1pct_low_p99'))} |")
    o.append('')
    o.append('## Phases\n')
    for k, v in S['phases'].items():
        o.append(f'- {k}: {fmt(v)}')
    o.append('')
    st = S['stalls_csv_windows']
    o.append(f"## Stalls (CSV windows with a frame >= {st['threshold_ms']} ms for the session; per span below)\n")
    o.append(f"- whole session: {st['stall_windows']} of {st['windows_total']} windows, >= {fmt(st['stalled_ms_lower_bound'])} ms stalled "
             f"(lower bound), worst {fmt(st['worst_ms'])} ms, {st['over_1000ms_windows']} windows with a frame > 1 s")
    for key in ('heavy_scene', 'steady'):
        d = S.get(key)
        if d and d.get('stalls'):
            s = d['stalls']
            o.append(f"- {key} (threshold {fmt(s['threshold_ms'])} ms): {s['stall_windows']} of {s['windows_total']} windows, >= {fmt(s['stalled_ms_lower_bound'])} ms, worst {fmt(s['worst_ms'])} ms")
    if 'log' in S:
        sa = S['log']['stall_attribution']
        o.append(f"- watchdog (log): {sa['watchdog_stalls']} stalls, {sa['watchdog_ms_total']} ms, worst {sa['watchdog_worst_ms']} ms")
        for k, v in list(sa['by_inside'].items())[:6]:
            o.append(f"    - {v['count']}x {v['ms']} ms (worst {v['worst_ms']}): {k}")
    o.append('')
    if S.get('columns_heavy'):
        o.append('## CSV columns, heavy scene (frame-weighted means per frame)\n')
        o.append('| column | mean |')
        o.append('|---|---|')
        for k, v in S['columns_heavy'].items():
            o.append(f'| {k} | {fmt(v)} |')
        o.append('')
    if 'log' in S:
        lg = S['log']
        o.append('## Log, from world entry on\n')
        o.append(f"- log span {fmt(lg['seconds_logged'])} s, WARN lines {lg['warn_lines']}, ERROR lines {lg['error_lines']}, "
                 f"shader translations {lg['shader_translations']} ({fmt(lg['shader_translate_ms_total'])} ms total), peak DirectStorage {fmt(lg['dstorage_mb_per_s_peak'])} MB/s")
        if lg.get('frame_lines_heavy'):
            o.append(f"- frame lines: {json.dumps(lg['frame_lines_heavy'])}")
        if lg.get('counters'):
            o.append(f"- submissions: {json.dumps(lg['counters'])}")
        o.append('')
        for k, v in lg['per_frame_subsystems_heavy'].items():
            o.append(f'- {k}: {json.dumps(v)}')
        o.append('')
    o.append('## Definitions\n')
    for d in S['definitions']:
        o.append(f'- {d}')
    return '\n'.join(o) + '\n'


HEADLINE = [
    ('heavy_scene.fps_mean', 'heavy: fps mean'), ('heavy_scene.ms_mean', 'heavy: ms mean'),
    ('heavy_scene.ms_median_reconstructed', 'heavy: ms median'), ('heavy_scene.ms_p90_reconstructed', 'heavy: ms P90'),
    ('heavy_scene.ms_p99_reconstructed', 'heavy: ms P99'), ('heavy_scene.fps_1pct_low_p99', 'heavy: 1% low fps'),
    ('heavy_scene.stalls.stall_windows', 'heavy: stall windows'), ('heavy_scene.stalls.stalled_ms_lower_bound', 'heavy: stalled ms (>=)'),
    ('session.fps_mean', 'session: fps mean'), ('session.ms_p99_reconstructed', 'session: ms P99'),
    ('stalls_csv_windows.stall_windows', 'session: stall windows'), ('stalls_csv_windows.stalled_ms_lower_bound', 'session: stalled ms (>=)'),
    ('phases.seconds_to_heavy_scene', 'seconds to heavy scene'), ('phases.stabilised_after_entry_s', 'seconds entry->stable'),
    ('columns_heavy.cpu_prepare_ms', 'CPU prepare ms/frame'), ('columns_heavy.gpu_ms', 'GPU ms/frame (CSV)'),
    ('columns_heavy.draws', 'draws/frame'), ('columns_heavy.copies_mb', 'copies MB/frame'), ('columns_heavy.textures_mb', 'texture MB/frame'),
    ('columns_heavy.copyback_mb', 'copy-back MB/frame'), ('columns_heavy.card_sent_mb', 'card sent MB/frame'),
    ('columns_heavy.ram_mb_max', 'RAM max MB'),
    ('log.counters.submissions_per_presented_frame', 'submissions/presented frame'),
    ('log.per_frame_subsystems_heavy.desc_slots.slots_looked_at', 'descriptor slots looked at/frame'),
    ('log.per_frame_subsystems_heavy.desc_slots.slots_mirrored', 'descriptor slots mirrored/frame'),
    ('log.per_frame_subsystems_heavy.card_clock.card_busy_pct_by_count', 'card busy % (log count)'),
    ('log.per_frame_subsystems_heavy.activity.present_ms', 'present ms/frame (thread sum)'),
    ('log.per_frame_subsystems_heavy.prep_split.before_steps_ms', 'prep: before steps ms'),
    ('log.per_frame_subsystems_heavy.prep_split.card_copies_ms', 'prep: card copies ms'),
    ('log.stall_attribution.watchdog_stalls', 'watchdog stalls'),
]


def dig(d, path):
    for p in path.split('.'):
        if not isinstance(d, dict) or p not in d:
            return None
        d = d[p]
    return d


def compare(A, B, a):
    o = [f"# A/B: {A['name']} (A) vs {B['name']} (B)\n",
         f"- A: {json.dumps({k: A['label'].get(k) for k in ('console_profile','dlss','framegen','resolution_switch','panel')})}",
         f"- B: {json.dumps({k: B['label'].get(k) for k in ('console_profile','dlss','framegen','resolution_switch','panel')})}",
         '- differences under ~5 % are noise (docs/findings.md); scenes differ run to run.\n',
         '| metric | A | B | B-A | % |', '|---|---|---|---|---|']
    for path, name in HEADLINE:
        x, y = dig(A, path), dig(B, path)
        if x is None and y is None:
            continue
        if isinstance(x, (int, float)) and isinstance(y, (int, float)):
            pct = f'{100 * (y - x) / x:+.1f}' if x else '-'
            o.append(f'| {name} | {fmt(x)} | {fmt(y)} | {y - x:+.2f} | {pct} |')
        else:
            o.append(f'| {name} | {fmt(x)} | {fmt(y)} | - | - |')
    return '\n'.join(o) + '\n'


def first(S, *paths):
    for p in paths:
        v = dig(S, p)
        if v is not None:
            return v
    return None


def index_md(Ss):
    cols = [('heavy fps', ('heavy_scene.fps_mean',)),
            ('heavy ms med', ('heavy_scene.ms_median_reconstructed', 'heavy_scene.ms_median_windowavg')),
            ('heavy ms P99', ('heavy_scene.ms_p99_reconstructed', 'heavy_scene.ms_p99_windowavg')),
            ('heavy stall win', ('heavy_scene.stalls.stall_windows',)),
            ('sess fps', ('session.fps_mean',)), ('s to heavy', ('phases.seconds_to_heavy_scene',)),
            ('cpu prep ms', ('columns_heavy.cpu_prepare_ms',)), ('draws/f', ('columns_heavy.draws',)),
            ('watchdog stalls', ('log.stall_attribution.watchdog_stalls',)),
            ('watchdog ms', ('log.stall_attribution.watchdog_ms_total',))]
    o = ['| run | profile | DLSS | framegen | passes off at end | dur s | ' + ' | '.join(c[0] for c in cols) + ' |',
         '|' + '---|' * (6 + len(cols))]
    vals = [[] for _ in cols]
    for S in Ss:
        if 'error' in S:
            o.append(f"| {S['name']} | error |")
            continue
        l = S['label']
        row = []
        for i, (_, ps) in enumerate(cols):
            v = first(S, *ps)
            row.append(fmt(v))
            if isinstance(v, (int, float)):
                vals[i].append(v)
        o.append(f"| {S['name']} | {l['console_profile']} | {l['dlss']} | {l['framegen']} | {fmt(l.get('passes_off_final'))} | "
                 f"{fmt(S['session']['duration_s'])} | " + ' | '.join(row) + ' |')
    for nm, fn in (('MEDIAN of runs', lambda v: sorted(v)[len(v) // 2]), ('MIN', min), ('MAX', max)):
        o.append(f'| **{nm}** | | | | | | ' + ' | '.join(fmt(r(fn(v), 2)) if v else '-' for v in vals) + ' |')
    return '\n'.join(o) + '\n'


def load_any(p, a):
    if p.endswith('.summary.json') or (p.endswith('.json') and os.path.isfile(p)):
        return json.load(open(p, encoding='utf-8'))
    return summarise(p, a)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('paths', nargs='*')
    ap.add_argument('--out', help='folder for <name>.summary.json and <name>.summary.md (default: print markdown)')
    ap.add_argument('--label', help='free text stored in the summary (e.g. "cold, A pressed at 12 s, scenario S1")')
    ap.add_argument('--stall-ms', type=float, default=200.0)
    ap.add_argument('--stall-rel', type=float, default=3.0, help='also require max >= this x the span median')
    ap.add_argument('--heavy-draws', type=float, default=1000.0)
    ap.add_argument('--warmup-s', type=float, default=30.0)
    ap.add_argument('--compare', nargs=2, metavar=('A', 'B'))
    ap.add_argument('--latest', nargs='?', const=1, type=int)
    a = ap.parse_args()
    if a.compare:
        A, B = load_any(a.compare[0], a), load_any(a.compare[1], a)
        txt = compare(A, B, a)
        if a.out:
            os.makedirs(a.out, exist_ok=True)
            open(os.path.join(a.out, 'compare.md'), 'w', encoding='utf-8').write(txt)
        print(txt)
        return 0
    paths = []
    for q in a.paths:
        paths += sorted(glob.glob(q)) if any(c in q for c in '*?[') else [q]
    if a.latest:
        base = os.path.join(os.environ.get('LOCALAPPDATA', ''), 'victory', 'runs')
        paths += sorted(d for d in glob.glob(os.path.join(base, '*')) if os.path.isfile(os.path.join(d, 'victory.log')))[-a.latest:]
    if not paths:
        ap.error('no input')
    rc = 0
    allS = []
    for p in paths:
        S = summarise(p, a)
        if a.out:
            os.makedirs(a.out, exist_ok=True)
            base = re.sub(r'[^\w.-]+', '_', S['name'])
            json.dump(S, open(os.path.join(a.out, base + '.summary.json'), 'w', encoding='utf-8'), indent=1)
            open(os.path.join(a.out, base + '.summary.md'), 'w', encoding='utf-8').write(md(S))
        else:
            sys.stdout.reconfigure(encoding='utf-8')
            print(md(S))
        rc |= 1 if 'error' in S else 0
        allS.append(S)
    if a.out and len(allS) > 1:
        open(os.path.join(a.out, 'index.md'), 'w', encoding='utf-8').write(index_md(allS))
    return rc


if __name__ == '__main__':
    sys.exit(main())
