/** Guest-owned JSONL adapter around the documented Agent SDK. No provider keys on disk. */
import { query, forkSession, type Query, type SDKUserMessage, type PermissionResult, type PermissionMode } from '@anthropic-ai/claude-agent-sdk';
import { createInterface } from 'node:readline';
import { randomUUID } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { posix } from 'node:path';

type Json = Record<string, any>;
const emit = (value: Json) => process.stdout.write(JSON.stringify(value) + '\n');
const notify = (method: string, params: Json) => emit({method, params});
class Input implements AsyncIterable<SDKUserMessage> {
  private items: SDKUserMessage[] = [];
  private wake?: () => void;
  closed = false;
  push(content: string | Json[], uuid: string, session: string) {
    this.items.push({type:'user',message:{role:'user',content:content as any},parent_tool_use_id:null,session_id:session,uuid:uuid as `${string}-${string}-${string}-${string}-${string}`});
    this.wake?.();
  }
  close() { this.closed=true; this.wake?.(); }
  async *[Symbol.asyncIterator]() {
    while (!this.closed) {
      if (!this.items.length) await new Promise<void>(resolve => {this.wake=resolve;});
      while (this.items.length) yield this.items.shift()!;
    }
  }
}
/** Largest image sent as is: 5 MB once base64-encoded. */
const MAX_IMAGE = 3_932_160;
/** An attached image as a content block; undefined for anything else under /attachments or elsewhere. */
async function image(path: string): Promise<Json | undefined> {
  if (!posix.normalize(path).startsWith('/attachments/')) return undefined;
  const data = await readFile(path).catch(() => undefined);
  if (!data || data.length > MAX_IMAGE) return undefined;
  const head = (start: number, end: number) => data.subarray(start, end).toString('latin1');
  const type = head(0, 8) === '\x89PNG\r\n\x1a\n' ? 'image/png'
    : data[0] === 0xff && data[1] === 0xd8 && data[2] === 0xff ? 'image/jpeg'
    : head(0, 4) === 'RIFF' && head(8, 12) === 'WEBP' ? 'image/webp'
    : head(0, 6) === 'GIF87a' || head(0, 6) === 'GIF89a' ? 'image/gif'
    : undefined;
  return type && {type:'image', source:{type:'base64', media_type:type, data:data.toString('base64')}};
}
/** A turn's input as message content: images first, then the text. */
async function content(items: Json[]): Promise<string | Json[]> {
  const text = items.filter(i => i.type === 'text').map(i => i.text ?? '').join('\n');
  const blocks: Json[] = [];
  for (const item of items) {
    if (item.type !== 'localImage' || typeof item.path !== 'string') continue;
    blocks.push(await image(item.path) ?? {type:'text', text:`[Image ${item.path} could not be attached; open it with your tools.]`});
  }
  if (!blocks.length) return text;
  return text.trim() ? [...blocks, {type:'text', text}] : blocks;
}
let sdk: Query | undefined;
let input: Input | undefined;
let session = '';
let resume: string | undefined;
let fork = false;
let resumeAt: string | undefined;
let turn: string | undefined;
let interrupted = false;
let config: Json = {model:process.env.RYZHUB_CLAUDE_MODEL,approval:'ask',reasoning:'medium',plan:false};
let commands: Json[] = [];
let started = new Set<string>();
const blockIds=new Map<string,string[]>();
const pending = new Map<string,(result:Json)=>void>();
const tools = new Map<string,{name:string,input:Json}>();
const permission = ():PermissionMode => config.plan ? 'plan' : config.approval==='allow_edits' ? 'acceptEdits' : 'default';
const params = (extra:Json={}) => ({threadId:session,turnId:turn,...extra});
/** An item event. What a subagent produces names the agent's call as its parent. */
function item(method:string,id:string,type:string,data:Json,raw?:unknown) {
  const parent=(raw as Json|undefined)?.parent_tool_use_id;
  notify(method,params({item:{id,type,...data,...(parent?{parentItemId:parent}:{})},raw}));
}
async function authorize(tool:string, args:Json, signal:AbortSignal, toolId:string):Promise<PermissionResult> {
  if (signal.aborted) return {behavior:'deny',message:'Interrupted'};
  if (tool!=='AskUserQuestion' && config.approval==='full_auto' && !config.plan) return {behavior:'allow',updatedInput:args};
  const id=`permission:${toolId}:${randomUUID()}`;
  const question=tool==='AskUserQuestion';
  const answer=await new Promise<Json>(resolve => {
    const finish=(result:Json)=>{pending.delete(id);signal.removeEventListener('abort',abort);resolve(result);};
    const abort=()=>finish({decision:'decline'});
    pending.set(id,finish);signal.addEventListener('abort',abort,{once:true});
    emit({id,method:question?'item/tool/requestUserInput':'item/commandExecution/requestApproval',params:params({harness:'claude',tool,input:args,command:tool+' '+JSON.stringify(args),questions:question?(args.questions??[]).map((q:Json,n:number)=>({id:String(n),question:q.question,options:q.options})):undefined})});
  });
  if (question && answer.answers) {
    const answers:Record<string,string>={};
    (args.questions??[]).forEach((q:Json,n:number)=>{answers[q.question]=(answer.answers[String(n)]?.answers??[]).join(', ');});
    return {behavior:'allow',updatedInput:{...args,answers}};
  }
  return answer.decision==='accept' ? {behavior:'allow',updatedInput:args} : {behavior:'deny',message:'User denied this action'};
}
async function start() {
  if (sdk) return;
  input=new Input();
  let project='';try {project=await readFile('/workspace/.platform/project.md','utf8');}catch{}
  try {project+='\n'+await readFile('/workspace/.platform/memory.md','utf8');}catch{}
  sdk=query({prompt:input,options:{
    cwd:process.env.RYZHUB_WORKSPACE??'/workspace',
    model:config.model,effort:config.reasoning,
    ...(resume?{resume,forkSession:fork,resumeSessionAt:resumeAt}:{sessionId:session}),
    permissionMode:permission(),
    settingSources:[], // User-controlled CLI settings cannot replace platform routing or hooks.
    systemPrompt:{type:'preset',preset:'claude_code',append:project},
    includePartialMessages:true,
    // What a subagent says arrives too, under the call that started it.
    forwardSubagentText:true,
    env:{...process.env,ANTHROPIC_BASE_URL:process.env.ANTHROPIC_BASE_URL??'http://127.0.0.1:8090',ANTHROPIC_API_KEY:process.env.ANTHROPIC_API_KEY??'guest-proxy',CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC:'1'},
    ...(process.env.RYZHUB_MCP==='1'?{mcpServers:{ryzhub:{type:'http' as const,url:'http://127.0.0.1:8090/mcp'}}}:{}),
    canUseTool:(name,args,options)=>authorize(name,args,options.signal,options.toolUseID),
    hooks:{PostToolUse:[{hooks:[async event=>{if(event.hook_event_name==='PostToolUse'){const output=JSON.stringify(event.tool_response);if(event.tool_name==='WebSearch')item('item/completed',event.tool_use_id,'webSearch',{query:(event.tool_input as Json)?.query??'',output},event);else item('item/completed',event.tool_use_id,'commandExecution',{command:event.tool_name,aggregatedOutput:output},event);}return {}; }]}]},
  }});
  const active=sdk;
  void consume(active).catch(()=>{if(sdk===active){sdk=undefined;notify('harness/exited',{outcome:'unknown',error:'SDK_STREAM_FAILED'});}});
  commands=await active.supportedCommands();
  const models=(await active.supportedModels()).map(m=>({id:m.value,model:m.value}));
  notify('session/capabilities',{harness:'claude',version:'0.3.283',commands,models,steer:'emulated'});
}
async function consume(active:Query) {
  let currentMessage='';
  for await (const message of active) {
    const raw=message as unknown as Json;
    // Keep native payloads in the guest journal, never reconstruct sessions from UI text.
    if (message.type==='system' && message.subtype==='init') {
      session=message.session_id;
      notify('session/initialized',{sessionId:session,raw:message});
    } else if(message.type==='stream_event') {
      const e=message.event as unknown as Json;
      if(e.type==='message_start') currentMessage=e.message.id;
      if(e.type==='content_block_start') {const key=`${currentMessage}:${e.content_block.type}`;blockIds.set(key,[...(blockIds.get(key)??[]),`${currentMessage}:${e.index}`]);}
      if(e.type==='content_block_start' && e.content_block.type==='text') {
        const id=`${currentMessage}:${e.index}`;started.add(id);item('item/started',id,'agentMessage',{text:''},message);
      }
      if(e.type==='content_block_delta' && e.delta.type==='text_delta') notify('item/agentMessage/delta',params({itemId:`${currentMessage}:${e.index}`,delta:e.delta.text}));
    } else if(message.type==='assistant') {
      for (let n=0;n<message.message.content.length;n++) {
        const block=message.message.content[n] as unknown as Json;
        // SDK assistant frames may each contain one block, retaining a common message id.
        const key=`${message.message.id}:${block.type}`;
        const id=block.type==='tool_use'?block.id:(blockIds.get(key)?.shift()??`${message.uuid}:${n}`);
        if(block.type==='text') {if(!started.has(id))item('item/started',id,'agentMessage',{text:''});item('item/completed',id,'agentMessage',{text:block.text,nativeMessageId:message.uuid},message);}
        if(block.type==='thinking') item('item/completed',id,'reasoning',{summary:[block.thinking],nativeMessageId:message.uuid},message);
        // WebSearch runs on the provider; it is projected like Codex's search, not as a command.
        if(block.type==='tool_use' && block.name==='WebSearch') {tools.set(id,{name:block.name,input:block.input});item('item/started',id,'webSearch',{query:block.input?.query??'',nativeMessageId:message.uuid},message);}
        else if(block.type==='tool_use') {tools.set(id,{name:block.name,input:block.input});item('item/started',id,block.name==='TodoWrite'?'plan':'commandExecution',{command:block.name+' '+JSON.stringify(block.input),text:JSON.stringify(block.input),nativeMessageId:message.uuid},message);}
      }
    } else if(message.type==='result') {
      const ended=turn;turn=undefined;
      notify('turn/completed',{threadId:session,turn:{id:ended,status:interrupted?'interrupted':message.is_error?'failed':'completed'},raw:message});
      interrupted=false;started.clear();blockIds.clear();tools.clear();
    } else if(raw.type==='system' && raw.subtype==='compact_boundary') {
      notify('session/compacted',{threadId:session,raw:message});
    } else {notify('session/event',params({raw:message}));}
  }
  if(sdk===active) {sdk=undefined;notify('harness/exited',{outcome:'unknown',error:'SDK_PROCESS_ENDED'});}
}
async function handle(request:Json):Promise<any> {
  const p=request.params??{};
  switch(request.method) {
    case 'initialize':return {capabilities:{steer:'emulated',interrupt:'native',fork:'native'}};
    case 'initialized':return;
    case 'model/list':return {data:[{id:config.model,model:config.model,supportedReasoningEfforts:['low','medium','high','xhigh','max'].map(reasoningEffort=>({reasoningEffort}))}]};
    case 'skills/list':return {data:commands};
    case 'thread/start':session=randomUUID();config={...config,...(p.config??{}),model:p.model??config.model};return {thread:{id:session}};
    case 'thread/resume':
      if(sdk && session===p.threadId)return {thread:{id:session}};
      if(sdk)throw new Error('SESSION_ALREADY_RUNNING');
      resume=p.threadId;session=resume!;config={...config,...(p.config??{}),model:p.model??config.model};return {thread:{id:session}};
    case 'thread/fork':
      if(sdk || turn)throw new Error('FORK_REQUIRES_NEW_SANDBOX');
      const result=await forkSession(p.threadId,{dir:process.env.RYZHUB_WORKSPACE??"/workspace",upToMessageId:p.resumeSessionAt});resume=result.sessionId;session=result.sessionId;return {thread:{id:session}};
    case 'session/configure':
      if(sdk && p.project!==undefined && p.project!==config.project){if(turn)throw new Error('TURN_BUSY');const old=sdk;sdk=undefined;input?.close();old.close();resume=session;}
      config={...config,...p};
      if(sdk){
        if(p.model)await sdk.setModel(p.model);
        await sdk.setPermissionMode(permission());
        if(p.reasoning)await sdk.applyFlagSettings({effortLevel:p.reasoning});
      }
      return {effective:'next_turn'};
    case 'turn/start': {
      if(turn)throw new Error('TURN_BUSY');
      await start();turn=randomUUID();interrupted=false;
      const id=turn;
      // Reply before any projection event so the server can register the native turn.
      setImmediate(()=>void content(p.input??[]).then(message=>input!.push(message,id,session)));
      return {turn:{id}};
    }
    case 'turn/steer':throw new Error('STEER_EMULATED_QUEUE_NEXT_TURN');
    case 'turn/interrupt':
      if(sdk && turn){interrupted=true;await sdk.interrupt();}return {};
    case 'thread/compact/start':
      if(turn)throw new Error('TURN_BUSY');await start();turn=randomUUID();input!.push('/compact',turn,session);return {turn:{id:turn}};
    default:throw new Error('UNSUPPORTED_METHOD');
  }
}
const lines=createInterface({input:process.stdin,crlfDelay:Infinity});
for await(const line of lines) {
  if(Buffer.byteLength(line)>4*1024*1024)throw new Error('REQUEST_TOO_LARGE');
  const request=JSON.parse(line);
  if(!request.method){pending.get(String(request.id))?.(request.result??{});continue;}
  // SDK control/permission messages remain responsive while a turn is running.
  void handle(request).then(result=>{if(request.id!==undefined)emit({id:request.id,result:result??{}});}).catch(()=>{if(request.id!==undefined)emit({id:request.id,error:{code:-32000,message:'CLAUDE_ADAPTER_REQUEST_FAILED'}});});
}
input?.close();sdk?.close();
