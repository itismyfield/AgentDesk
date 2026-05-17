import { Children, isValidElement, type ReactNode } from "react";

export function getOverlayAccessibleTitle(
  title: ReactNode,
  ariaLabel: string | undefined,
  fallback: string,
) {
  const explicitLabel = normalizeText(ariaLabel);
  if (explicitLabel) return explicitLabel;

  return getOverlayTitleText(title) ?? fallback;
}

function getOverlayTitleText(title: ReactNode) {
  const text = collectText(title);
  return normalizeText(text);
}

function collectText(node: ReactNode): string {
  if (node === null || node === undefined || typeof node === "boolean") {
    return "";
  }

  if (
    typeof node === "string" ||
    typeof node === "number" ||
    typeof node === "bigint"
  ) {
    return String(node);
  }

  if (Array.isArray(node)) {
    return node.map(collectText).join(" ");
  }

  if (isValidElement<{ children?: ReactNode; "aria-hidden"?: unknown }>(node)) {
    if (
      node.props["aria-hidden"] === true ||
      node.props["aria-hidden"] === "true"
    ) {
      return "";
    }
    return collectText(node.props.children);
  }

  let text = "";
  Children.forEach(node, (child) => {
    text = `${text} ${collectText(child)}`;
  });
  return text;
}

function normalizeText(text: string | undefined) {
  const normalized = text?.replace(/\s+/g, " ").trim();
  return normalized && normalized.length > 0 ? normalized : undefined;
}
