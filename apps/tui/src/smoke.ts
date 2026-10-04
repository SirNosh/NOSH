import { createTestRenderer } from '@opentui/core/testing';
import { DaemonClient } from './client.js';
import { Controller, type TranscriptEntry } from './controller.js';
import { createTranscript } from './visual.js';
import { createMarkdownStyle } from './theme.js';
import { createWorkspace } from './ui.js';
const test = await createTestRenderer({width:100,height:30,kittyKeyboard:true});
const client = new DaemonClient({baseUrl:'http://127.0.0.1',sessionToken:'test'});
const controller = new Controller(client);
const workspace = createWorkspace(test.renderer,controller,() => {});
function check(value: unknown, message: string): asserts value { if (!value) throw new Error(message); }
// Read live state without the narrowing left behind by earlier assertions.
const currentView = (): string => controller.view, composerText = (): string => workspace.input.value;
try {
  await test.renderOnce();
  let frame = test.captureCharFrame();
  check(frame.includes('███╗   ██╗') && frame.includes('nosh 0.1.0') && frame.includes('Choose a project first'), 'OpenTUI workspace did not render');
  await test.mockInput.typeText('research draft');
  test.mockInput.pressEnter({shift:true});
  await test.renderOnce();
  check(workspace.input.value.includes('\n'), 'Shift+Enter must insert a newline');
  const draft = workspace.input.value;
  test.mockInput.pressKey('p',{ctrl:true});
  await test.renderOnce();
  check(workspace.dialogs.isOpen && test.captureCharFrame().includes('Commands'), 'Ctrl+P must open palette');
  await test.mockInput.typeText('model'); await test.renderOnce();
  check(test.captureCharFrame().includes('Select model'), 'Palette must filter command choices');
  check(workspace.input.value === draft, 'Dialog typing leaked into composer');
  test.mockInput.pressEscape(); await test.renderOnce();
  check(!workspace.dialogs.isOpen && workspace.input.value === draft, 'Escape must restore unchanged composer');
  workspace.input.value = '';
  await test.mockInput.typeText('/'); await test.renderOnce();
  check(workspace.dialogs.isOpen, 'Slash must open command search');
  test.mockInput.pressEscape(); await test.renderOnce();
  await test.mockInput.typeText('model example example low'); await test.renderOnce();
  check(!workspace.dialogs.isOpen && workspace.input.value.startsWith('/model '), 'Dismissed slash menu must permit raw commands');
  workspace.input.value = '';
  await test.mockInput.typeText('/help'); await test.renderOnce();
  test.mockInput.pressEnter(); await new Promise(resolve => setTimeout(resolve, 50)); await test.renderOnce();
  check(controller.view === 'help' && workspace.input.value === '', 'A command chosen from the slash menu must consume its composer trigger');
  test.mockInput.pressEscape(); await new Promise(resolve => setTimeout(resolve, 50)); await test.renderOnce();
  check(currentView() === 'chat', 'Esc must return to the conversation');
  test.mockInput.pressArrow('up'); await new Promise(resolve => setTimeout(resolve, 50)); await test.renderOnce();
  check(composerText() === '/help' && !workspace.dialogs.isOpen, 'History recall of a slash command must not reopen slash search (and Esc navigation is not history)');
  test.mockInput.pressEscape(); test.mockInput.pressEscape(); await test.renderOnce();
  check(composerText() === '', 'Esc Esc must clear a recalled draft');
  workspace.dialogs.openProjectForm(); await test.renderOnce();
  check(test.captureCharFrame().includes('Repository path'), 'Project form missing');
  test.mockInput.pressEscape(); await test.renderOnce();
  controller.view = 'help'; controller.detail = 'Native help check'; workspace.render();
  await test.renderOnce();
  check(test.captureCharFrame().includes('ctrl+b hide'), 'Session sidebar missing at wide size');
  test.resize(60,20); workspace.render(); await test.renderOnce();
  frame = test.captureCharFrame();
  check(frame.includes('nosh 0.1.0') && !frame.includes('ctrl+b hide'), 'Narrow layout must hide sidebar');
  workspace.dispose();
  test.resize(100, 30);
  await checkNativeTranscript();
} finally { workspace.dispose(); controller.dispose(); client.close(); test.renderer.destroy(); }
console.log('OpenTUI native smoke passed: home/session, multiline input, searchable palette, modal focus, form, resize, transcript reconciliation.');

