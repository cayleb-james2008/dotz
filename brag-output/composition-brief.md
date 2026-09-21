# Hyperframes Composition Brief: dotz

## Objective
Create a short launch-style brag video for dotz.

## Output
- Composition directory: `/home/cayleb/Work/projects/oss-showcase/dotz/brag-output/composition/`
- Rendered video: `/home/cayleb/Work/projects/oss-showcase/dotz/brag-output/brag.mp4`
- Format: landscape — 1920x1080
- Duration: 20.2 seconds (0 → 20.19)

## Source Material
- Project root: /home/cayleb/Work/projects/oss-showcase/dotz
- Primary files read: web/index.html, web/styles.css, README.md
- Product name: dotz
- Tagline / strongest claim: One prompt becomes a team of AI coding agents.
- Key UI or visual moment to recreate: the live workflow DAG (prompt → scout/planner/worker/reviewer → ◆ HUMAN GATE → verified result) + the command-center composer line
- Copy that must appear verbatim:
  - One prompt. A team of agents.
  - tell dotz what to build…
  - 813 tests passing
  - ◆ HUMAN GATE

## Creative Direction
- Tone preset: polished
- Creative direction: serious engineering showcase with an edge — a hiring-manager cut
- Interpretation: 4 scenes, long holds, clean slides; confidence through restraint; dark dashboard type, hairline borders, mono data lines
- Angle: The graph is the product — the video performs multi-agent orchestration on screen instead of describing it.
- Hook: "One prompt. A team of agents." over "DOTZ · ULTRA CODE" + axum/Tauri sub-line
- Outro / punchline: "dotz — ultra code · pi.dev" + "Rust-native multi-agent coding, on Windows + Linux."
- Avoid:
  - Generic SaaS language
  - Abstract filler visuals
  - Unrelated visual redesign

## Visual Identity
- Background: #0d0d12 (exact, --bg)
- Panel: #232336 (--surface)
- Text: #e6e9f3 (exact, --text)
- Accent: #b4befe (lavender, --lav)
- Secondary accent: #89dceb (cyan)
- Pass green: #a6e3a1 (bold verdict stamps)
- Hairline: #2f2f45 (--border)
- Muted #a6adc8: large (≥31px) or bold use only (contrast caution on near-black)
- Display font: system-ui stack with technical letterspacing (site uses Chakra Petch, not available to renderer)
- Body font: system-ui stack; data/mono lines: ui-monospace/Consolas
- Visual references from the project: workflow DAG SVG node/edge graph, command-center composer, ◆ HUMAN GATE card, topbar knob labels (PROVIDER/MODEL/SUBAGENT/REASONING)

## Storyboard
Use the storyboard in `/home/cayleb/Work/projects/oss-showcase/dotz/brag-output/brag-plan.md` as the creative contract.

Scene summary:
1. Hook: the premise — 3.27s — giant "One prompt. A team of agents." + axum/Tauri sub + platform strip
2. Live workflow graph — 5.47s — prompt fans out to scout/planner/worker/reviewer → ◆ HUMAN GATE → verified result
3. Proof payoff — 7.10s — "813 tests passing" + sandbox previews + on-device memory + verify-before-merge
4. Outro: the name — 4.35s — dotz + ultra code · pi.dev + Windows + Linux

## Audio
- Audio role: restrained professional bed with sparse accents
- Audio arc: quiet bed establishes under hook, momentum under graph fan-out, single bell at 813 payoff, soft bell + fade at outro
- Music: music-bed.mp3 (copied vol-12 into composition/assets/music/), volume 0.30, fade last ~1.5s
- Music cue guidance: bundled preset /home/cayleb/.skill-library/active/brag/assets/music/cues/happy-beats-business-moves-vol-12-by-ende-dot-app.music-cues.json — strong locks at 8.74 / 13.11 / 15.84 (±0.15s); beat-grid node arrivals at 4.39/4.91/5.34/6.00 and proof lines at 13.64/14.20 (±0.10s); ignore cues wherever they hurt readability
- Audio-reactive treatment: subtle; graph-node glow + verdict presence breathe gently with the bed (authored pulse acceptable if RMS extraction helper unavailable — document it)
- Audio-coupled moments:
  - Scene 2 node fan-out — one soft drop as first node lands, rest silent on beats
  - Scene 3 813 payoff — one short bell at 13.11
  - Scene 4 outro title — one soft bell at ~15.9
- SFX selection guidance: low-HF-risk only; card/node sounds for node reveals, short announcement cue for the 813 payoff, restraint when the edit is busy
- SFX analysis guidance: /home/cayleb/.skill-library/active/brag/assets/sfx/sfx-analysis.md
- Exact SFX choice: Hyperframes should choose filenames, timestamps, density, and volume based on the implemented animation (suggested: interface/drop_001, impact/impactBell_heavy_000, impact/impactBell_heavy_003 — already copied into composition/assets/sfx/)
- Audio files: copied into `/home/cayleb/Work/projects/oss-showcase/dotz/brag-output/composition/assets/`

## Hyperframes Instructions
Load the composition-building Hyperframes domain skills — `hyperframes-core` (composition contract + `data-*` timing), `hyperframes-animation` (motion), `hyperframes-creative` (design spec, beats, audio-reactive), `hyperframes-keyframes` (seek-safe keyframes), and `hyperframes-cli` (lint/check/render). /brag is its own workflow: do not enter the `hyperframes` entry-point intent interview and do not route into its generic promo / launch-video workflow. Prefer native Hyperframes conventions over anything in `/brag`.

Requirements:
- Show at least one real UI, copy, or visual element from the source project.
- Keep all text readable in the final render.
- Keep the video within 15-25 seconds.
- Include the planned music/SFX layer unless audio was explicitly disabled or documented as intentionally silent.
- Treat `/brag` audio notes as guidance, not a fixed cue sheet. Choose SFX after the visual animation exists.
- Treat music cue metadata as optional timing hints. Hyperframes decides exact animation timing and should ignore cues that hurt readability, scene pacing, or the product story.
- Major reveals may move toward nearby strong cues within about 0.15s. Smaller entrances may align to nearby beat points within about 0.10s. Use only 1-3 strong cue locks in a 15-25s video unless the edit clearly benefits from more.
- Use SFX to support motion and interaction: card sounds for card-like reveals, short announcement cues for major payoffs, key/click sounds for text or user actions, and restraint when the edit is already busy.
- Honor planned music treatment such as fade-outs, ducking, beat-aligned reveals, or letting a final SFX ring over the music, using the best Hyperframes-supported implementation.
- When music is present and the treatment is not `none`, consider Hyperframes audio-reactive workflow: extract audio data and use RMS/frequency bands for subtle, brand-specific motion. Good targets are glow, depth, background warmth, card presence, title emphasis, or other existing visual elements. Avoid waveform/equalizer visuals, musical-note graphics, generic particle systems, strobing, or heavy pulsing.
- Use local assets for audio and any required runtime/media dependencies when possible.
- Run `hyperframes check` before render — it is brag's single gate.
