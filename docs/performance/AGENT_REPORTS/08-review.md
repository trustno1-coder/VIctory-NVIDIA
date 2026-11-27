# 08 Architecture review (read-only)

Full audit: `docs/performance/ARCHITECTURE_AUDIT.md`. Nothing was built or run; no `.rs` edited.

- 425 distinct `VICTORY_*` names, about 340 behaviour switches. No single default table: code defaults (six idioms),
  `Play-VIctory.cmd` (~21 overrides), `host/menu.rs`. The shipped config differs from code defaults (`THIN`, `SHOW_AFTER`,
  `CARD_STEPS`, `JOIN_COPIES`, `INDIRECT_WINDOW`, `RELAY=2` are on only via the launcher).
- Real invariants: shader writes the title CPU reads land in title memory in order before any fence/present/wait that observes
  them (ocean maps via `texback.rs`, culling/skinning lists, bakes); stale shader output never overwrites newer title bytes;
  aliased views clean only if all read-only; slots never name unuploaded/freed textures; raw loads past a view return zeros;
  indirect args bounded; wave/helper-lane semantics; dynamic slots returned on GPU completion.
- Incidental: page/block sizes, card hash passes as the detector, per-slot mirroring cadence, which thread lands, ambient values.
- Workarounds on workarounds: five title-write detectors (guards, dirty bits, texture hash, hot-hash, recheck) plus diagnostic
  checks; slot-hiding chain (`SHOW_AFTER`/`KEEP_VIEWS`/`neutral()`/bake publication); relay x late-wait x join stack;
  `INDIRECT_REST`; hand-tuned pressure constants.
- Doc drift: relay "off by default" comment vs on in code; `WAVE_WIDE` "kept off" in journal vs enabled in
  `emit/mod.rs:747` (verify before translator work).
- Replay proves card-side equivalence for a recorded frame; it cannot prove watch/guard/landing logic, landing order,
  multi-frame state, races or TDR. Its integration test is synthetic (128x128).
- Missing enablers: resolved-config dump, golden-scene image/buffer diff, title-memory digest per present (catches landing
  reorder that pictures miss), soak harness, process-level A/B covering all switch idioms.
- Per-refactor checklists C1-C9 (config table, detector unification, declared writes, relay collapse, slot lifetime, budget
  model, dead-switch removal, translator, async) are in section 6 of the audit.
- Order: config table + dump -> harness -> slot lifetime/budget -> detectors -> relay.
- Limits: Appendix A default classes are heuristic; confirm at the read site.
