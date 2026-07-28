import { describe, expect, it } from "vitest";

import { aggregate, answerOf, readQuestions, type Question } from "./Approval";

/**
 * The `ask_user` answer is a contract with the model, not a display detail: the
 * harness turns it into one note, and the TUI has been producing that exact
 * text for every session the model has ever seen. Two frontends spelling one
 * answer two ways is two definitions of the same thing, so the format is
 * pinned here against `QuestionPage::answer` / `submit_or_advance` in
 * `crates/tcode-tui/src/approval.rs`.
 */

const parse = (input: unknown): Question[] => {
  const questions = readQuestions(input);
  if (!questions) throw new Error("expected a question form");
  return questions;
};

const ask = {
  questions: [
    {
      question: "How should the clock be injected?",
      options: [
        { label: "Trait object", description: "swappable" },
        { label: "Generic parameter", description: "monomorphized" },
      ],
    },
  ],
};

describe("reading the call", () => {
  it("appends the escape hatch the model never writes", () => {
    const [question] = parse(ask);
    expect(question.options.map((option) => option.label)).toEqual([
      "Trait object",
      "Generic parameter",
      "Something else",
    ]);
  });

  it("tolerates the legacy single-question shape and bare string options", () => {
    const [question] = parse({ question: "Which one?", options: ["a", "b"] });
    expect(question.question).toBe("Which one?");
    expect(question.options[0]).toEqual({ label: "a", description: "", preview: null });
  });

  it("is not a question form when the call is anything else", () => {
    expect(readQuestions({ file_path: "src/main.rs", old_string: "a" })).toBeNull();
    expect(readQuestions({ questions: [] })).toBeNull();
    expect(readQuestions(null)).toBeNull();
  });
});

describe("the answer", () => {
  it("is the label alone for a plain pick", () => {
    const [question] = parse(ask);
    expect(answerOf(question, [0], "")).toBe("Trait object");
  });

  it("carries a note after an em dash", () => {
    const [question] = parse(ask);
    expect(answerOf(question, [0], " keep it private ")).toBe(
      "Trait object — keep it private",
    );
  });

  it("joins a multi-select with commas", () => {
    const [question] = parse(ask);
    expect(answerOf(question, [0, 1], "")).toBe("Trait object, Generic parameter");
  });

  it("reports only the typed text when the menu missed the point", () => {
    const [question] = parse(ask);
    // The rejected label must not ride along: the model would read it as the
    // user's choice.
    expect(answerOf(question, [2], "neither — pass a Duration in")).toBe(
      "neither — pass a Duration in",
    );
  });

  it("numbers several questions and names each one", () => {
    const questions = parse({
      questions: [
        { question: "Which shape?", options: [{ label: "Trait" }, { label: "Generic" }] },
        { question: "Which tests?", options: [{ label: "Backoff" }, { label: "Cancel" }] },
      ],
    });
    expect(aggregate(questions, ["Trait", "Backoff, Cancel"])).toBe(
      "1. Which shape? → Trait\n2. Which tests? → Backoff, Cancel",
    );
  });

  it("sends one question's answer with no numbering", () => {
    const questions = parse(ask);
    expect(aggregate(questions, ["Trait object"])).toBe("Trait object");
  });
});
