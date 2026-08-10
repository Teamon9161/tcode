import { closeBrackets, closeBracketsKeymap } from "@codemirror/autocomplete";
import {
  defaultKeymap,
  history,
  historyField,
  historyKeymap,
  indentWithTab,
} from "@codemirror/commands";
import {
  bracketMatching,
  indentOnInput,
  indentUnit,
  syntaxHighlighting,
} from "@codemirror/language";
import { EditorState, type Extension } from "@codemirror/state";
import {
  drawSelection,
  dropCursor,
  EditorView,
  highlightActiveLine,
  highlightActiveLineGutter,
  highlightSpecialChars,
  keymap,
  lineNumbers,
  rectangularSelection,
} from "@codemirror/view";
import { highlightSelectionMatches, search, searchKeymap } from "@codemirror/search";
import { classHighlighter } from "@lezer/highlight";

/**
 * The editor's stable, theme-free behavior. CodeMirror's aggregate
 * `basicSetup` also installs its default highlight style, which carries a
 * palette of its own. Keeping the list explicit lets syntax arrive only as
 * stable `tok-*` classes whose colors remain owned by the app theme.
 *
 * `indentWithTab` deliberately comes after the standard keymaps. CodeMirror's
 * view still reserves its built-in Escape-then-Tab path, which temporarily
 * gives Tab back to the browser so a keyboard user can leave the editor.
 */
export function workspaceEditorExtensions(readOnly: boolean): Extension[] {
  return [
    lineNumbers(),
    highlightActiveLineGutter(),
    highlightSpecialChars(),
    history(),
    drawSelection(),
    dropCursor(),
    rectangularSelection(),
    highlightActiveLine(),
    indentOnInput(),
    bracketMatching(),
    closeBrackets(),
    search(),
    highlightSelectionMatches(),
    syntaxHighlighting(classHighlighter),
    indentUnit.of("  "),
    EditorState.tabSize.of(2),
    EditorState.readOnly.of(readOnly),
    EditorView.editable.of(!readOnly),
    EditorView.contentAttributes.of({
      "aria-label": "Workspace file editor",
      spellcheck: "false",
      autocapitalize: "off",
      autocorrect: "off",
    }),
    keymap.of([
      ...closeBracketsKeymap,
      ...defaultKeymap,
      ...searchKeymap,
      ...historyKeymap,
      indentWithTab,
    ]),
  ];
}

/** Rebuild a cached state with this component instance's fresh extensions.
 * The official history field is serialized explicitly, preserving selection
 * and undo/redo without retaining callbacks or compartments from an unmounted
 * EditorView. */
export function restoreWorkspaceEditorState(
  cached: EditorState,
  extensions: Extension,
): EditorState {
  return EditorState.fromJSON(
    cached.toJSON({ history: historyField }),
    { extensions },
    { history: historyField },
  );
}
