// Every pattern that reads Claude Code's on-screen text lives here. Pane text
// is a last-resort fallback: hooks and the transcript are the sources of
// truth, and these patterns only cover what neither reports (the choices in a
// permission prompt, the workspace trust prompt, first-run and logged-out
// screens). They depend on UI wording, so they are pinned to one version.
export const PATTERNS_CLAUDE_VERSION = "2.1.322";

// eslint-disable-next-line no-control-regex
const ANSI = /\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-_]/g;

export function stripAnsi(text) {
  return text.replace(ANSI, "");
}

const OPTION_LINE = /^\s*(❯\s*)?(\d+)\.\s+(.+?)\s*$/;

/**
 * Find a numbered permission prompt at the bottom of the pane.
 * Returns { question, options: [{ key, text, selected }] } or null.
 */
export function findPermissionPrompt(paneText) {
  const lines = stripAnsi(paneText).split("\n");
  const footer = lines.findLastIndex((line) => /Esc to cancel/.test(line));
  if (footer < 0) return null;
  const options = [];
  let index = footer - 1;
  while (index >= 0 && lines[index].trim() === "") index--;
  for (; index >= 0; index--) {
    const match = OPTION_LINE.exec(lines[index]);
    if (!match) break;
    options.unshift({ key: match[2], text: match[3], selected: Boolean(match[1]) });
  }
  if (options.length < 2) return null;
  const question = index >= 0 ? lines[index].trim() : "";
  return { question, options };
}

/**
 * Pick the prompt option for a Codex approval decision by its text, never by
 * its position. Returns the option, or null when no option fits.
 */
export function chooseOption(options, decision) {
  const plainYes = options.find((option) => /^Yes$/i.test(option.text));
  if (decision === "accept") return plainYes ?? null;
  if (decision === "acceptForSession") {
    const broader = options.find(
      (option) =>
        /^Yes,/i.test(option.text) &&
        /don.t ask again|always allow|for this session|during this session/i.test(option.text) &&
        !/auto mode/i.test(option.text),
    );
    // No session-wide choice on offer: fall back to the narrower "Yes".
    return broader ?? plainYes ?? null;
  }
  if (decision === "decline") return options.find((option) => /^No\b/i.test(option.text)) ?? null;
  return null;
}

/** Workspace trust prompt shown before a session starts in a new folder. */
export function findTrustPrompt(paneText) {
  const lines = stripAnsi(paneText).split("\n");
  if (!lines.some((line) => /Is this a project you created or one you trust|Do you trust the files in this folder/.test(line))) {
    return null;
  }
  const choices = lines
    .map((line) => /^\s*(❯\s*)?(Yes, I trust this folder|No, exit)\s*$/.exec(line))
    .filter(Boolean)
    .map((match) => ({ text: match[2], selected: Boolean(match[1]) }));
  if (choices.length !== 2) return null;
  return { choices };
}

/** First-run or login screens that need a human at a real terminal. */
export function findBlockingScreen(paneText) {
  const text = stripAnsi(paneText);
  if (/Choose the text style that looks best|Press Enter to continue…|Security notes:/.test(text)) {
    return "Claude Code is showing its first-run setup. Run `claude` once in a terminal to finish it, then retry.";
  }
  if (/Select login method|Please run \/login|Invalid API key|OAuth token (?:has )?expired|Not logged in/i.test(text)) {
    return "Claude Code is not logged in. Run `claude` in a terminal and log in (or export ANTHROPIC_API_KEY before starting the bridge), then retry.";
  }
  return null;
}

/** The interrupted marker Claude Code prints after Escape. */
export function showsInterrupted(paneText) {
  return /Interrupted · What should Claude do instead\?/.test(stripAnsi(paneText));
}