async function checkNativeTranscript() {
  const syntax = createMarkdownStyle();
  const transcript = createTranscript(test.renderer, syntax);
  test.renderer.root.add(transcript.root);
  const warnings: string[] = [];
  const warn = console.warn;
  const error = console.error;
  // Keep warnings visible and fail the smoke; do not mask renderer diagnostics.
  console.warn = (...args: unknown[]) => { warnings.push(args.map(String).join(' ')); warn(...args); };
  console.error = (...args: unknown[]) => { warnings.push(args.map(String).join(' ')); error(...args); };
  const entry = (id: string, text = id): TranscriptEntry => ({
    id, kind: 'assistant', title: 'Assistant', text, sequence: null, status: 'streaming',
  });
  const checkOrder = (ids: string[]) => {
    check(JSON.stringify(transcript.root.getChildren().map(child => child.id)) === JSON.stringify(ids.map(id => 'message-' + id)),
      'Native transcript order differs from replay order');
    check(transcript.root.getLayoutNode().getChildCount() === ids.length,
      'Native Yoga child count differs from transcript (duplicate or missing layout nodes)');
  };
  try {
    transcript.update([entry('a', 'Alpha'), entry('live', 'Draft')]);
    await test.renderOnce();
    const [a, live] = transcript.root.getChildren();
    const body = live!.getChildren()[1];
    transcript.update([entry('a', 'Alpha'), entry('live', 'Streaming delta'), entry('b', 'Beta')]);
    await test.renderOnce();
    checkOrder(['a', 'live', 'b']);
    check(live!.getChildren()[1] === body, 'Streaming must preserve native body identity');
    check(test.captureCharFrame().includes('Streaming delta'), 'Stream delta did not render');
    transcript.update([entry('b', 'Beta'), entry('a', 'Alpha'), entry('live', 'Streaming delta')]);
    await test.renderOnce();
    checkOrder(['b', 'a', 'live']);
    check(transcript.root.getChildren()[1] === a, 'Reorder rebuilt an existing message');
    transcript.update([entry('b', 'Beta'), entry('a', 'Alpha'), { ...entry('saved', 'Final answer'), status: 'complete' }]);
    await test.renderOnce();
    checkOrder(['b', 'a', 'saved']);
    check(live!.isDestroyed, 'Settled ephemeral block was not released');
    const frame = test.captureCharFrame();
    // Completed Markdown may parse asynchronously. Check its native position,
    // and the visible order of the already-rendered streaming blocks.
    const [b, alpha, saved] = transcript.root.getChildren();
    check(frame.includes('Beta') && frame.indexOf('Beta') < frame.indexOf('Alpha')
      && b!.y < alpha!.y && alpha!.y < saved!.y, 'Native rendered rows do not match transcript order');
    const entries = Array.from({ length: 101 }, (_, i) => entry(String(i), 'Message ' + i));
    transcript.update(entries.slice(0, 100));
    const [first, second] = transcript.root.getChildren();
    transcript.update(entries);
    await test.renderOnce();
    checkOrder(entries.slice(1).map(entry => entry.id));
    check(transcript.root.getChildren().length === 100 && transcript.root.getChildren()[0] === second && first!.isDestroyed,
      'Native transcript must evict old rows and retain surviving nodes');
    transcript.update([]);
    await test.renderOnce();
    checkOrder([]);
    check(warnings.length === 0, 'Native transcript emitted warnings: ' + warnings.join('; '));
  } finally {
    console.warn = warn; console.error = error;
    transcript.root.destroyRecursively();
    syntax.destroy();
  }
}
