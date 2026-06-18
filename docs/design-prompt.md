Design **dotz** — a desktop coding-agent dashboard (like Claude Desktop / opencode) built on the pi.dev agent harness. ONE self-contained interactive HTML mockup, desktop-first (1280–1920px), dark. Use realistic mock data so it looks like a real running tool, not a toy.

AESTHETIC: cyberbrutalist / Catppuccin Mocha. Palette: bg #1e1e2e, panels #181825, surface #313244, border #45475a, text #cdd6f4, muted #a6adc8, primary lavender #b4befe, green #a6e3a1, red #f38ba8, yellow #f9e2af, cyan #89dceb, peach #fab387. SHARP corners (no rounding), thick 2px borders, high contrast, subtle scanline/grid texture. Fonts: 'Chakra Petch' for labels/headings (UPPERCASE, letter-spaced), 'JetBrains Mono' for code/ids/tool output, 'Pixelify Sans' for the 'dotz' wordmark. ASCII-block accents (░▒▓█▀▄) as section dividers and in the logo. Blinking block cursor █ for streaming. No glassmorphism, no rounded SaaS cards, no rainbow gradients.

LAYOUT — three columns + a bottom status bar:
TOP BAR: pixel 'dotz' wordmark + 'pi.dev coding agent' subtitle; center shows current session name 'fix: ws backpressure'; right shows a model badge 'openrouter / nex-agi/nex-n2-pro:free' and a green ● PI ONLINE chip.
LEFT RAIL (240px): 'SESSIONS' header with a '+ NEW' button; 3 session cards (name, last-active, token count e.g. '1.6K tok'); bottom: a connection chip with a blinking block + 'ws ✓ 127.0.0.1:4317'.
CENTER (flex): scrollable chat transcript, then a composer pinned at the bottom. Mock a real turn:
  • USER bubble: 'Create dotz_probe.txt with hello, then read it back.'
  • ASSISTANT block containing, in order: (1) a collapsible '▶ THINKING' strip — dimmed monospace, 2 lines visible; (2) a TOOL CARD with header '⚙ write' and a green status badge '✓ DONE', a left accent bar, body shows args in mono (path: dotz_probe.txt, content: "hello") and output 'Successfully wrote 15 bytes'; (3) assistant text 'Done — wrote the file and read it back. Contents: hello.'
  • A SECOND assistant message that is currently STREAMING: a half sentence followed by a blinking █ cursor, with a small 'medium ◷' reasoning tag.
COMPOSER: bordered multiline input, placeholder 'message dotz…   / skills · @ files'; a neon 'SEND ▸' button on the right; a row of quick-chips '/implement' '/scout-and-plan' 'skill:code-review'; while streaming, show a red 'STOP ■' button and a 'STEER' toggle.
RIGHT RAIL (300px) — stacked control cards, each with an ASCII header bar (e.g. '▓▓ MODEL ▓▓'):
  • MODEL: a TEXT INPUT labeled 'OPENROUTER MODEL' prefilled 'nex-agi/nex-n2-pro:free' — emphasize it is a FREE-FORM custom model id (you can type any OpenRouter model id); show tags 'ctx 256K' and 'reasoning ✓'; caption 'OpenRouter = custom model id'.
  • REASONING: a segmented slider OFF · MIN · LOW · MED · HIGH · XHIGH with HIGH active (neon lavender fill).
  • TOOLS: toggle chips read / bash / edit / write (ON, green) and grep / find / ls (OFF, dim).
  • SKILLS: list 'skill:deep-research', 'skill:code-review', 'skill:systematic-debugging', each with a ▶ run button.
  • SUBAGENTS: workflow cards '/implement', '/scout-and-plan', '/implement-and-review', plus agent chips 'scout', 'planner', 'reviewer'.
STATUS BAR (bottom, full width, mono): tokens '↑1.6K ↓420', cost '$0.00 free', and a context-usage meter '0.3% of 256K' as a thin neon bar.

Make it dense, powerful, and crisp — a real developer tool. Tasteful neon on dark, strong typographic hierarchy, ASCII texture throughout.
