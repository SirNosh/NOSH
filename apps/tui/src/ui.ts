/**
 * OpenCode home/session/prompt layout adapted for NOSH's research daemon.
 * Copyright (c) 2025 opencode. MIT license, pinned sources: ../UPSTREAM.md.
 */
import { createCliRenderer, BoxRenderable, TextRenderable, ScrollBoxRenderable,
  CliRenderEvents, TextAttributes, type CliRenderer, type KeyEvent } from '@opentui/core';
import { DaemonClient, type TuiConfig } from './client.js';
import { Controller, HELP, safeText } from './controller.js';
import { createDialogs } from './dialog.js';
import { slashQuery } from './palette.js';
import { ComposerRenderable } from './composer.js';
import { createMarkdownStyle, NOSH_LOGO, splitBorder, theme } from './theme.js';
import { createTranscript, createInspector, createResearchSidebar, text } from './visual.js';

function clip(value: string, width: number): string {
  const clean = safeText(value).replace(/\s+/g, ' ');
  return clean.length > width ? clean.slice(0, Math.max(0, width - 1)) + '…' : clean;
}
function basename(value: string): string { return value.replace(/[/\\]+$/, '').split(/[/\\]/).pop() || value; }

/** Imperative native scene; keep render/input/scroll available for smoke tests. */
export function createWorkspace(renderer: CliRenderer, controller: Controller, quit: () => void) {
  const syntax = createMarkdownStyle();
  let disposed = false;
  let controlledChange = false;
  let slashDismissed = false;
  let previousView = '';
  let previousProject = '';
  let showSidebar = true;
  const history: string[] = [];
  let historyIndex = 0;
  let historyDraft = '';
  const shell = new BoxRenderable(renderer, { id: 'workspace', width: '100%', height: '100%',
    flexDirection: 'column', backgroundColor: theme.background });
  const body = new BoxRenderable(renderer, { id: 'workspace-body', width: '100%', flexGrow: 1,
    minHeight: 0, flexDirection: 'row' });
  const main = new BoxRenderable(renderer, { id: 'main', flexGrow: 1, minWidth: 0,
    height: '100%', paddingX: 2, flexDirection: 'column' });
  const header = new BoxRenderable(renderer, { id: 'session-header', height: 3, flexShrink: 0,
    flexDirection: 'column', paddingTop: 1 });
  const heading = text(renderer, 'session-title', '', theme.text); heading.height = 1;
  heading.attributes = TextAttributes.BOLD;
  const context = text(renderer, 'context', '', theme.muted); context.height = 1;
  header.add(heading); header.add(context);
  const topSpace = new BoxRenderable(renderer, { id: 'home-space-top', flexGrow: 1, minHeight: 0 });
  const hero = new BoxRenderable(renderer, { id: 'home-hero', alignSelf: 'center', alignItems: 'center',
    width: '100%', maxWidth: 75, flexShrink: 0, marginBottom: 2, flexDirection: 'column' });
  const logo = new TextRenderable(renderer, { id: 'brand', content: NOSH_LOGO, fg: theme.text,
    height: 5, width: 29, flexShrink: 0, wrapMode: 'none', selectable: false });
  const wordmark = new TextRenderable(renderer, { id: 'wordmark', content: 'N O S H', fg: theme.accent,
    height: 1, width: 7, visible: false, flexShrink: 0 });
  const tagline = new TextRenderable(renderer, { id: 'tagline', content: 'Your research. A shared direction.',
    height: 1, width: 34, marginTop: 1, fg: theme.muted, selectable: false });
  hero.add(logo); hero.add(wordmark); hero.add(tagline);
  const scroll = new ScrollBoxRenderable(renderer, { id: 'conversation', flexGrow: 1, minHeight: 0,
    width: '100%', paddingTop: 1, stickyScroll: true, stickyStart: 'bottom', scrollX: false,
    verticalScrollbarOptions: { visible: false }, horizontalScrollbarOptions: { visible: false },
    contentOptions: { width: '100%', flexDirection: 'column' } });
  const transcript = createTranscript(renderer, syntax);
  const inspector = createInspector(renderer);
  scroll.add(transcript.root); scroll.add(inspector.root);
  const promptHost = new BoxRenderable(renderer, { id: 'prompt-host', width: '100%', alignSelf: 'center',
    flexShrink: 0, flexDirection: 'column', paddingTop: 1 });
  const composer = new BoxRenderable(renderer, { id: 'composer-box', width: '100%', flexShrink: 0,
    flexDirection: 'column', backgroundColor: theme.element, paddingLeft: 2, paddingRight: 2,
    paddingTop: 1, paddingBottom: 1, border: ['left'], customBorderChars: splitBorder, borderColor: theme.accent });
  const input = new ComposerRenderable(renderer, { id: 'composer', width: '100%',
    placeholder: 'Ask NOSH… what should we investigate?', placeholderColor: theme.muted,
    textColor: theme.text, focusedTextColor: theme.text, backgroundColor: theme.element,
    focusedBackgroundColor: theme.element, cursorColor: theme.accent,
    onMouseDown: () => { if (!dialogs.isOpen) input.focus(); },
  });
  const modelLine = new BoxRenderable(renderer, { id: 'model-line', width: '100%', flexDirection: 'row',
    marginTop: 1, height: 1, flexShrink: 0, gap: 1 });
  const role = new TextRenderable(renderer, { id: 'role', content: 'Research', fg: theme.accent,
    flexShrink: 0, height: 1 });
  const model = new TextRenderable(renderer, { id: 'model', content: 'Default model', fg: theme.text,
    flexShrink: 1, minWidth: 0, height: 1, onMouseDown: () => { void dialogs.openModels(); } });
  const thinking = new TextRenderable(renderer, { id: 'thinking', content: '', fg: theme.warning,
    flexShrink: 0, height: 1, onMouseDown: () => { void dialogs.openThinking(); } });
  modelLine.add(role); modelLine.add(model); modelLine.add(thinking);
  composer.add(input); composer.add(modelLine);
  const hints = text(renderer, 'hints', '', theme.muted); hints.height = 1; hints.marginTop = 1;
  const status = text(renderer, 'status', '', theme.muted); status.maxHeight = 3;
  promptHost.add(composer); promptHost.add(hints); promptHost.add(status);
  const homeTip = new TextRenderable(renderer, { id: 'home-tip', content: '', fg: theme.muted,
    height: 1, maxWidth: 75, width: '100%', alignSelf: 'center', marginTop: 1, flexShrink: 0 });
  const bottomSpace = new BoxRenderable(renderer, { id: 'home-space-bottom', flexGrow: 1, minHeight: 0 });
  main.add(header); main.add(topSpace); main.add(hero); main.add(scroll); main.add(promptHost);
  main.add(homeTip); main.add(bottomSpace);

  // OpenCode's quiet, filled sidebar; NOSH state replaces LSP/MCP/file widgets.
  const sidebar = new BoxRenderable(renderer, { id: 'sidebar', width: 32, height: '100%', flexShrink: 0,
    backgroundColor: theme.panel, flexDirection: 'column', paddingX: 2, paddingY: 1 });
  const sideTitle = text(renderer, 'sidebar-title', 'Research workspace'); sideTitle.attributes = TextAttributes.BOLD;
  const sideProject = text(renderer, 'sidebar-project', '', theme.muted);
  const sideScroll = new ScrollBoxRenderable(renderer, { id: 'sidebar-scroll', width: '100%', flexGrow: 1,
    minHeight: 0, scrollX: false, verticalScrollbarOptions: { visible: false } });
  const research = createResearchSidebar(renderer); sideScroll.add(research.root);
  const sideFooter = text(renderer, 'sidebar-footer', '/status  graph    /jobs  work', theme.muted);
  sidebar.add(sideTitle); sidebar.add(sideProject); sidebar.add(sideScroll); sidebar.add(sideFooter);
  body.add(main); body.add(sidebar);
  const footer = new BoxRenderable(renderer, { id: 'footer', height: 1, flexShrink: 0, width: '100%',
    flexDirection: 'row', paddingX: 2, justifyContent: 'space-between' });
  const cwd = new TextRenderable(renderer, { id: 'cwd', content: '', fg: theme.muted,
    height: 1, flexGrow: 1, minWidth: 0, onMouseDown: () => { void dialogs.openProjects(); } });
  const version = new TextRenderable(renderer, { id: 'version', content: 'N O S H  0.1.0', fg: theme.muted,
    height: 1, flexShrink: 0 });
  footer.add(cwd); footer.add(version); shell.add(body); shell.add(footer); renderer.root.add(shell);

  function setComposer(value: string) {
    controlledChange = true; input.value = value; controlledChange = false;
    slashDismissed = false;
  }
  async function submit(value: string, propagateError = false): Promise<void> {
    if (disposed || !value.trim()) return;
    if (controller.busy) { if (propagateError) throw new Error('Wait for the current command'); return; }
    if (value.trim() === '/quit') { quit(); return; }
    const draft = input.value;
    try {
      const pending = controller.execute(value);
      render(); await pending;
      if (disposed) return;
      if (input.value === draft) setComposer('');
      if (history.at(-1) !== value) history.push(value);
      if (history.length > 100) history.shift();
      historyIndex = history.length;
      if (controller.pendingAction()) dialogs.openConfirmation();
    } catch (error) {
      // Dialogs must see failures; composer submissions keep the failed draft.
      if (propagateError) throw error;
    }
    finally { if (!disposed) render(); }
  }
  const dialogs = createDialogs(renderer, controller, {
    onClose: () => { if (!disposed) { slashDismissed = input.value.startsWith('/'); input.focus(); render(); } },
    onChange: () => { if (!disposed) render(); },
    submit: value => submit(value, true), setComposer, quit,
    theme: { bg: theme.background, panel: theme.panel, text: theme.text, muted: theme.muted,
      accent: theme.accent, border: theme.border, selected: theme.element, error: theme.error },
  });
  input.onSubmit = () => { if (!dialogs.isOpen) void submit(input.value); };
  input.onContentChange = () => {
    if (controlledChange || dialogs.isOpen || disposed) return;
    const value = input.value;
    if (!value) slashDismissed = false;
    const query = slashQuery(value);
    if (!slashDismissed && query !== undefined) dialogs.openSlash(query);
    render();
  };

  function render() {
    if (disposed || renderer.isDestroyed) return;
    const width = renderer.width;
    const compact = renderer.height < 24;
    const project = controller.projects.find(p => p.projectId === controller.projectId);
    const entries = controller.transcriptEntries();
    const activity = controller.activity();
    const home = controller.view === 'chat' && entries.length === 0 && !activity.working;
    const changedView = previousView !== controller.view || previousProject !== controller.projectId;
    sidebar.visible = !home && width >= 80 && showSidebar;
    sidebar.width = width >= 140 ? 38 : width >= 100 ? 32 : 28;
    header.visible = !home; scroll.visible = !home;
    hero.visible = home; topSpace.visible = home; bottomSpace.visible = home; homeTip.visible = home && !compact;
    logo.visible = renderer.height >= 20; wordmark.visible = renderer.height < 20;
    tagline.visible = renderer.height >= 20; hero.marginBottom = compact ? 1 : 2;
    promptHost.maxWidth = home ? 75 : undefined;
    composer.paddingTop = compact && !home ? 0 : 1;
    composer.paddingBottom = compact && !home ? 0 : 1;
    input.maxHeight = compact ? 3 : Math.max(3, Math.min(6, Math.floor(renderer.height / 4)));
    const mainWidth = Math.max(12, width - (sidebar.visible ? sidebar.width : 0) - 4);
    const topic = entries.find(entry => entry.kind === 'user')?.text;
    heading.content = clip(`NOSH  /  ${controller.view === 'chat' ? topic || basename(project?.repositoryRoot || 'Research workspace') : controller.view.toUpperCase()}`, mainWidth);
    context.content = clip(`${basename(project?.repositoryRoot || 'No project')}  ·  ${controller.view === 'chat' ? 'Research conversation' : 'Esc conversation'}  ·  ctrl+p commands`, mainWidth);
    const roleName = activity.role || 'Research';
    role.content = roleName.charAt(0).toUpperCase() + roleName.slice(1);
    const modelName = controller.selection ? controller.selection.model.id : activity.model;
    const level = controller.selection?.thinkingLevel || activity.thinkingLevel;
    model.content = clip(`· ${modelName || 'Default model'}`, Math.max(12, Math.min(home ? 42 : 38, mainWidth - roleName.length - 17)));
    thinking.content = level ? `· ${level}` : '';
    composer.borderColor = controller.error ? theme.error : controller.busy || activity.working ? theme.secondary : theme.accent;
    hints.content = mainWidth < 62 ? 'enter send  ⇧enter newline  ctrl+p commands'
      : 'enter send   shift+enter newline   ctrl+p commands   tab thinking';
    const working = controller.busy || activity.working;
    const spinner = ['▪▫▫', '▫▪▫', '▫▫▪'][Math.floor(Date.now() / 180) % 3];
    status.fg = controller.error ? theme.error : working ? theme.secondary : theme.muted;
    status.content = safeText(controller.error ? `! ${controller.error}` : working ? `${spinner} ${activity.label || 'Working…'}  ·  daemon continues if you detach`
      : controller.connection !== 'connected' ? `○ ${controller.connection}`
      : controller.pendingAction() ? '? Action staged · /confirm to review · /discard to dismiss'
      : entries.at(-1)?.kind === 'receipt' ? `✓ ${clip(entries.at(-1)!.title, mainWidth - 4)}` : '');
    status.visible = !!status.plainText;
    homeTip.content = project ? 'Start with a question, a paper, or an experiment.' : 'ctrl+o choose project    /open start research';
    cwd.content = clip(project?.repositoryRoot || process.cwd(), Math.max(8, width - 24));
    sideProject.content = clip(project ? basename(project.repositoryRoot) : 'No project selected', sidebar.width - 4);
    transcript.root.visible = controller.view === 'chat'; inspector.root.visible = controller.view !== 'chat';
    transcript.update(entries);
    if (controller.view !== 'chat') inspector.update(controller.inspectorRows(), controller.view === 'help' ? HELP : controller.detail);
    research.update(controller.sidebarRows());
    if (changedView) {
      scroll.stickyScroll = controller.view === 'chat';
      scroll.stickyStart = controller.view === 'chat' ? 'bottom' : 'top';
      scroll.scrollTo(controller.view === 'chat' ? scroll.scrollHeight : 0);
      previousView = controller.view; previousProject = controller.projectId;
    }
  }
  const handleKey = (key: KeyEvent) => {
    if (key.ctrl && key.name === 'c') { key.preventDefault(); quit(); return; }
    if (dialogs.handleKey(key)) return;
    const take = () => { key.preventDefault(); key.stopPropagation(); };
    if (key.ctrl && key.name === 'p') { take(); dialogs.openPalette(); return; }
    if (key.ctrl && key.name === 'o') { take(); void dialogs.openProjects(); return; }
    if (key.ctrl && key.name === 'm') { take(); void dialogs.openModels(); return; }
    if (key.name === 'tab') { take(); void dialogs.openThinking(); return; }
    if (key.ctrl && key.name === 'b') { take(); showSidebar = !showSidebar; render(); return; }
    if (key.name === 'escape') {
      take();
      if (input.value) setComposer('');
      else if (controller.view !== 'chat') void submit('/chat');
      render(); return;
    }
    if (key.name === 'pageup') { take(); scroll.scrollBy(-Math.max(3, scroll.height - 3)); return; }
    if (key.name === 'pagedown') { take(); scroll.scrollBy(Math.max(3, scroll.height - 3)); return; }
    if (key.ctrl && key.name === 'home') { take(); scroll.scrollTo(0); return; }
    if (key.ctrl && key.name === 'end') { take(); scroll.scrollTo(scroll.scrollHeight); return; }
    if (key.name === 'up' && input.logicalCursor.row === 0 && history.length) {
      take(); if (historyIndex === history.length) historyDraft = input.value;
      historyIndex = Math.max(0, historyIndex - 1); setComposer(history[historyIndex]!); return;
    }
    if (key.name === 'down' && historyIndex < history.length && input.logicalCursor.row >= input.lineCount - 1) {
      take(); historyIndex++; setComposer(history[historyIndex] ?? historyDraft); return;
    }
  };
  renderer.keyInput.on('keypress', handleKey);
  renderer.on(CliRenderEvents.RESIZE, render);
  function dispose() {
    if (disposed) return;
    disposed = true;
    renderer.keyInput.off('keypress', handleKey); renderer.off(CliRenderEvents.RESIZE, render);
    dialogs.destroy(); shell.destroyRecursively(); syntax.destroy();
  }
  shell.on('destroyed', () => {
    if (!disposed) {
      disposed = true; renderer.keyInput.off('keypress', handleKey); renderer.off(CliRenderEvents.RESIZE, render);
      dialogs.destroy(); syntax.destroy();
    }
  });
  input.focus(); render();
  return { render, input, scroll, dialogs, dispose };
}

