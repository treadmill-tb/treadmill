import {
  defaultKeymap,
  history,
  historyKeymap,
  indentWithTab,
} from "@codemirror/commands";
import { json } from "@codemirror/lang-json";
import {
  bracketMatching,
  defaultHighlightStyle,
  indentUnit,
  syntaxHighlighting,
} from "@codemirror/language";
import { EditorState } from "@codemirror/state";
import { EditorView, keymap, lineNumbers } from "@codemirror/view";
import { useEffect, useRef } from "react";

const theme = EditorView.theme({
  "&": {
    border: "1px solid var(--border-strong)",
    background: "var(--surface)",
    color: "var(--fg)",
    fontSize: "0.85em",
  },
  "&.cm-focused": { outline: "1px solid var(--accent)" },
  ".cm-scroller": {
    fontFamily: "var(--mono)",
    minHeight: "20rem",
    maxHeight: "36rem",
  },
  ".cm-gutters": {
    background: "var(--bg)",
    color: "var(--fg-faint)",
    border: "none",
    borderRight: "1px solid var(--border)",
  },
});

export function JsonEditor({
  initialValue,
  onChange,
}: {
  initialValue: string;
  onChange: (value: string) => void;
}) {
  const mount = useRef<HTMLDivElement>(null);
  const latest = useRef(onChange);
  useEffect(() => {
    latest.current = onChange;
  });

  useEffect(() => {
    const view = new EditorView({
      parent: mount.current ?? undefined,
      state: EditorState.create({
        doc: initialValue,
        extensions: [
          lineNumbers(),
          history(),
          keymap.of([...defaultKeymap, ...historyKeymap, indentWithTab]),
          json(),
          syntaxHighlighting(defaultHighlightStyle, { fallback: true }),
          bracketMatching(),
          indentUnit.of("  "),
          EditorView.lineWrapping,
          theme,
          EditorView.updateListener.of((update) => {
            if (update.docChanged) {
              latest.current(update.state.doc.toString());
            }
          }),
        ],
      }),
    });
    return () => view.destroy();
  }, [initialValue]);

  return <div ref={mount} />;
}
