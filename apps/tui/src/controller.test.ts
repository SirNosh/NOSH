import { describe, it, expect, vi } from 'vitest';
import { Controller, acceptPage, safeText, type NoshEvent } from './controller.js';
import { DaemonClient } from './client.js';
const event = (sequence: number, projectId = 'p'): NoshEvent => ({eventId:`e${sequence}`,sequence,type:'agent.completed',payload:{message:'hello'},scope:{projectId}});
function setup() { const client = new DaemonClient({baseUrl:'http://localhost',sessionToken:'s'}); const request = vi.spyOn(client,'request').mockResolvedValue({}); const controller = new Controller(client,'p'); controller.projects = [{projectId:'p',repositoryRoot:'/p'},{projectId:'q',repositoryRoot:'/q'}]; return {controller,request}; }
describe('thin controller', () => {
  it('keeps the prior view and content when a command fails', async () => {
    const {controller} = setup(); controller.projectId = ''; controller.view = 'help'; controller.detail = 'help text';
    await expect(controller.execute('/jobs')).rejects.toThrow('Select or open');
    expect(controller.view).toBe('help'); expect(controller.detail).toBe('help text');
  });
  it('keeps the explicitly selected intake model after opening a project', async () => {
    const {controller,request} = setup();
    controller.selection = {model:{provider:'x',id:'model'},thinkingLevel:'low'};
    request.mockResolvedValueOnce({project:{projectId:'new',repositoryRoot:'/new'}});
    await controller.execute('/open {"path":"/new","workingTitle":"Study","createRepository":true}');
    expect(controller.projectId).toBe('new');
    expect(controller.selection).toEqual({model:{provider:'x',id:'model'},thinkingLevel:'low'});
  });
  it('bootstraps once, resumes deltas after reconnect, and resets on project switch', async () => {
    const {controller,request} = setup();
    request.mockResolvedValueOnce({events:[event(999),event(1000)],nextCursor:1000,hasMore:false})
      .mockRejectedValueOnce(new Error('offline'))
      .mockResolvedValueOnce({events:[event(1001),event(1002)],nextCursor:1002,hasMore:false})
      .mockResolvedValueOnce({events:[event(700,'q')],nextCursor:700,hasMore:false});
    await controller.poll();
    expect(request).toHaveBeenLastCalledWith('/events?projectId=p&after=0&limit=200&recent=true');
    expect(controller.cursor).toBe(1000);
    await controller.poll(); expect(controller.cursor).toBe(1000);
    await controller.poll();
    expect(request).toHaveBeenLastCalledWith('/events?projectId=p&after=1000&limit=200');
    expect(controller.events.map(e => e.sequence)).toEqual([999,1000,1001,1002]);
    await controller.execute('/project q'); await controller.poll();
    expect(request).toHaveBeenLastCalledWith('/events?projectId=q&after=0&limit=200&recent=true');
    expect(controller.events.map(e => e.sequence)).toEqual([700]);
  });
  it('does not repeat recent bootstrap after a successful empty page', async () => {
    const {controller,request} = setup();
    request.mockResolvedValueOnce({events:[]}).mockResolvedValueOnce({events:[event(1)]});
    await controller.poll(); await controller.poll();
    expect(request).toHaveBeenNthCalledWith(1,'/events?projectId=p&after=0&limit=200&recent=true');
    expect(request).toHaveBeenLastCalledWith('/events?projectId=p&after=0&limit=200');
    expect(controller.cursor).toBe(1);
  });
  it('bounds history and filters duplicates, old cursors and other projects', () => {
    const page = Array.from({length:600},(_,i) => event(i+1));
    const result = acceptPage([], [...page,event(601,'other'),event(600)],'p',0);
    expect(result.events).toHaveLength(400); expect(result.after).toBe(600);
    expect(acceptPage(result.events,[event(3)],'p',600).events).toHaveLength(400);
  });
  it('requires explicit confirmation and preserves inspected version and idempotency', async () => {
    const {controller,request} = setup();
    await controller.execute('/approve proposal 7'); expect(request).not.toHaveBeenCalled();
    await controller.execute('/confirm');
    expect(request).toHaveBeenCalledWith('/graph-proposals/proposal/approve',expect.objectContaining({projectId:'p',expectedProposalVersion:7,idempotencyKey:expect.stringMatching(/^tui-/)}));
    await expect(controller.execute('/confirm')).rejects.toThrow('No staged'); expect(request).toHaveBeenCalledTimes(1);
  });
  it('clears pending actions, model and cursor on project switch', async () => {
    const {controller} = setup(); controller.cursor = 50; controller.events = [event(50)];
    await controller.execute('/cancel job'); await controller.execute('/project q');
    expect(controller.cursor).toBe(0); expect(controller.events).toEqual([]);
    await expect(controller.execute('/confirm')).rejects.toThrow('No staged');
  });
  it('rejects stale asynchronous pages after project changes', async () => {
    const {controller,request} = setup(); let resolve!: (v:unknown) => void;
    request.mockReturnValue(new Promise(r => {resolve = r;}));
    const poll = controller.poll(); await controller.execute('/project q'); resolve({events:[event(1)]}); await poll;
    expect(controller.events).toEqual([]); expect(controller.cursor).toBe(0);
  });
  it('validates model thinking and sends explicit selection with chat', async () => {
    const {controller,request} = setup(); controller.models = [{provider:'x',id:'model',name:'Model',thinkingLevels:['off','low']}];
    await expect(controller.execute('/model x model high')).rejects.toThrow('Thinking');
    await controller.execute('/model x model low'); await controller.execute('hello');
    expect(request).toHaveBeenCalledWith('/chat',expect.objectContaining({model:{provider:'x',id:'model'},thinkingLevel:'low',message:'hello'}));
  });
  it('asks for bounded pages and reconnects without losing cursor', async () => {
    const {controller,request} = setup(); controller.cursor=9;
    request.mockRejectedValueOnce(new Error('offline')).mockResolvedValueOnce({events:[event(10)]});
    await controller.poll(); expect(controller.connection).toContain('reconnecting'); expect(controller.cursor).toBe(9);
    await controller.poll(); expect(controller.cursor).toBe(10); expect(request).toHaveBeenLastCalledWith('/events?projectId=p&after=9&limit=200');
  });
  it('renders authoritative assistant completion and strips terminal controls', () => {
    const {controller} = setup(); controller.events=[event(1)]; expect(controller.transcript()).toContain('NOSH'); expect(controller.transcript()).toContain('hello');
    expect(safeText('hello\x1b]52;secret\x07')).not.toContain('\x1b');
  });
});

