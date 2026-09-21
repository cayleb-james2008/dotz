# Brag Plan: dotz

## What is this app?
dotz is a native-Rust (axum + Tauri) multi-agent coding dashboard: one prompt becomes a team of AI subagents running on a live workflow graph, with sandboxed web previews and on-device memory — 813 tests passing, on Windows + Linux.

## The angle
The graph is the product. Instead of describing multi-agent orchestration, the video performs it: a prompt fans out to scout / planner / worker / reviewer nodes on a live DAG, each landing on the music's beat, then a reviewer stamps it verified. The hook is the premise itself — one prompt, a whole team — stated with engineering confidence, no parody.

## Hook (first 2-3 seconds)
Full-screen settled lockup: small "DOTZ · ULTRA CODE" marker, giant "One prompt. A team of agents.", sub "native-Rust multi-agent coding dashboard — axum + Tauri". No buildup, no logo-first; the premise IS the hook.

## Key moments (the middle)
- The live workflow graph recreated: prompt node fans out to 4 agent nodes (scout / planner / worker / reviewer) arriving one by one on the beat grid, converging on a "◆ HUMAN GATE → verified result" verdict.
- The proof strip: "813 tests passing" landing on the strong cue, with "sandboxed web previews · on-device memory" and "in-process runtime — no SDK, no IPC".
- The composer line, verbatim in spirit: "tell dotz what to build…" — the entry point of the user flow.

## Outro / punchline
"dotz — ultra code · pi.dev" + "Rust-native multi-agent coding, on Windows + Linux." Lands on the final strong cue; one soft bell payoff. Confident close for a hiring-manager audience.

## User flow worth showing
Entry → key action → result: type into the composer ("tell dotz what to build…") → watch subagents materialize as live graph nodes → reviewer verifies before it lands. Centerpiece scenes show that flow (composer line → DAG fan-out → verified result), not landing-page sections.

## Tone
- Preset: polished
- Creative direction: serious engineering showcase with an edge — a hiring-manager cut, confident and specific
- Interpretation: 4 scenes with long holds and clean slides; confidence through restraint; dark technical dashboard type, hairline borders, mono data lines, node/chip graph motifs exactly like the app. No jokes, no parody, no invented claims.

## Format: landscape — 1920x1080
## Duration: 20.2 seconds (0 → 20.19)

## Visual identity (from the project)
- Background: #0d0d12 (near-black, --bg)
- Panel: #232336 (--surface)
- Accent: #b4befe (lavender, --lav)
- Secondary accent: #89dceb (cyan, --cyan)
- Text: #e6e9f3 (--text)
- Muted: #a6adc8 (--muted, large/bold use only)
- Pass green: #a6e3a1 (--green, bold verdict stamps)
- Hairline: #2f2f45 (--border)
- Display font: "Chakra Petch", system sans fallback (site --display; bundled Google font not available to renderer — use system-ui stack with technical letterspacing)
- Body font: system-ui stack; data/mono lines: ui-monospace/"JetBrains Mono"/Consolas
- Strongest visual element: the live workflow DAG (SVG node/edge graph with panel-colored chips) + the command-center composer + the ◆ HUMAN GATE card

## Share copy (draft)
Introducing dotz: a native-Rust multi-agent coding dashboard where one prompt becomes a team of agents on a live workflow graph — 813 tests passing, on Windows + Linux.

