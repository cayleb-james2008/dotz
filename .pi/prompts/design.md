---
description: Open Design (native) — design a graphic/UI artifact using a bundled design system, then preview & export it in the DESIGN panel
---
Design workflow (Open Design, native to dotz). The DESIGN panel opens automatically. For the request: $@

1. PICK a design system. Browse the 150+ bundled systems in the DESIGN panel (or GET /api/design/systems). READ the chosen system's `.pi/design-systems/<slug>/DESIGN.md` and `tokens.css` and honor its tokens — never invent off-brand colors/spacing.
2. LOAD a relevant design skill from the pool (source: design) with the `skill` tool when one fits (e.g. canvas-design, brand-guidelines, ad-creative, article-magazine, algorithmic-art).
3. AUTHOR a real, self-contained HTML/CSS artifact: paste the chosen system's `:root` tokens FIRST, then build everything with `var(...)`. Avoid AI-slop (no purple gradients, fake glassmorphism, generic SaaS cards); meet WCAG contrast, real focus states, 44px touch targets. Save it as an `.html` file in the project.
4. PREVIEW the artifact in the DESIGN panel and EXPORT it (HTML or PDF).

Decompose and disperse to subagents per the workflow doctrine when the task is non-trivial, and verify the result against the chosen design system's rules before claiming done.
