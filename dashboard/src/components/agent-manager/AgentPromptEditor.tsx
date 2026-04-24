import { useEffect, useRef, useState } from "react";

interface AgentPromptEditorProps {
  value: string;
  onChange: (value: string) => void;
  minHeight?: number;
  label: string;
}

export default function AgentPromptEditor({
  value,
  onChange,
  minHeight = 300,
  label,
}: AgentPromptEditorProps) {
  const hostRef = useRef<HTMLDivElement | null>(null);
  const editorRef = useRef<any>(null);
  const onChangeRef = useRef(onChange);
  const [monacoReady, setMonacoReady] = useState(false);
  const [loadFailed, setLoadFailed] = useState(false);

  useEffect(() => {
    onChangeRef.current = onChange;
  }, [onChange]);

  useEffect(() => {
    let disposed = false;
    let subscription: { dispose: () => void } | null = null;

    const loadEditor = async () => {
      try {
        const moduleName = "monaco-editor";
        const monaco = await import(/* @vite-ignore */ moduleName);
        if (disposed || !hostRef.current) return;

        const editor = monaco.editor.create(hostRef.current, {
          value,
          language: "markdown",
          minimap: { enabled: false },
          wordWrap: "on",
          scrollBeyondLastLine: false,
          automaticLayout: true,
          fontSize: 13,
          lineHeight: 20,
          padding: { top: 12, bottom: 12 },
          theme: "vs-dark",
        });

        editorRef.current = editor;
        subscription = editor.onDidChangeModelContent(() => {
          onChangeRef.current(editor.getValue());
        });
        setMonacoReady(true);
      } catch {
        if (!disposed) setLoadFailed(true);
      }
    };

    loadEditor();

    return () => {
      disposed = true;
      subscription?.dispose();
      editorRef.current?.dispose?.();
      editorRef.current = null;
    };
  }, []);

  useEffect(() => {
    const editor = editorRef.current;
    if (!editor) return;
    if (editor.getValue() !== value) {
      editor.setValue(value);
    }
  }, [value]);

  const sharedStyle = {
    minHeight,
    background: "var(--th-input-bg)",
    border: "1px solid var(--th-input-border)",
    color: "var(--th-text-primary)",
  };

  if (loadFailed) {
    return (
      <textarea
        aria-label={label}
        value={value}
        onChange={(event) => onChange(event.target.value)}
        className="w-full resize-y rounded-2xl px-3 py-3 font-mono text-xs leading-5 outline-none"
        style={sharedStyle}
      />
    );
  }

  return (
    <div
      aria-label={label}
      className="relative overflow-hidden rounded-2xl"
      style={sharedStyle}
    >
      {!monacoReady && (
        <textarea
          value={value}
          onChange={(event) => onChange(event.target.value)}
          className="absolute inset-0 h-full w-full resize-none bg-transparent px-3 py-3 font-mono text-xs leading-5 outline-none"
          style={{ color: "var(--th-text-primary)" }}
        />
      )}
      <div ref={hostRef} className="h-full w-full" style={{ minHeight }} />
    </div>
  );
}
