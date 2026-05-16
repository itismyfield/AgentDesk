// @vitest-environment happy-dom

import { act, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";
import { BottomSheet } from "./BottomSheet";
import { Drawer } from "./Drawer";
import { Modal } from "./Modal";

interface PreventableEvent {
  preventDefault: () => void;
}

interface CapturedProps {
  children?: ReactNode;
  dismissible?: boolean;
  onEscapeKeyDown?: (event: PreventableEvent) => void;
  onInteractOutside?: (event: PreventableEvent) => void;
  onCloseAutoFocus?: (event: Event) => void;
  [key: string]: unknown;
}

const captured = vi.hoisted(() => ({
  dialogContentProps: [] as CapturedProps[],
  vaulContentProps: [] as CapturedProps[],
  vaulRootProps: [] as CapturedProps[],
}));

vi.mock("@radix-ui/react-dialog", async () => {
  const React = await vi.importActual<typeof import("react")>("react");

  function renderElement(
    tagName: string,
    props: CapturedProps & { children?: ReactNode },
  ) {
    const {
      children,
      onEscapeKeyDown,
      onInteractOutside,
      onCloseAutoFocus,
      ...domProps
    } = props;
    return React.createElement(tagName, domProps, children);
  }

  return {
    Root: ({ children }: { children: ReactNode }) =>
      React.createElement("div", { "data-dialog-root": true }, children),
    Portal: ({ children }: { children: ReactNode }) =>
      React.createElement(React.Fragment, null, children),
    Overlay: (props: CapturedProps & { children?: ReactNode }) =>
      renderElement("div", props),
    Content: (props: CapturedProps & { children?: ReactNode }) => {
      captured.dialogContentProps.push(props);
      return renderElement("section", { role: "dialog", ...props });
    },
    Title: (props: CapturedProps & { children?: ReactNode }) =>
      renderElement("h2", props),
    Description: (props: CapturedProps & { children?: ReactNode }) =>
      renderElement("p", props),
    Close: ({ children }: { children: ReactNode }) =>
      React.createElement(React.Fragment, null, children),
  };
});

vi.mock("vaul", async () => {
  const React = await vi.importActual<typeof import("react")>("react");

  function renderElement(
    tagName: string,
    props: CapturedProps & { children?: ReactNode },
  ) {
    const {
      children,
      onEscapeKeyDown,
      onInteractOutside,
      onCloseAutoFocus,
      ...domProps
    } = props;
    return React.createElement(tagName, domProps, children);
  }

  return {
    Drawer: {
      Root: (props: CapturedProps & { children?: ReactNode }) => {
        captured.vaulRootProps.push(props);
        return React.createElement(
          "div",
          { "data-vaul-root": true },
          props.children,
        );
      },
      Portal: ({ children }: { children: ReactNode }) =>
        React.createElement(React.Fragment, null, children),
      Overlay: (props: CapturedProps & { children?: ReactNode }) =>
        renderElement("div", props),
      Content: (props: CapturedProps & { children?: ReactNode }) => {
        captured.vaulContentProps.push(props);
        return renderElement("section", { role: "dialog", ...props });
      },
      Title: (props: CapturedProps & { children?: ReactNode }) =>
        renderElement("h2", props),
      Close: ({ children }: { children: ReactNode }) =>
        React.createElement(React.Fragment, null, children),
      Handle: (props: CapturedProps & { children?: ReactNode }) =>
        renderElement("div", props),
    },
  };
});

describe("overlay primitives", () => {
  let container: HTMLDivElement | null = null;
  let root: Root | null = null;

  async function render(element: ReactNode) {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
    await act(async () => {
      root?.render(element);
    });
    return container;
  }

  function preventableEvent() {
    return { preventDefault: vi.fn() };
  }

  afterEach(async () => {
    if (root) {
      await act(async () => {
        root?.unmount();
      });
      root = null;
    }
    container?.remove();
    container = null;
    captured.dialogContentProps.length = 0;
    captured.vaulContentProps.length = 0;
    captured.vaulRootProps.length = 0;
    vi.restoreAllMocks();
  });

  it("associates modal descriptions with the dialog content", async () => {
    await render(
      <Modal
        open
        onClose={() => {}}
        title="Details"
        description="Longer context"
      >
        Body
      </Modal>,
    );

    const dialog = document.querySelector("[role='dialog']");
    const description = document.querySelector("p");

    expect(dialog?.getAttribute("aria-describedby")).toBeTruthy();
    expect(dialog?.getAttribute("aria-describedby")).toBe(description?.id);
  });

  it("keeps modal escape and backdrop guards wired", async () => {
    await render(
      <Modal
        open
        onClose={() => {}}
        title="Details"
        closeOnEsc={false}
        closeOnBackdrop={false}
      >
        Body
      </Modal>,
    );

    const escapeEvent = preventableEvent();
    const outsideEvent = preventableEvent();
    captured.dialogContentProps[0].onEscapeKeyDown?.(escapeEvent);
    captured.dialogContentProps[0].onInteractOutside?.(outsideEvent);

    expect(escapeEvent.preventDefault).toHaveBeenCalledTimes(1);
    expect(outsideEvent.preventDefault).toHaveBeenCalledTimes(1);
  });

  it("keeps drawer escape and backdrop guards wired", async () => {
    window.matchMedia = vi.fn().mockReturnValue({
      matches: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });

    await render(
      <Drawer
        open
        onClose={() => {}}
        title="Details"
        closeOnEsc={false}
        closeOnBackdrop={false}
      >
        Body
      </Drawer>,
    );

    const escapeEvent = preventableEvent();
    const outsideEvent = preventableEvent();
    captured.dialogContentProps[0].onEscapeKeyDown?.(escapeEvent);
    captured.dialogContentProps[0].onInteractOutside?.(outsideEvent);

    expect(escapeEvent.preventDefault).toHaveBeenCalledTimes(1);
    expect(outsideEvent.preventDefault).toHaveBeenCalledTimes(1);
  });

  it("keeps bottom sheet escape and backdrop guards wired", async () => {
    await render(
      <BottomSheet
        open
        onClose={() => {}}
        title="Details"
        closeOnEsc={false}
        closeOnBackdrop={false}
      >
        Body
      </BottomSheet>,
    );

    const escapeEvent = preventableEvent();
    const outsideEvent = preventableEvent();
    captured.vaulContentProps[0].onEscapeKeyDown?.(escapeEvent);
    captured.vaulContentProps[0].onInteractOutside?.(outsideEvent);

    expect(captured.vaulRootProps[0].dismissible).toBe(false);
    expect(escapeEvent.preventDefault).toHaveBeenCalledTimes(1);
    expect(outsideEvent.preventDefault).toHaveBeenCalledTimes(1);
  });
});
