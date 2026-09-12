/**
 * OpenCode dark palette, markdown scopes, and split border adapted under MIT.
 * Copyright (c) 2025 opencode. Exact sources and full license: ../UPSTREAM.md.
 * NOSH uses the imperative @opentui/core renderer; no OpenCode runtime is loaded.
 */
import { SyntaxStyle } from '@opentui/core';

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

/** NOSH artwork, not an OpenCode wordmark. Each glyph is five terminal cells. */
export const NOSH_LOGO = [
  '███  ██  █████  ██████ ██   ██',
  '████ ██ ██   ██ ██     ██   ██',
  '██ ████ ██   ██ █████  ███████',
  '██  ███ ██   ██     ██ ██   ██',
  '██   ██  █████  ██████ ██   ██',
].join('\n');
