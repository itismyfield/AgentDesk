# ADK Dashboard Design Direction

Status:

- canonical working reference for ADK dashboard design work
- source snapshot: `docs/adk-dashboard-design-direction-v1.md`

## Why this document exists

This document keeps dashboard work coherent across issues, PRs, and future UI rewrites.

The goal is not to freeze every visual decision forever. The goal is to keep the product moving in one consistent direction.

## Core framing

AgentDesk dashboard is not a Discord replacement.

It is the operational UI for jobs visual structure and direct manipulation do better than natural language:

- quick state comprehension
- proactive visibility
- fast direct manipulation
- onboarding and guided setup
- selective delight around agents, office motion, XP, and receipts

## Reference stack

- Core reference: Linear
- Brand accent: VoltAgent
- Explanation/onboarding layer: Mintlify

Reference ratio:

- 70% Linear
- 20% VoltAgent
- 10% Mintlify

This is a single system with limited accents, not a collage of references.

## Product identity

ADK dashboard should feel like:

- operations-first
- precise but not cold
- restrained, not bland
- fast, not flashy
- playful in selected surfaces, not everywhere

Working sentence:

`a trustworthy operations spine with selective delight`

## Decision priority

When tradeoffs conflict, choose in this order:

1. operational clarity
2. mobile usability
3. system consistency
4. delight

## System rules

1. Hierarchy comes from border, spacing, and surface contrast before shadow.
2. Accent color is sparse and meaningful.
3. Mobile layouts are first-class, not scaled-down desktop panels.
4. Dense data should still be scannable in under 3 seconds.
5. Playful elements must never weaken operational clarity.

## Visual tokens

Target dark-mode starting tokens:

- `bg.app`: `#121313`
- `bg.surface`: `#17191a`
- `bg.panel`: `#1d2022`
- `bg.panel-active`: `#23272a`
- `border.subtle`: `#2a2f33`
- `border.strong`: `#3a4147`
- `text.primary`: `#f3f5f7`
- `text.secondary`: `#b8c0c7`
- `text.tertiary`: `#7f8a94`
- `accent.primary`: `#6ef2a3`
- `accent.primary-soft`: `#173126`
- `accent.info`: `#66b3ff`
- `accent.warn`: `#f5bd47`
- `accent.danger`: `#ff6b6b`

Rules:

- Keep one primary accent only.
- Use semantic colors only for state.
- Avoid global gradients, glass, and neon glow in core operational paths.

## Typography

- Primary UI voice: clean grotesk sans
- Technical voice: mono only for logs, IDs, commands, and numeric values

Recommended scale:

- `12`: meta labels
- `14`: default UI copy
- `16`: section title
- `20`: panel headline
- `28`: key metric

Rules:

- Do not use mono as the default font across the whole product.
- Long explanations must remain readable on mobile.

## Spacing and shape

- Base spacing unit: `4px`
- Common spacing: `8 / 12 / 16 / 20 / 24`
- Panel radius: `14px`
- Control radius: `10px`
- Chip radius: `999px`

Rules:

- Prefer fewer sizes, repeated consistently.
- Do not invent page-specific spacing systems or corner radii.

## Depth model

Allowed panel depth:

- layer 0: app background
- layer 1: section surface
- layer 2: active panel

Most screens should stop at depth 2.

## Layout system

- Desktop: 12-column grid
- Tablet: 8-column grid
- Mobile: 4-column grid
- Mobile gutter: `16px`

Rules:

- No page-level horizontal overflow.
- Mobile defaults to single column unless a compact two-column metric layout is clearly better.
- Sidebars collapse into drawers or segmented top navigation on mobile.

## Component grammar

### Metric cards

Use for:

- pulse summaries
- health stats
- token usage
- agent throughput

Behavior:

- one strong value
- one short supporting label
- one tiny change or status hint

### Status rows

Use for:

- agent list
- queue items
- issues requiring attention

Behavior:

- left: name and identity
- center: short status summary
- right: chip or mini metric

Mobile rule:

- collapse into stacked row
- never force horizontal scrolling

### Settings rows

Structure:

- setting name
- one-line explanation
- helper or warning if needed
- control on the right for desktop
- control below for mobile

Control mapping:

- bool -> toggle
- enum -> segmented control or select
- number -> stepper or constrained input
- text -> text field only when free-form text is actually needed

### Narrative panels

Use for:

- onboarding
- empty states
- configuration guidance
- feature explanation

Rules:

- keep copy tight
- prefer inline helper text over tooltip-only explanation

### Logs and runs

Use for:

- command output
- agent events
- task run detail

Rules:

- mono only inside content area
- preserve contrast and timestamp rhythm
- allow copy/paste and easy scanning

## Screen direction

### Pulse

- fast operational scan
- summary band first
- attention and degradation surfaced early

### Health and Control Center

- most Linear-like surface in the product
- dense but tidy metrics
- grouping by concern

### Settings

- structured configuration console, not a long form dump
- grouped settings rows
- explicit save model
- mobile accordion sections with stacked controls

### Agent detail and logs

- technical and clear
- slightly terminal-adjacent, not terminal cosplay

### Onboarding

- calm and guided
- short explanations
- one action emphasis per step

### Office, XP, receipt, achievements

- playful layer of ADK
- may use more expressive motion and accent
- must still feel connected to the global system

## Motion

- default UI transition: `160ms`
- panel open/close: `220ms`
- avoid bounce-heavy motion
- use skeletons for loading
- use short toast and brief highlight for success feedback

Decorative motion should be concentrated in office-like or gamified surfaces only.

## Mobile non-negotiables

- zero horizontal page scroll
- no tiny text below practical tap readability
- controls must have touch-friendly hit targets
- settings controls must stack cleanly
- long labels must wrap or truncate intentionally
- chips and badges must never push layout wider than viewport

## Anti-patterns

- too many reference styles mixed globally
- purple or multiple accent colors competing
- heavy shadows everywhere
- glassmorphism or blur-heavy panels
- mono everywhere
- desktop panel compositions shrunk onto mobile
- explanation hidden only in tooltip
- playful UI layered onto core operational paths

## Implementation order

### Phase 1

- establish tokens
- unify spacing, radius, borders, and text hierarchy

### Phase 2

- Settings becomes the reference implementation screen

### Phase 3

- Pulse, Health, Control Center

### Phase 4

- Agent detail, run detail, logs

### Phase 5

- Office, receipts, XP, achievements

## Working rule for future dashboard work

For dashboard issues and PRs, include at least one short sentence about which design principles the change is following.

Examples:

- `operations-first`
- `mobile-first`
- `border-driven hierarchy`
- `selective delight`
- `settings are structure-first`

## Related documents

- `docs/adk-dashboard-design-direction-v1.md`
- `docs/adk-dashboard-design-implementation-todo.md`
