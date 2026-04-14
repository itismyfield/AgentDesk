# ADK Dashboard Design Direction v1

> This file is the original v1 snapshot.
> For future dashboard work, use `docs/adk-dashboard-design-direction.md` as the canonical working reference.

## Goal

AgentDesk dashboard is not a Discord replacement. It is the operational UI that does the jobs visual UX does better than natural language:

- quick state comprehension
- proactive visibility
- fast direct manipulation
- onboarding and guided setup
- selective delight around agents, office motion, XP, and receipts

The dashboard should feel like a trustworthy control plane first, with delight layered in intentionally.

## Design Summary

- Core reference: Linear
- Brand accent: VoltAgent
- Explanation/onboarding layer: Mintlify

Reference ratio:

- 70% Linear
- 20% VoltAgent
- 10% Mintlify

This means the system should be coherent and singular, not a patchwork of many inspirations.

## Product Mood

- operations-first
- precise but not cold
- restrained, not bland
- fast, not flashy
- playful in selected surfaces, not everywhere

## Visual Principles

1. Hierarchy comes from border, spacing, and surface contrast before shadow.
2. Accent color is sparse and meaningful.
3. Mobile layouts are first-class, not scaled-down desktop panels.
4. Dense data should still be scannable in under 3 seconds.
5. Playful elements should never reduce operational clarity.

## Color Tokens

Suggested starting tokens:

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
- Avoid global gradients, glass, and neon glow.

## Typography

- Primary UI voice: clean grotesk sans
- Technical voice: mono for logs, IDs, commands, and numeric values only

Recommended scale:

- `12`: meta labels
- `14`: default UI copy
- `16`: section title
- `20`: panel headline
- `28`: key metric

Rules:

- Do not use mono as the default font across the whole product.
- Long explanations should remain highly readable on mobile.

## Spacing and Shape

- Base spacing unit: `4px`
- Common spacing: `8 / 12 / 16 / 20 / 24`
- Panel radius: `14px`
- Control radius: `10px`
- Chip radius: `999px`

Rules:

- Prefer fewer sizes, repeated consistently.
- Do not invent page-specific corner radii or spacing systems.

## Depth Model

Allowed panel depth:

- layer 0: app background
- layer 1: section surface
- layer 2: active panel

Avoid building deep nested card stacks. Most screens should stop at depth 2.

## Layout System

- Desktop: 12-column grid
- Tablet: 8-column grid
- Mobile: 4-column grid
- Mobile gutter: `16px`

Rules:

- No page-level horizontal overflow.
- Mobile defaults to single-column unless a compact two-column metric layout is clearly better.
- Sidebars collapse into drawers or segmented top navigation on mobile.

## Component Families

### 1. Metric Cards

Use for:

- pulse summaries
- health stats
- token usage
- agent throughput

Behavior:

- one strong value
- one short supporting label
- one tiny change or status hint

### 2. Status Rows

Use for:

- agent list
- queue items
- issues requiring attention

Behavior:

- left: name and primary identity
- center: short status summary
- right: chip or mini metric

On mobile:

- collapse into stacked row
- never force horizontal scrolling

### 3. Settings Rows

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

### 4. Narrative Panels

Use for:

- onboarding
- empty states
- configuration guidance
- feature explanation

Rules:

- keep copy tight
- prefer inline helper text over tooltip-only explanation

### 5. Logs and Runs

Use for:

- command output
- agent events
- task run detail

Rules:

- mono only inside content area
- preserve contrast and timestamp rhythm
- allow copy/paste and easy scanning

## Screen Direction

### Pulse

Target feeling:

- fast operational scan

Composition:

- top summary band
- key attention cards
- recent activity list
- escalation rail for degraded or blocked states

### Health and Control Center

Target feeling:

- most Linear-like surface in the product

Composition:

- dense but tidy metrics
- clear red/yellow/green semantics
- strong grouping by concern

### Settings

Target feeling:

- structured configuration console, not a long form dump

Composition:

- section navigation
- grouped settings rows
- clear descriptions
- explicit save model

Mobile:

- accordion sections
- controls stacked under labels

### Agent Detail and Logs

Target feeling:

- technical, clear, slightly terminal-adjacent

Composition:

- identity header
- current state
- recent events
- logs or runs

### Onboarding

Target feeling:

- calm and guided

Composition:

- stepper
- short explanations
- visible progress
- one action emphasis per step

### Office, XP, Receipt, Achievements

Target feeling:

- the playful layer of ADK

Rules:

- keep the global design system intact
- allow more expressive motion and accent here
- never let delight reduce comprehension

## Motion

- default UI transition: `160ms`
- panel open/close: `220ms`
- avoid bounce-heavy motion
- use skeletons for loading
- use short toast and brief highlight for success feedback

Decorative motion should be concentrated in office-like or gamified surfaces only.

## Mobile Non-Negotiables

- zero horizontal page scroll
- no tiny text below practical tap readability
- controls must have touch-friendly hit targets
- settings controls must stack cleanly
- long labels must wrap or truncate intentionally
- chips and badges must never push layout wider than viewport

## Anti-Patterns

- too many reference styles mixed globally
- purple or multiple accent colors competing
- heavy shadows everywhere
- glassmorphism or blur-heavy panels
- mono everywhere
- desktop panel compositions shrunk onto mobile
- explanation hidden only in tooltip
- playful UI layered onto core operational paths

## Implementation Priority

### Phase 1

- establish tokens
- unify spacing, radius, borders, text hierarchy

### Phase 2

- Settings becomes the reference implementation screen

### Phase 3

- Pulse, Health, Control Center

### Phase 4

- Agent detail, run detail, logs

### Phase 5

- Office, receipts, XP, achievements

## Success Criteria

We will know the direction is working when:

- new screens feel like one product
- mobile no longer feels like compressed desktop
- settings are understandable without extra explanation
- operational states are legible at a glance
- playful surfaces still feel connected to the same design system

## Working Decision

ADK dashboard should become:

`a trustworthy operations spine with selective delight`

Not:

`a generic SaaS admin panel`

And not:

`a playful experiment that weakens operational clarity`
