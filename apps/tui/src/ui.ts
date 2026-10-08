/**
 * OpenCode home/session/prompt layout adapted for NOSH's research daemon.
 * Copyright (c) 2025 opencode. MIT license, pinned sources: ../UPSTREAM.md.
 */
import { writeFileSync } from 'node:fs';
import { createCliRenderer, BoxRenderable, TextRenderable, ScrollBoxRenderable, StyledText, fg,
  CliRenderEvents, TextAttributes, type CliRenderer, type KeyEvent } from '@opentui/core';
import { DaemonClient, type TuiConfig } from './client.js';
import { Controller, HELP, safeText } from './controller.js';
import { createDialogs } from './dialog.js';
import { slashQuery } from './palette.js';
import { ComposerRenderable } from './composer.js';
import { compactLogoText, createMarkdownStyle, keyHints, logoText, splitBorder, theme } from './theme.js';
import { createTranscript, createInspector, createResearchMap, createResearchSidebar, text, type AttentionItem } from './visual.js';

// Empty inspector views explain the state instead of showing the raw response.
const EMPTY_VIEW: Record<string, string> = {
  status: 'No missions, directions, autoresearch or agents yet.\nKeep talking in the conversation to shape the project; research work is proposed for your approval.',
  jobs: 'No supervised jobs for this project.\nJobs started by research work appear here with their resources and output.',
  approvals: 'Nothing is waiting for approval.',
  projects: 'No registered projects. Press ctrl+o or use /new to open one.',
  models: 'No authenticated models. Configure provider authentication in Pi, then /refresh.',
  result: 'The action was applied. The daemon returned no records.',
  detail: 'The daemon returned no details for this record.',
};

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
  let escapeArmedAt = 0;
  let spinTimer: ReturnType<typeof setInterval> | undefined;
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
  const logo = new TextRenderable(renderer, { id: 'brand', content: logoText(), fg: theme.text,
    height: 6, width: 35, flexShrink: 0, wrapMode: 'none', selectable: false });
  const wordmark = new TextRenderable(renderer, { id: 'wordmark', content: compactLogoText(), fg: theme.accent,
    height: 1, width: 7, visible: false, flexShrink: 0 });
  const tagline = new TextRenderable(renderer, { id: 'tagline', content: 'autonomous research you can verify',
    height: 1, width: 34, marginTop: 1, fg: theme.muted, selectable: false });
  hero.add(logo); hero.add(wordmark); hero.add(tagline);
  const scroll = new ScrollBoxRenderable(renderer, { id: 'conversation', flexGrow: 1, minHeight: 0,
    width: '100%', paddingTop: 1, stickyScroll: true, stickyStart: 'bottom', scrollX: false,
    verticalScrollbarOptions: { visible: false }, horizontalScrollbarOptions: { visible: false },
    contentOptions: { width: '100%', flexDirection: 'column' } });
  const transcript = createTranscript(renderer, syntax);
  const inspector = createInspector(renderer);
  const researchMap = createResearchMap(renderer);
  // The conversation always opens with the NOSH mark, above the first message.
  const banner = new BoxRenderable(renderer, { id: 'session-banner', flexDirection: 'column', flexShrink: 0, height: 8, minHeight: 8, paddingLeft: 1, marginBottom: 1 });
  banner.add(new TextRenderable(renderer, { id: 'session-logo', content: logoText(), height: 6, minHeight: 6, width: 35, flexShrink: 0, wrapMode: 'none', selectable: false }));
  banner.add(new TextRenderable(renderer, { id: 'session-tagline', content: 'autonomous research you can verify', fg: theme.muted, height: 1, marginTop: 1, selectable: false }));
  scroll.add(banner); scroll.add(transcript.root); scroll.add(inspector.root); scroll.add(researchMap.root);
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
  const sideTitle = new TextRenderable(renderer, { id: 'sidebar-title', content: compactLogoText(), height: 1, flexShrink: 0 }); sideTitle.attributes = TextAttributes.BOLD;
  const sideSubtitle = text(renderer, 'sidebar-subtitle', 'research workspace', theme.muted);
  const sideProject = text(renderer, 'sidebar-project', '', theme.muted);
  const sideScroll = new ScrollBoxRenderable(renderer, { id: 'sidebar-scroll', width: '100%', flexGrow: 1,
    minHeight: 0, scrollX: false, verticalScrollbarOptions: { visible: false } });
  const research = createResearchSidebar(renderer); sideScroll.add(research.root);
  const sideFooter = text(renderer, 'sidebar-footer', 'ctrl+g map  ctrl+b hide', theme.muted);
  sidebar.add(sideTitle); sidebar.add(sideSubtitle); sidebar.add(sideProject); sidebar.add(sideScroll); sidebar.add(sideFooter);
  body.add(main); body.add(sidebar);
  const footer = new BoxRenderable(renderer, { id: 'footer', height: 1, flexShrink: 0, width: '100%',
    flexDirection: 'row', paddingX: 2, justifyContent: 'space-between' });
  const cwd = new TextRenderable(renderer, { id: 'cwd', content: '', fg: theme.muted,
    height: 1, flexGrow: 1, minWidth: 0, onMouseDown: () => { void dialogs.openProjects(); } });
  const connection = new TextRenderable(renderer, { id: 'connection', content: '', fg: theme.muted,
    height: 1, flexShrink: 0, marginRight: 2 });
  const version = new TextRenderable(renderer, { id: 'version', content: 'nosh 0.1.0', fg: theme.muted,
    height: 1, flexShrink: 0 });
  footer.add(cwd); footer.add(connection); footer.add(version); shell.add(body); shell.add(footer); renderer.root.add(shell);

  function setComposer(value: string) {
    controlledChange = true; input.value = value; controlledChange = false;
    // The content-change event can arrive after controlledChange resets; a recalled
    // or programmatic "/command" must not reopen slash search and capture Up/Down/Esc.
    slashDismissed = value.startsWith('/');
  }
  async function submit(value: string, propagateError = false): Promise<void> {
    if (disposed || !value.trim()) return;
    if (controller.busy) { if (propagateError) throw new Error('Wait for the current command'); return; }
    const command = value.trim();
    if (command === '/quit') { quit(); return; }
    // Form-backed commands open their dialog instead of failing as unknown.
    const dialog = { '/new': () => dialogs.openProjectForm(true), '/open': () => dialogs.openProjectForm(false),
      '/thinking': () => dialogs.openThinking() }[command];
    if (dialog && !propagateError) { setComposer(''); dialog(); return; }
    if (!command.startsWith('/') && !controller.projectId && !propagateError) { void dialogs.openProjects(); return; }
    const draft = input.value;
    try {
      const pending = controller.execute(value);
      render(); await pending;
      if (disposed) return;
      // Dialog submissions (e.g. opening a project) must not erase the user's draft.
      if (!propagateError && input.value === draft) setComposer('');
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
    controller.error = '';
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
    sideFooter.content = sidebar.width >= 38 ? 'ctrl+g map  ctrl+b hide  /jobs' : 'ctrl+g map  ctrl+b hide';
    header.visible = !home; scroll.visible = !home; banner.visible = controller.view === 'chat' && renderer.height >= 20;
    hero.visible = home; topSpace.visible = home; bottomSpace.visible = home; homeTip.visible = home && !compact;
    logo.visible = renderer.height >= 20; wordmark.visible = renderer.height < 20;
    tagline.visible = renderer.height >= 20; hero.marginBottom = compact ? 1 : 2;
    promptHost.maxWidth = home ? 75 : undefined;
    composer.paddingTop = compact && !home ? 0 : 1;
    composer.paddingBottom = compact && !home ? 0 : 1;
    input.maxHeight = compact ? 3 : Math.max(3, Math.min(6, Math.floor(renderer.height / 4)));
    const mainWidth = Math.max(12, width - (sidebar.visible ? sidebar.width : 0) - 4);
    const topic = entries.find(entry => entry.kind === 'user')?.text;
    heading.content = clip(`NOSH  /  ${controller.view === 'chat' ? topic || basename(project?.repositoryRoot || 'Research workspace') : controller.view === 'map' ? 'RESEARCH MAP' : controller.view.toUpperCase()}`, mainWidth);
    context.content = clip(`${basename(project?.repositoryRoot || 'No project')}  ·  ${controller.view === 'chat' ? 'Research conversation' : 'Esc conversation'}  ·  ctrl+p commands`, mainWidth);
    const roleName = (activity.role || 'Research').replaceAll('_', ' ');
    role.content = roleName.charAt(0).toUpperCase() + roleName.slice(1);
    const modelName = controller.selection ? controller.selection.model.id : activity.model;
    const level = controller.selection?.thinkingLevel || activity.thinkingLevel;
    const noModel = controller.modelsChecked && !controller.models.length;
    model.content = noModel ? '· no model connected — run nosh login in a terminal' : clip(`· ${modelName || 'Default model'}`, Math.max(12, (home ? 71 : mainWidth) - roleName.length - (level ? level.length + 8 : 6)));
    model.fg = noModel ? theme.warning : theme.text;
    thinking.content = level && !noModel ? `· ${level}` : '';
    composer.borderColor = controller.error ? theme.error : controller.busy || activity.working ? theme.secondary : theme.accent;
    hints.content = controller.view !== 'chat' ? keyHints([['esc', 'conversation'], ['pgup/pgdn', 'scroll'], ...(controller.view === 'map' ? [] : [['ctrl+g', 'map'] as [string, string]]), ['ctrl+p', 'commands']])
      : !project ? keyHints([['ctrl+o', 'choose a project'], ['ctrl+p', 'commands']])
      : mainWidth < 72 ? keyHints([['enter', 'send'], ['⇧enter', 'newline'], ['ctrl+p', 'commands']])
      : keyHints([['enter', 'send'], ['⇧enter', 'newline'], ['ctrl+p', 'commands'], ['ctrl+g', 'map'], ['f2', 'model'], ['ctrl+t', 'thinking']]);
    const working = controller.busy || activity.working;
    const spinner = ['▪▫▫', '▫▪▫', '▫▫▪'][Math.floor(Date.now() / 180) % 3];
    status.fg = controller.error ? theme.error : working ? theme.secondary : theme.muted;
    const armed = input.value && Date.now() - escapeArmedAt < 1500;
    status.content = safeText(controller.error ? `! ${controller.error}` : armed ? 'esc again to clear the draft' : working ? `${spinner} ${activity.label || 'Working…'}  ·  daemon continues if you detach`
      : controller.connection.startsWith('reconnecting') ? `○ ${controller.connection} · retrying automatically`
      : controller.pendingAction() ? '? Action staged · /confirm to review · /discard to dismiss'
      : entries.at(-1)?.kind === 'receipt' ? `✓ ${clip(entries.at(-1)!.title, mainWidth - 4)}` : '');
    status.visible = !!status.plainText;
    homeTip.content = noModel
      ? new StyledText([fg(theme.warning)(' ! '), fg(theme.text)('No model account connected. '), fg(theme.muted)('Run '), fg(theme.accent)('nosh login'), fg(theme.muted)(' in a terminal, then '), fg(theme.accent)('/refresh'), fg(theme.muted)('.')])
      : project
      ? new StyledText([fg(theme.accent)(' ◆ '), fg(theme.text)(basename(project.repositoryRoot)), fg(theme.muted)('   ask a research question, or try '), fg(theme.accent)('ctrl+g'), fg(theme.muted)(' map · '), fg(theme.accent)('/help')])
      : new StyledText([fg(theme.muted)(' Open a repository you already have, or create a new one: '), fg(theme.accent)('/open'), fg(theme.muted)(' · '), fg(theme.accent)('/new')]);
    const placeholder = project ? 'Ask NOSH… what should we investigate?' : 'Choose a project first — press ctrl+o';
    if (input.placeholder !== placeholder) input.placeholder = placeholder;
    const online = controller.connection === 'connected';
    connection.content = online ? '● connected' : controller.connection === 'connecting' ? '○ connecting' : '○ reconnecting';
    connection.fg = online ? theme.success : theme.warning;
    cwd.content = clip(project?.repositoryRoot || 'No project · ctrl+o to choose', Math.max(8, width - 38));
    sideProject.content = clip(project ? basename(project.repositoryRoot) : 'No project selected', sidebar.width - 4);
    transcript.root.visible = controller.view === 'chat'; inspector.root.visible = controller.view !== 'chat' && controller.view !== 'map'; researchMap.root.visible = controller.view === 'map';
    if (controller.view === 'map') researchMap.update(controller.researchMap, mainWidth);
    transcript.update(entries);
    if (controller.view !== 'chat' && controller.view !== 'map') inspector.update(controller.inspectorRows(), controller.view === 'help' ? HELP
      : controller.view === 'paths' ? controller.detail : EMPTY_VIEW[controller.view] ?? 'Nothing to show.');
    const sideRows = controller.sidebarRows();
    const attention: AttentionItem[] = [];
    const staged = controller.pendingAction();
    if (staged) attention.push({ title: staged.title, hint: '/confirm · /discard' });
    const proposals = sideRows.filter(row => row.section === 'approvals' && row.state === 'pending').length;
    if (proposals) attention.push({ title: `${proposals} graph proposal${proposals > 1 ? 's' : ''} pending`, hint: '/approvals to inspect' });
    research.update(sideRows, attention);
    // The spinner animates only while work is in flight.
    if (working && !spinTimer) spinTimer = setInterval(render, 180);
    else if (!working && spinTimer) { clearInterval(spinTimer); spinTimer = undefined; }
    if (changedView) {
      scroll.stickyScroll = controller.view === 'chat';
      scroll.stickyStart = controller.view === 'chat' ? 'bottom' : 'top';
      scroll.scrollTo(controller.view === 'chat' ? scroll.scrollHeight : 0);
      // Inspector views show a scrollbar only when content overflows; chat stays clean.
      if (controller.view === 'chat') scroll.verticalScrollBar.visible = false;
      else scroll.verticalScrollBar.resetVisibilityControl();
      previousView = controller.view; previousProject = controller.projectId;
    }
  }
  /** OpenCode-style ctrl+t: advance to the selected model's next advertised level. */
  async function cycleThinking(): Promise<void> {
    // Without an explicit selection, cycle the model the active agent is running.
    const activity = controller.activity(), slash = activity.model.indexOf('/');
    const selected = controller.selection ?? (slash > 0
      ? { model: { provider: activity.model.slice(0, slash), id: activity.model.slice(slash + 1) }, thinkingLevel: activity.thinkingLevel }
      : undefined);
    if (!selected || controller.busy) { if (!selected) dialogs.openThinking(); return; }
    try {
      const models = controller.models.length ? controller.models : await controller.loadModels();
      const model = models.find(m => m.provider === selected.model.provider && m.id === selected.model.id);
      if (!model?.thinkingLevels.length) { dialogs.openModels(); return; }
      const next = model.thinkingLevels[(model.thinkingLevels.indexOf(selected.thinkingLevel) + 1) % model.thinkingLevels.length]!;
      await controller.execute(`/model ${model.provider} ${model.id} ${next}`);
    } catch { /* controller.error is rendered in the status line. */ }
    finally { if (!disposed) render(); }
  }
  const handleKey = (key: KeyEvent) => {
    if (key.ctrl && key.name === 'c') { key.preventDefault(); quit(); return; }
    if (dialogs.handleKey(key)) return;
    const take = () => { key.preventDefault(); key.stopPropagation(); };
    if (key.ctrl && key.name === 'p') { take(); dialogs.openPalette(); return; }
    if (key.ctrl && key.name === 'o') { take(); void dialogs.openProjects(); return; }
    // Ctrl+M is Enter (CR) in most terminals, so the model picker uses F2.
    if (key.name === 'f2') { take(); void dialogs.openModels(); return; }
    if (key.ctrl && key.name === 't') { take(); void cycleThinking(); return; }
    if (key.ctrl && key.name === 'b') { take(); showSidebar = !showSidebar; render(); return; }
    if (key.ctrl && key.name === 'g') { take(); if (!controller.busy) void controller.execute(controller.view === 'map' ? '/chat' : '/map').catch(() => undefined).finally(render); return; }
    if (key.name === 'escape') {
      take();
      // A single Esc never discards a draft; a second Esc within 1.5s clears it.
      // Navigation, not a typed command: keep it out of input history.
      if (controller.view !== 'chat') { if (!controller.busy) void controller.execute('/chat').catch(() => undefined).finally(render); }
      else if (input.value && Date.now() - escapeArmedAt < 1500) { setComposer(''); escapeArmedAt = 0; }
      else if (input.value) { escapeArmedAt = Date.now(); controller.error = ''; }
      else controller.error = '';
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
    disposed = true; clearInterval(spinTimer);
    renderer.keyInput.off('keypress', handleKey); renderer.off(CliRenderEvents.RESIZE, render);
    dialogs.destroy(); shell.destroyRecursively(); syntax.destroy();
  }
  shell.on('destroyed', () => {
    if (!disposed) {
      disposed = true; clearInterval(spinTimer); renderer.keyInput.off('keypress', handleKey); renderer.off(CliRenderEvents.RESIZE, render);
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
  const recordProject = () => { if (config.handoffPath && controller.projectId) try { writeFileSync(config.handoffPath, JSON.stringify({ currentProjectId: controller.projectId })); } catch { /* Best effort: the next open falls back to the configured project. */ } };
  const cleanup = () => {
    recordProject();
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
