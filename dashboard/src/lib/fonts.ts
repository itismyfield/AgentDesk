export const FONT_STACK_SANS =
  '-apple-system, BlinkMacSystemFont, "Apple SD Gothic Neo", "Noto Sans KR", "Segoe UI", system-ui, sans-serif';
export const FONT_STACK_MONO =
  '"JetBrains Mono", "SFMono-Regular", "SF Mono", Menlo, Consolas, monospace';
export const FONT_STACK_PIXEL =
  '"Silkscreen", -apple-system, BlinkMacSystemFont, "Apple SD Gothic Neo", "Segoe UI", system-ui, sans-serif';

const CJK_GLYPH_RE =
  /[\u1100-\u11ff\u3040-\u30ff\u3130-\u318f\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff\uac00-\ud7af]/;

export function getFontFamilyForText(
  text: string | null | undefined,
  variant: "sans" | "mono" | "pixel" = "pixel",
): string {
  if (variant === "sans") return FONT_STACK_SANS;
  if (variant === "mono") return FONT_STACK_MONO;
  return text && CJK_GLYPH_RE.test(text)
    ? FONT_STACK_SANS
    : FONT_STACK_PIXEL;
}