describe('workspace presentation', () => {
  const agentEvent = (sequence: number | null, type: string, payload: Record<string,unknown> = {}, agentId = 'a'): NoshEvent => ({eventId:`${type}-${sequence}`,sequence,type,payload,scope:{projectId:'p',agentId}});
  it('projects durable user, assistant, tool, failure and host receipts with stable IDs', () => {
    const {controller} = setup();
    const start = agentEvent(2,'agent.tool_started',{toolCallId:'call',toolName:'read',args:{path:'paper.md'}});
    controller.events = [agentEvent(1,'chat.user_message',{message:'Read the paper',agentId:'a',selection:{model:{provider:'x',id:'m'},thinkingLevel:'low'}}),start];
    const toolId = controller.transcriptEntries().find(e => e.kind === 'tool')!.id;
    controller.events.push(agentEvent(3,'agent.tool_completed',{toolCallId:'call',toolName:'read',result:{content:[{type:'text',text:'Paper contents'},{type:'thinking',text:'never display'}]}}),agentEvent(4,'agent.completed',{message:'The paper is ready.'}),agentEvent(5,'agent.terminal_receipt',{accepted:true,retryAllowed:false,status:'completed',effect:{state:'completed'}}),agentEvent(6,'agent.failed',{message:'Provider access failed'}));
    const entries = controller.transcriptEntries();
    expect(entries.map(e => e.kind)).toEqual(['user','tool','assistant','receipt','failure']);
    expect(entries[1]).toMatchObject({id:toolId,status:'completed',text:'Paper contents'});
    expect(entries[2]).toMatchObject({model:'x/m',thinkingLevel:'low'});
    expect(JSON.stringify(entries)).not.toContain('never display');
    expect(entries[3]?.text).toContain('accepted: true');
  });
  it('shows validation receipts separately from terminal JSON and preserves host failure', () => {
    const {controller} = setup(); controller.events = [agentEvent(1,'agent.completed',{message:JSON.stringify({$schema:'https://nosh.dev/schemas/terminal-output/v1',schemaVersion:1,records:[{summary:'Experiment finished'}]})}),agentEvent(2,'agent.terminal_receipt',{accepted:true,retryAllowed:false,status:'completed',effect:{state:'failed'}})];
    expect(controller.transcriptEntries()[0]?.text).toContain('Experiment finished');
    expect(controller.transcriptEntries()[0]?.text).not.toContain('$schema');
    expect(controller.transcriptEntries()[1]?.status).toBe('failed');
  });
  it('streams bounded drafts, deduplicates live IDs and replaces them with authoritative completion', () => {
    const {controller} = setup(), listener = vi.fn(); controller.subscribe(listener);
    controller.ingestLiveEvent(agentEvent(1,'agent.started'));
    const first = agentEvent(null,'agent.text_delta',{delta:'Hello'}); controller.ingestLiveEvent(first); controller.ingestLiveEvent(first);
    const draft = controller.transcriptEntries()[0]!; expect(draft).toMatchObject({kind:'assistant',text:'Hello',status:'streaming'});
    controller.ingestLiveEvent({...first,eventId:'delta-2',payload:{delta:' world'}});
    expect(controller.transcriptEntries()[0]).toMatchObject({id:draft.id,text:'Hello world'});
    expect(controller.cursor).toBe(1); expect(controller.activity().working).toBe(true);
    controller.ingestLiveEvent(agentEvent(2,'agent.completed',{message:'Authoritative result'}));
    expect(controller.transcriptEntries()).toHaveLength(1); expect(controller.transcriptEntries()[0]?.text).toBe('Authoritative result');
    controller.ingestLiveEvent({...first,eventId:'late'}); expect(controller.transcriptEntries()).toHaveLength(1);
    expect(controller.activity().working).toBe(false); expect(listener).toHaveBeenCalled();
    controller.ingestLiveEvent(agentEvent(3,'agent.started'));
    controller.ingestLiveEvent({...first,eventId:'new-turn',payload:{delta:'Next turn'}});
    expect(controller.transcriptEntries().at(-1)).toMatchObject({text:'Next turn',status:'streaming'});
  });
  it('clears ephemeral tool output on completion, disconnect and project switch', () => {
    const {controller} = setup(); controller.ingestLiveEvent(agentEvent(1,'agent.tool_started',{toolCallId:'call',toolName:'read',args:{path:'file'}}));
    const update = agentEvent(null,'agent.tool_update',{toolCallId:'call',toolName:'read',partialResult:{content:[{type:'text',text:'partial'}]}});
    controller.ingestLiveEvent(update); expect(controller.transcriptEntries()).toHaveLength(1); expect(controller.transcriptEntries()[0]?.text).toBe('partial');
    controller.ingestLiveEvent(agentEvent(2,'agent.tool_completed',{toolCallId:'call',toolName:'read',result:{content:[{type:'text',text:'final'}]}}));
    expect(controller.transcriptEntries()[0]?.text).toBe('final');
    controller.ingestLiveEvent({...update,eventId:'late-tool-update'}); expect(controller.transcriptEntries()[0]?.text).toBe('final');
    controller.ingestLiveEvent(agentEvent(null,'agent.text_delta',{delta:'temporary'})); controller.clearLiveDrafts();
    expect(controller.transcriptEntries().every(e => e.sequence !== null)).toBe(true);
    controller.selectProject('q'); controller.ingestLiveEvent(update); expect(controller.transcriptEntries()).toEqual([]);
  });
  it('tracks concurrent agent work and does not claim new composer model ran the active turn', async () => {
    const {controller} = setup(); controller.selection = {model:{provider:'next',id:'future'},thinkingLevel:'high'};
    controller.events = [agentEvent(1,'chat.user_message',{message:'hello',selection:{model:{provider:'current',id:'active'},thinkingLevel:'low'}}),agentEvent(2,'agent.started'),agentEvent(3,'agent.started',{},'b'),agentEvent(4,'agent.completed',{message:'finished'},'b')];
    expect(controller.activity()).toMatchObject({working:true,agentId:'a',model:'current/active',thinkingLevel:'low'});
    controller.events.push(agentEvent(5,'agent.failed',{message:'failed'})); expect(controller.activity()).toMatchObject({working:false,model:'next/future'});
  });
  it('uses read-only catalog loads and detached pending snapshots', async () => {
    const {controller,request} = setup(); controller.view = 'help'; controller.detail = 'unchanged';
    request.mockResolvedValueOnce({projects:[{projectId:'p',repositoryRoot:'/p'}]}).mockResolvedValueOnce({models:[{provider:'x',id:'m',name:'Model',thinkingLevels:['off']} ]});
    await controller.loadProjects(); const models = await controller.loadModels(); models[0]!.thinkingLevels.push('fake');
    expect(controller.view).toBe('help'); expect(controller.detail).toBe('unchanged'); expect(controller.models[0]?.thinkingLevels).toEqual(['off']);
    await controller.execute('/approve proposal 7'); const action = controller.pendingAction()!; action.body.expectedProposalVersion = 99;
    expect(controller.pendingAction()?.body.expectedProposalVersion).toBe(7);
  });
  it('renders structured status and approval rows with inspected versions instead of JSON blobs', async () => {
    const {controller,request} = setup(); request.mockImplementation(async path => {
      if (path === '/missions?projectId=p') return {missions:[{entityId:'mis',version:4,state:'running',value:{title:'A study',objective:'Measure quality'}}]};
      if (path === '/directions?projectId=p') return {directions:[]};
      if (path === '/autoresearch?projectId=p') return {executions:[{entityId:'aut',version:2,state:'blocked',value:{decisionQuestion:'Can we improve?'}}]};
      if (path === '/agents?projectId=p') return {agents:[{agentId:'a',role:'research_director',modelProvider:'x',modelId:'m',thinkingLevel:'low',status:'running'}]};
      if (path === '/projects/p/contract') return {contract:null};
      if (path === '/graph-proposals?projectId=p') return {proposals:[{entityId:'prop',version:7,state:'pending',value:{record:{rationale:'Add comparison',contractImpact:'scope'}}}]};
      return {};
    });
    await controller.execute('/status'); expect(controller.inspectorRows()).toEqual(expect.arrayContaining([expect.objectContaining({id:'mis',section:'missions',title:'A study',version:4,state:'running'}),expect.objectContaining({id:'aut',section:'autoresearch',title:'Can we improve?'})]));
    await controller.execute('/approvals'); expect(controller.inspectorRows()).toEqual(expect.arrayContaining([expect.objectContaining({id:'prop',version:7,title:'Add comparison'}),expect.objectContaining({id:'contract',state:'not approved'})]));
    expect(controller.sidebarRows().some(row => row.id === 'mis')).toBe(true);
  });
  it('derives sidebar state from durable events without inventing inspected versions', () => {
    const {controller} = setup(); controller.events = [agentEvent(1,'mission.created',{entityId:'mis',state:'running'}),agentEvent(2,'job.state_changed',{jobId:'j',state:'failed',command:['python','run.py'],failureReason:'timeout'}),agentEvent(3,'graph.proposal_pending',{proposalId:'prop',contractImpact:'scope'})];
    expect(controller.sidebarRows()).toEqual(expect.arrayContaining([expect.objectContaining({id:'mis',state:'running'}),expect.objectContaining({id:'j',state:'failed'}),expect.objectContaining({id:'prop',state:'pending'})]));
    expect(controller.sidebarRows().find(row => row.id === 'mis')?.version).toBeUndefined();
  });
  it('keeps the current server /tail endpoint and shows literal bounded output plus job resources', async () => {
    const {controller,request} = setup(); request.mockResolvedValueOnce({text:'old'.repeat(10000)+'\nrecent\x1b[31m text'}).mockResolvedValueOnce({job:{jobId:'j',state:'running',command:['python','train.py']},resources:{elapsedSeconds:12,residentBytes:1024,outputStalledSeconds:1}});
    await controller.execute('/tail j stderr'); expect(request).toHaveBeenLastCalledWith('/jobs/j/tail?projectId=p&stream=stderr');
    expect(controller.inspectorRows()[0]?.fields[0]?.value).toContain('recent text'); expect(controller.inspectorRows()[0]?.fields[0]?.value.length).toBeLessThanOrEqual(16000);
    await controller.execute('/job j'); expect(controller.inspectorRows()[0]?.fields).toContainEqual({label:'elapsed Seconds',value:'12'});
  });
  it('bounds rendered history, strips complete OSC/ANSI and ignores other-project events', () => {
    const {controller} = setup(); controller.events = Array.from({length:400},(_,i) => agentEvent(i+1,'agent.completed',{message:'x'.repeat(16000)}));
    const entries = controller.transcriptEntries(); expect(entries.reduce((n,e) => n+e.text.length,0)).toBeLessThanOrEqual(64000);
    expect(entries.length).toBeLessThanOrEqual(120);
    expect(safeText('before\x1b]52;c;secret\x07\x1b[31mred\x1b[0m\rafter\u202e')).toBe('beforeredafter');
    controller.events = [event(1,'other')]; expect(controller.transcriptEntries()).toEqual([]);
  });
});
describe('project-scoped concurrency', () => {
  it.each(['/jobs','/status','/approvals','/job j','/tail j','hello','/confirm','/open {"path":"/new","workingTitle":"New","createRepository":false}'])('ignores late %s response after project switch', async command => {
    const {controller,request} = setup(); let resolve!: (v:unknown) => void;
    if (command === '/confirm') await controller.execute('/cancel j');
    const response = new Promise(r => {resolve=r;}); request.mockReturnValue(response);
    const pending = controller.execute(command); controller.selectProject('q');
    resolve({jobs:[{jobId:'stale',state:'running'}],missions:[],directions:[],executions:[],agents:[],proposals:[],contract:null,project:{projectId:'new',repositoryRoot:'/new'}}); await pending;
    expect(controller.projectId).toBe('q'); expect(controller.view).toBe('chat'); expect(controller.detail).toBe(''); expect(controller.inspectorRows()).toEqual([]);
  });
  it('does not restore the old view or error after a stale command failure', async () => {
    const {controller,request} = setup(); let reject!: (v:unknown) => void; request.mockReturnValue(new Promise((_r,j) => {reject=j;}));
    controller.view='help'; controller.detail='old help'; const pending=controller.execute('/jobs'); controller.selectProject('q'); reject(new Error('old error'));
    await expect(pending).rejects.toThrow('old error'); expect(controller.view).toBe('chat'); expect(controller.detail).toBe(''); expect(controller.error).toBe('');
  });
  it('never retries a failed confirmation and keeps its original project/version/idempotency', async () => {
    const {controller,request} = setup(); await controller.execute('/transition missions m 3 paused'); const action=controller.pendingAction()!;
    request.mockRejectedValueOnce(new Error('lost response'));
    await expect(controller.execute('/confirm')).rejects.toThrow('lost response'); expect(controller.pendingAction()).toBeUndefined();
    await expect(controller.execute('/confirm')).rejects.toThrow('No staged action'); expect(request).toHaveBeenCalledTimes(1); expect(request).toHaveBeenCalledWith(action.path,action.body);
  });
  it('subscribes only after HTTP bootstrap and closes stale project callbacks/disposes', async () => {
    const {controller,request} = setup(); vi.spyOn(controller.client,'authenticate').mockResolvedValue();
    let onEvent!: (e:NoshEvent) => void, onState!: (s:'connecting'|'connected'|'disconnected') => void; const close=vi.fn();
    const subscribe=vi.spyOn(controller.client,'subscribe').mockImplementation((_p,_after,e,s) => {onEvent=e; onState=s; return close;});
    request.mockResolvedValueOnce({projects:controller.projects}).mockResolvedValueOnce({events:[event(10)]});
    await controller.initialize(); expect(subscribe).not.toHaveBeenCalled(); await controller.poll(); expect(subscribe).toHaveBeenCalledWith('p',10,expect.any(Function),expect.any(Function));
    onEvent({eventId:'delta',sequence:null,type:'agent.text_delta',payload:{delta:'draft'},scope:{projectId:'p',agentId:'a'}}); expect(controller.transcriptEntries().some(e => e.status==='streaming')).toBe(true);
    onState('disconnected'); expect(controller.transcriptEntries().some(e => e.status==='streaming')).toBe(false);
    controller.selectProject('q'); expect(close).toHaveBeenCalledTimes(1); onEvent(event(11)); expect(controller.events).toEqual([]); controller.dispose();
  });
});
