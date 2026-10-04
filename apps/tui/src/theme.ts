/**
 * OpenCode dark palette, markdown scopes, and split border adapted under MIT.
 * Copyright (c) 2025 opencode. Exact sources and full license: ../UPSTREAM.md.
 * NOSH uses the imperative @opentui/core renderer; no OpenCode runtime is loaded.
 */
import { SyntaxStyle, StyledText, fg, type TextChunk } from '@opentui/core';

export const theme = {
  background: '#0a0a0a', panel: '#141414', element: '#1e1e1e',
  border: '#3c3c3c', text: '#eeeeee', muted: '#909090',
  accent: '#fab283', secondary: '#5c9cf5', purple: '#9d7cd8',
  success: '#7fd88f', warning: '#f5a742', error: '#e06c75', cyan: '#56b6c2',
} as const;

// OpenCode ui/border.ts SplitBorder: one uninterrupted accent, not a boxed form.
export const splitBorder = {
  topLeft: '', bottomLeft: '', vertical: '┃', topRight: '', bottomRight: '',
  horizontal: ' ', bottomT: '', topT: '', cross: '', leftT: '', rightT: '',
};

/** Own one native syntax style per workspace and release it on teardown. */
export function createMarkdownStyle(): SyntaxStyle {
  return SyntaxStyle.fromStyles({
    default: { fg: theme.text },
    'markup.heading': { fg: theme.purple, bold: true },
    'markup.heading.1': { fg: theme.purple, bold: true },
    'markup.heading.2': { fg: theme.purple, bold: true },
    'markup.heading.3': { fg: theme.purple, bold: true },
    'markup.strong': { fg: theme.warning, bold: true },
    'markup.bold': { fg: theme.warning, bold: true },
    'markup.italic': { fg: '#e5c07b', italic: true },
    'markup.list': { fg: theme.accent },
    'markup.quote': { fg: '#e5c07b', italic: true },
    'markup.raw': { fg: theme.success },
    'markup.raw.block': { fg: theme.text },
    'markup.raw.inline': { fg: theme.success, bg: theme.element },
    'markup.link': { fg: theme.accent, underline: true },
    'markup.link.label': { fg: theme.cyan, underline: true },
    'markup.link.url': { fg: theme.accent, underline: true },
    label: { fg: theme.cyan }, conceal: { fg: theme.muted },
    comment: { fg: theme.muted, italic: true }, keyword: { fg: theme.purple },
    string: { fg: theme.success }, number: { fg: theme.warning },
    function: { fg: theme.accent }, type: { fg: '#e5c07b' },
    operator: { fg: theme.cyan }, punctuation: { fg: theme.text },
  });
}

/** NOSH artwork (block wordmark with outline strokes), not an OpenCode wordmark. */
const NOSH_LOGO_LINES = [
  '███╗   ██╗ ██████╗ ███████╗██╗  ██╗',
  '████╗  ██║██╔═══██╗██╔════╝██║  ██║',
  '██╔██╗ ██║██║   ██║███████╗███████║',
  '██║╚██╗██║██║   ██║╚════██║██╔══██║',
  '██║ ╚████║╚██████╔╝███████║██║  ██║',
  '╚═╝  ╚═══╝ ╚═════╝ ╚══════╝╚═╝  ╚═╝',
];

function hex(value: string): [number, number, number] { return [1, 3, 5].map((index) => parseInt(value.slice(index, index + 2), 16)) as [number, number, number]; }
function mix(from: string, to: string, amount: number): string {
  const a = hex(from), b = hex(to);
  return '#' + a.map((channel, index) => Math.round(channel + (b[index]! - channel) * amount).toString(16).padStart(2, '0')).join('');
}
/** Left-to-right peach-to-violet gradient; outline strokes take a dimmer shade of the same hue. */
function gradientText(lines: string[]): StyledText {
  const width = Math.max(...lines.map((line) => line.length));
  const chunks: TextChunk[] = [];
  lines.forEach((line, row) => {
    let run = ''; let runColor = theme.background as string;
    const flush = () => { if (run) chunks.push(fg(runColor)(run)); run = ''; };
    for (let column = 0; column < line.length; column++) {
      const char = line[column]!;
      const base = mix(theme.accent, theme.purple, column / Math.max(1, width - 1));
      const color = char === ' ' ? theme.background : char === '█' || /[A-Z]/.test(char) ? base : mix(base, theme.background, 0.55);
      if (color !== runColor) { flush(); runColor = color; }
      run += char;
    }
    flush();
    if (row < lines.length - 1) chunks.push(fg(theme.text)('\n'));
  });
  return new StyledText(chunks);
}
export function logoText(): StyledText { return gradientText(NOSH_LOGO_LINES); }
/** One-line wordmark for short terminals. */
export function compactLogoText(): StyledText { return gradientText(['N O S H']); }
/** "key label" pairs: accent keys, muted labels. */
export function keyHints(pairs: Array<[string, string]>): StyledText {
  return new StyledText(pairs.flatMap(([key, label], index) => [fg(theme.accent)(key), fg(theme.muted)(` ${label}${index < pairs.length - 1 ? '   ' : ''}`)]));
}
