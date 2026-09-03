/**
 * Strip the escape sequences the shell writes so the transcript can be
 * matched and shown as plain text.
 *
 * The prompt is coloured, cargo's output is coloured, and the line editor
 * redraws with `\x1b[K` and cursor moves — none of which a caller of the
 * protocol should have to know about. This covers CSI and OSC, which is all
 * rubrc's shell emits; a lone `\r` is left alone, since cargo's progress
 * lines depend on it.
 */
const CSI = /\x1b\[[0-?]*[ -/]*[@-~]/g;
const OSC = /\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g;

export const stripAnsi = (text: string): string => text.replace(OSC, "").replace(CSI, "");