export async function runTui(config: TuiConfig): Promise<void> {
  if (!process.stdin.isTTY || !process.stdout.isTTY) throw new Error('NOSH TUI needs an interactive terminal');
  const client = new DaemonClient(config);
  const controller = new Controller(client, config.currentProjectId);
  try { await controller.initialize(); } catch (error) { client.close(); throw error; }
  let finish!: () => void;
  const done = new Promise<void>(resolve => { finish = resolve; });
  let closed = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let frameTimer: ReturnType<typeof setTimeout> | undefined;
  let renderer: CliRenderer | undefined;
  let workspace: ReturnType<typeof createWorkspace> | undefined;
  let unsubscribe: (() => void) | undefined;
  const cleanup = () => {
    if (closed) return;
    closed = true; clearTimeout(timer); clearTimeout(frameTimer); unsubscribe?.();
    controller.dispose(); client.close(); workspace?.dispose();
    process.off('SIGINT', close); process.off('SIGTERM', close); finish();
  };
  const close = () => { cleanup(); renderer?.destroy(); };
  try {
    renderer = await createCliRenderer({ exitOnCtrlC: false, exitSignals: [], onDestroy: cleanup,
      targetFps: 20, maxFps: 30, backgroundColor: theme.background, consoleMode: 'disabled' });
    workspace = createWorkspace(renderer, controller, close);
    const scheduleRender = () => {
      if (closed || frameTimer) return;
      frameTimer = setTimeout(() => { frameTimer = undefined; if (!closed) workspace?.render(); }, 50);
    };
    unsubscribe = controller.subscribe(scheduleRender);
    process.on('SIGINT', close); process.on('SIGTERM', close);
    const tick = async () => {
      await controller.poll();
      if (closed) return;
      scheduleRender();
      // One bounded reconciliation page per tick. The controller owns live deltas.
      timer = setTimeout(() => { void tick(); }, controller.connection === 'connected' ? 1000 : 5000);
    };
    void tick(); await done;
  } finally { close(); }
}
