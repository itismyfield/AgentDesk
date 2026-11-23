// Agent manager styles that Tailwind utilities cannot express; React hoists this once into <head>.
// 1) keyframes for the `motion-safe:animate-[…]` classes;
// 2) the app's global focus outline lands on the bare <input>/<textarea> inside kit fields, so it moves to the field box.
const css =
  "@keyframes agent-fade-in{from{opacity:0}}" +
  "@keyframes agent-sheet-in{from{transform:translateY(100%)}}" +
  "@keyframes agent-card-in{from{opacity:0;transform:translateY(8px) scale(.98)}}" +
  ".agent-kit-scope :is(.aui-text-field-clean__box,.aui-text-area-clean__box) :is(input,textarea):focus-visible{outline:none}" +
  ".agent-kit-scope :is(.aui-text-field-clean__box,.aui-text-area-clean__box):has(:focus-visible){outline:2px solid var(--th-focus-ring,var(--accent));outline-offset:2px}";

export function AgentManagerStyles() {
  return (
    <style href="agent-manager-styles" precedence="default">
      {css}
    </style>
  );
}
