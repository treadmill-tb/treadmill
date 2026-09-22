import {
  defaultKeymap,
  history,
  historyKeymap,
  indentWithTab,
} from "@codemirror/commands";
import { json } from "@codemirror/lang-json";
import {
  bracketMatching,
  HighlightStyle,
  indentUnit,
  syntaxHighlighting,
} from "@codemirror/language";
import { EditorState } from "@codemirror/state";
import { EditorView, keymap, lineNumbers } from "@codemirror/view";
import { tags } from "@lezer/highlight";
import { useEffect, useRef } from "react";

const highlight = HighlightStyle.define([
  { tag: tags.propertyName, color: "var(--active)" },
  { tag: tags.string, color: "var(--ok)" },
  { tag: tags.number, color: "var(--warn)" },
  { tag: [tags.bool, tags.null], color: "var(--danger)" },
]);

const theme = EditorView.theme({
  "&": {
    border:
      "var(--pico-border-width) solid var(--pico-form-element-border-color)",
    borderRadius: "var(--pico-border-radius)",
    overflow: "hidden",
    background: "var(--pico-form-element-background-color)",
    color: "var(--pico-form-element-color)",
    fontSize: "var(--text-sm)",
  },
  "&.cm-focused": {
    outline: "none",
    borderColor: "var(--pico-form-element-active-border-color)",
    boxShadow:
      "0 0 0 var(--pico-outline-width) var(--pico-form-element-focus-color)",
  },
  ".cm-scroller": {
    fontFamily: "var(--pico-font-family-monospace)",
    minHeight: "20rem",
    maxHeight: "36rem",
  },
  ".cm-gutters": {
    background: "var(--pico-code-background-color)",
    color: "var(--pico-muted-color)",
    border: "none",
    borderRight:
      "var(--pico-border-width) solid var(--pico-muted-border-color)",
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
          syntaxHighlighting(highlight),
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
