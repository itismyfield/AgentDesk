# ADK Dashboard Design Implementation TODO

Purpose:

- convert the design direction into concrete tokens, component rules, and screen-level refactors
- keep future dashboard work tied to real files instead of abstract taste

## Current gap summary

### `dashboard/src/styles/main.css`

Current state:

- indigo/cyan leaning theme
- several gradient and glow-heavy utilities still shape the default visual tone

Needed direction:

- warm dark neutral base
- border and surface contrast before glow
- one sparse primary accent

### `dashboard/src/components/SettingsView.tsx`

Current state:

- already closest to the target direction
- grouped settings and clearer helper copy exist

Needed direction:

- use as the reference screen for the design system
- extract reusable section shell, settings row, and helper patterns later

### `dashboard/src/components/PulseView.tsx`

Current state:

- good candidate for operational scan improvements

Needed direction:

- clearer summary hierarchy
- more consistent metric card grammar
- stronger attention rail for degraded states

### `dashboard/src/components/ControlCenterView.tsx`

Current state:

- operationally important, but still needs stricter layout and hierarchy discipline

Needed direction:

- denser but tidier grouping
- stronger state semantics
- clearer section rhythm

### `dashboard/src/components/OnboardingWizard.tsx`

Current state:

- functionally rich
- explanation density is high

Needed direction:

- calmer Mintlify-like step flow
- shorter helper copy
- stronger primary action focus per step

### `dashboard/src/components/OfficeView.tsx`

Current state:

- valuable playful layer

Needed direction:

- preserve delight
- ensure playful visuals still inherit global tokens, type rhythm, and spacing rules

## Implementation phases

## Phase 1: Token foundation

Target files:

- `dashboard/src/styles/main.css`

Tasks:

- replace the current default visual center of gravity with the canonical dark token set
- reduce gradient-as-identity usage in operational surfaces
- define shared spacing, radius, border, and semantic color naming
- distinguish operational accent from playful accent usage

Exit criteria:

- core surfaces no longer feel cyan/indigo-driven
- panels, text, and borders read as one coherent system

## Phase 2: Reference screen

Target files:

- `dashboard/src/components/SettingsView.tsx`
- `dashboard/src/components/common/*`

Tasks:

- standardize section header grammar
- standardize settings row grammar
- standardize helper and warning text pattern
- identify which pieces should become shared UI building blocks

Exit criteria:

- Settings becomes the screen future UI work copies from first

## Phase 3: Operations surfaces

Target files:

- `dashboard/src/components/PulseView.tsx`
- `dashboard/src/components/ControlCenterView.tsx`
- `dashboard/src/components/DashboardPageView.tsx`

Tasks:

- unify metric card style
- unify status row composition
- create a stronger degraded/blocked attention pattern
- remove decorative treatments that do not help scanning

Exit criteria:

- Pulse and Control Center feel like the same operational product

## Phase 4: Narrative and logs

Target files:

- `dashboard/src/components/OnboardingWizard.tsx`
- `dashboard/src/components/session-panel/SessionPanel.tsx`
- other log/run detail surfaces

Tasks:

- make onboarding calmer and more guided
- improve readability of technical run/log surfaces
- keep mono contained to technical content

Exit criteria:

- explanation-heavy and technical screens still feel part of one system

## Phase 5: Playful layer alignment

Target files:

- `dashboard/src/components/OfficeView.tsx`
- `dashboard/src/components/dashboard/ReceiptWidget.tsx`
- XP and achievement-related surfaces

Tasks:

- preserve delight
- remove any playful styling that breaks core system consistency
- make rewards and receipts feel connected to the same product spine

Exit criteria:

- playful surfaces feel expressive without looking like a separate app

## Design review checklist

Use this checklist in future dashboard work:

- does the change improve operational clarity first?
- does mobile still feel intentional, not compressed?
- does the screen use existing token and spacing rules?
- is accent color meaningful rather than decorative?
- is helper text inline where understanding depends on it?
- if playful, does it still feel connected to the same system?

## Near-term practical targets

- establish canonical tokens in `dashboard/src/styles/main.css`
- keep `dashboard/src/components/SettingsView.tsx` as the reference screen
- align `PulseView` and `ControlCenterView` next
- revisit `OnboardingWizard` after operational surfaces stabilize