## Audio direction
- Role: restrained professional bed with sparse accents
- Music: happy-beats-business-moves-vol-12-by-ende-dot-app.mp3 (steady and clean, 109.96 BPM; skill's pick for polished/cinematic)
- Music treatment: start 0.0, volume 0.30, no fade-in, gentle fade-out over last ~1.5s; major reveals lean toward strong cues
- Music cue guidance: bundled preset read (assets/music/cues/...vol-12...json). Strong-cue locks (major moments, ±0.15s): Scene2→3 transition at 8.74s (strong_beat 8.7423); 813-tests payoff at 13.11s (strong_beat 13.1077); Scene3→4 transition at 15.84s. Beat-grid windows (sequential nodes, ±0.10s): agent nodes at 4.39 / 4.91 / 5.34 / 6.00; proof lines at 13.64 / 14.20. Nodes arrive on beats but HOLD as a set to 8.74s so text stays readable.
- Audio-reactive treatment: subtle; graph-node glow + verdict presence breathe gently with the bed (authored pulse on the timeline; no waveform/equalizer visuals)
- SFX posture: sparse, low-HF-risk only (polished): interface/drop_001 (node reveal), impact/impactBell_heavy_000 (813 payoff), impact/impactBell_heavy_003 (outro landing)
- Audio-coupled moments: agent nodes arrive one by one with one soft drop on the first node; 813 payoff lands with one short bell; outro title lands with one soft bell
- Restraint rule: no SFX on every node, no typing ticks under dense copy, nothing aggressive, music never above 0.30

## Storyboard

### Scene 1 — Hook: the premise — 3.27s (0 → 3.27)
On screen: small "DOTZ · ULTRA CODE" eyebrow, giant "One prompt. A team of agents.", sub "native-Rust multi-agent coding dashboard — axum + Tauri". Bottom strip: "RUST · TAURI · WINDOWS + LINUX".
Sequential/interaction: none — full lockup fades/slides in fast (0.4s), then HOLDS settled to 3.27.
Audio intent: music establishes the bed; quiet confidence.
Audio-coupled idea: none (let the hook read in silence-plus-bed).
Music: vol-12 steady bed at 0.30.
Transition mood: clean slide → Scene 2.

### Scene 2 — Live workflow graph — 5.47s (3.27 → 8.74) // beat-locked: 8.74s
On screen: recreated DAG — header "Live workflow graph", sub "every subagent a node · every tool a chip". Prompt node fans out to 4 agent nodes (scout / planner / worker / reviewer), converging on "◆ HUMAN GATE → verified result". Composer hint "tell dotz what to build…" as the entry label.
Sequential/interaction: yes — 4 agent nodes arrive one by one on the beat grid (// beat-grid: scout 4.39, planner 4.91, worker 5.34, reviewer 6.00), full graph holds to 8.74.
Audio intent: quiet momentum building under the fan-out.
Audio-coupled idea: one soft drop_001 as scout lands; nodes 2-4 arrive silently on beats (restraint).
Music: bed continues.
Transition mood: clean slide → Scene 3 (// beat-locked 8.7423 strong_beat).

### Scene 3 — Proof payoff — 7.10s (8.74 → 15.84) // beat-locked: 15.84s
On screen: kicker "In-process runtime. No SDK, no IPC." then giant "813 tests passing" landing on the 13.11 strong cue with lines "sandboxed web previews · on-device memory" and "adversarial verify-before-merge".
Sequential/interaction: yes — kicker arrives early; proof lines arrive on beats 13.64 / 14.20 after the payoff; full set holds to 15.84. // beat-grid noted in composition.
Audio intent: the payoff moment — one short bell as 813 lands.
Audio-coupled idea: impactBell_heavy_000 at 13.11 with the verdict (// beat-locked 13.1077 strong_beat).
Music: bed continues.
Transition mood: clean slide → Scene 4 (// beat-locked 15.836 strong_beat).

### Scene 4 — Outro: the name — 4.35s (15.84 → 20.19)
On screen: "DOTZ · ULTRA CODE" eyebrow, "dotz" title, "Rust-native multi-agent coding, on Windows + Linux.", "ultra code · pi.dev". Settles by ~16.6, holds to end (music fades last 1.5s).
Sequential/interaction: none — lockup arrives as one, holds.
Audio intent: landing + gentle close; one soft bell under the title, music fades out.
Audio-coupled idea: impactBell_heavy_003 at ~15.9 with the title.
Music: bed fades 18.7 → 20.19.
Transition mood: end (hold final frame; no exit animation).

**Music mood for this video:** steady, clean, restrained professional bed.
**Audio summary:** A quiet vol-12 bed at 0.30 carries all 20 seconds; three sparse low-risk accents (one drop, two soft bells) mark the graph fan-out, the 813 payoff, and the outro landing; everything else is held text and clean slides.
