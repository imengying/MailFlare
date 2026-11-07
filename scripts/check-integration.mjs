import assert from 'node:assert/strict';
import { readFile, readdir, mkdtemp } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createHmac } from 'node:crypto';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
const root = process.cwd();
const secret = 'local-integration-secret-only';
const objects = new Map();
const calls = [];
let failAttachment = false;
let failDelete = false;
let barkFailures = 0;
const mf = new Miniflare(convertV4MiniflareOptions({
  rootPath: root, modulesRoot: `${root}/build`,
  modules: [{type:'ESModule',path:`${root}/build/index.js`},{type:'CompiledWasm',path:`${root}/build/index_bg.wasm`}],
  compatibilityDate: '2025-01-01', compatibilityFlags: ['global_fetch_strictly_public'],
  bindings: { ADMIN_USERNAME:'local',ADMIN_PASSWORD:'local-password',JWT_SECRET:secret },
  d1Databases: ['DB','LEGACY'], unsafeTriggerHandlers: true,
  outboundService: async request => {
    const url = new URL(request.url);
    const call = { method: request.method, url: request.url };
    calls.push(call);
    if (url.hostname.endsWith('.r2.cloudflarestorage.com')) {
      if (request.method === 'PUT') {
        if (failAttachment && !url.pathname.endsWith('original.eml')) return new Response('storage unavailable', { status: 503 });
        objects.set(url.pathname, Buffer.from(await request.arrayBuffer()));
        return new Response(null, {status: 200});
      }
      if (request.method === 'DELETE') {
        if (failDelete) return new Response('storage unavailable', {status: 503});
        objects.delete(url.pathname);
        return new Response(null, {status: 204});
      }
      if (request.method === 'GET') return objects.has(url.pathname) ? new Response(objects.get(url.pathname)) : new Response(null, {status: 404});
    }
    if (url.hostname === 'api.day.app') {
      call.payload = await request.json();
      if (barkFailures-- > 0) return Response.json({code:500,message:'temporary failure'}, {status:500});
      return Response.json({code:200});
    }
    if (url.hostname === 'api.telegram.org') {
      call.payload = await request.json();
      return Response.json({ok:true,result:{message_id:1}});
    }
    throw new Error(`Unexpected outbound request: ${url.origin}`);
  },
}));
function jwt(scope = 'api', sub = 'local') {
  const now = Math.floor(Date.now()/1000);
  const unsigned = [JSON.stringify({alg:'HS256',typ:'JWT'}),JSON.stringify({sub,scp:scope,iat:now,exp:now+3600})].map(s=>Buffer.from(s).toString('base64url')).join('.');
  return `${unsigned}.${createHmac('sha256',secret).update(unsigned).digest('base64url')}`;
}
const authorization = `Bearer ${jwt()}`;
async function request(path, options={}) { return mf.dispatchFetch(`https://mailflare.test${path}`, { ...options, headers: { authorization, ...options.headers } }); }
async function data(path, options={}) { const response = await request(path,options); const result=await response.json(); assert.equal(response.status,200,JSON.stringify(result)); assert.equal(result.ok,true); return result.data; }
const settings = {r2_account_id:'test',r2_access_key_id:'local',r2_secret_access_key:'local-secret',r2_bucket:'mail',telegram_enabled:false,bark_enabled:false};
try {
  const db = await mf.getD1Database('DB');
  for (const name of (await readdir('migrations')).sort()) {
    const sql = await readFile(`migrations/${name}`,'utf8');
    await db.exec(sql.replace(/--[^\n]*/g,'').replace(/\s+/g,' '));
  }
  await db.prepare("INSERT INTO configs (key,value) VALUES ('settings',?1)").bind(JSON.stringify(settings)).run();
  const worker = await mf.getWorker();
  const cron = async () => {
    assert.equal((await worker.scheduled({cron:'* * * * *'})).outcome,'ok');
  };
  const sql = async (query, ...values) => db.prepare(query).bind(...values).run();
  const count = async table => (await db.prepare(`SELECT count(*) AS c FROM ${table}`).first()).c;
  const setSettings = async () => sql("UPDATE configs SET value = ?1 WHERE key = 'settings'",JSON.stringify(settings));
  const deliver = async (raw, recipient = 'to@example.test') => {
    const params = new URLSearchParams({ from: 'sender@example.test', to: recipient, format: 'json' });
    const response = await mf.dispatchFetch(`http://localhost/cdn-cgi/local/email?${params}`, {method:'POST',body:raw});
    assert.equal(response.status,200,await response.text());
  };
  const mime = (id, otp = false) => [
    'From: sender@example.test','To: to@example.test',`Message-ID: <${id}@example.test>`,
    'Date: Wed, 1 Jan 2020 00:00:00 +0000',
    `Subject: ${otp ? 'Verification code 482913' : id}`, 'MIME-Version: 1.0',
    'Content-Type: multipart/mixed; boundary="test-boundary"','',
    '--test-boundary','Content-Type: text/plain; charset=utf-8','',otp ? 'Your verification code is 482913' : 'hello',
    '--test-boundary','Content-Type: application/octet-stream','Content-Disposition: attachment; filename="sample.txt"','Content-Transfer-Encoding: base64','','aGVsbG8gYXR0YWNobWVudA==','--test-boundary--','',
  ].join('\r\n');
  const emailId = async subject => (await db.prepare('SELECT id FROM emails WHERE subject = ?1').bind(subject).first()).id;
  const forceDue = async table => sql(`UPDATE ${table} SET available_at = 0, lease_until = 0`);

  // 已发布的 0001 升级到最终 0002，保留已有邮件并加入全部新结构。
  const legacy = await mf.getD1Database('LEGACY');
  const executeFile = async (database,path) => database.exec((await readFile(path,'utf8')).replace(/--[^\n]*/g,'').replace(/\s+/g,' '));
  await executeFile(legacy,'migrations/0001_init.sql');
  await legacy.exec("INSERT INTO emails (id,subject) VALUES ('legacy','keep me');");
  await legacy.prepare('INSERT INTO emails (id,message_id,recipient,subject) VALUES (?1,?2,?3,?4)')
    .bind('before-upgrade','shared@example.test','a@example.test','keep delivery').run();
  const oldColumns = 'id,message_id,sender,recipient,subject,text_body,html_body,attachments,is_read,created_at';
  const beforeUpgrade = (await legacy.prepare(`SELECT ${oldColumns} FROM emails ORDER BY id`).all()).results;
  await executeFile(legacy,'migrations/0002_reliability.sql');
  assert.deepEqual((await legacy.prepare(`SELECT ${oldColumns} FROM emails ORDER BY id`).all()).results,beforeUpgrade);
  assert.deepEqual(await legacy.prepare("SELECT push_status,raw_key,storage_status,deleted_at FROM emails WHERE id='legacy'").first(),
    { push_status:null, raw_key:null, storage_status:'complete', deleted_at:null });
  const schema = async database => (await database.prepare("SELECT type,name,sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' AND name NOT LIKE '_cf_%' ORDER BY type,name").all()).results;
  assert.deepEqual(await schema(legacy),await schema(db),'新库和已有 0001 数据库升级后结构一致');
  const legacyInsert = (id,messageId,recipient) => legacy.prepare('INSERT OR IGNORE INTO emails (id,message_id,recipient) VALUES (?1,?2,?3)').bind(id,messageId,recipient).run();
  assert.equal((await legacyInsert('second-alias','shared@example.test','b@example.test')).meta.changes,1);
  assert.equal((await legacyInsert('duplicate-alias','shared@example.test','a@example.test')).meta.changes,0);
  for (const [prefix,messageId] of [['missing',null],['empty','']]) {
    assert.equal((await legacyInsert(`${prefix}-1`,messageId,'a@example.test')).meta.changes,1);
    assert.equal((await legacyInsert(`${prefix}-2`,messageId,'a@example.test')).meta.changes,1);
  }
  console.log('通过：全新建库、0001 升级到 0002、历史邮件保留及新字段默认值');
  console.log('通过：旧库去重索引升级保留数据、多收件地址隔离、缺失 Message-ID 不误判');

  // 同源并发失败必须原子限流；其他来源仍可登录。
  const login = (ip,password='bad') => worker.fetch('https://mailflare.test/api/auth/login', {method:'POST',headers:{'content-type':'application/json','cf-connecting-ip':ip},body:JSON.stringify({username:'local',password})});
  const attempts = await Promise.all(Array.from({length:12},()=>login('192.0.2.1')));
  assert.equal(attempts.filter(r=>r.status===403).length,5);
  assert.equal(attempts.filter(r=>r.status===429).length,7);
  assert.equal((await login('192.0.2.2','local-password')).status,200);
  await sql("UPDATE login_attempts SET reset_at=0");
  assert.equal((await login('192.0.2.1','local-password')).status,200);
  await sql("UPDATE configs SET value='broken' WHERE key='settings'");
  assert.equal((await request('/api/auth/config')).status,500);
  await setSettings();
  console.log('通过：并发登录限流、来源隔离、窗口过期、配置故障不降级');

  failAttachment = true;
  const raw = mime('recover');
  await deliver(raw);
  const id = await emailId('recover');
  const pending = await data(`/api/emails/${id}/export`);
  assert.equal(pending.storage_status,'pending');
  assert.equal(pending.attachments[0].stored,false);
  assert.equal(await count('inbound_jobs'),1);
  assert.equal(await (await request(`/api/originals/${id}`)).text(),raw);
  assert.equal((await mf.dispatchFetch(`https://mailflare.test/api/originals/${id}`)).status,401);
  const ticket = jwt('dl',id);
  assert.equal((await mf.dispatchFetch(`https://mailflare.test/api/originals/${id}?token=${ticket}`)).status,200);
  assert.equal((await mf.dispatchFetch(`https://mailflare.test/api/originals/${id}?token=${jwt('dl','another')}`)).status,403);
  failAttachment = false;
  await forceDue('inbound_jobs');
  await Promise.all([cron(),cron()]);
  const recovered = await data(`/api/emails/${id}/export`);
  assert.equal(recovered.storage_status,'complete');
  assert.equal(recovered.attachments[0].stored,true);
  assert.equal(await count('inbound_jobs'),0);
  assert.equal(await (await request(`/api/attachments/${id}_0`)).text(),'hello attachment');
  const uploads = calls.filter(c=>c.method==='PUT').length;
  await deliver(raw);
  assert.equal(await count('emails'),1);
  assert.equal(calls.filter(c=>c.method==='PUT').length,uploads);
  console.log('通过：原文留存、附件失败恢复、并发任务领取、重复投递、下载鉴权');

  // 附件成功但邮件 INSERT 失败：对象和恢复任务必须仍然有归属。
  await sql("CREATE TRIGGER inject_failure BEFORE INSERT ON emails WHEN NEW.subject = 'db-failure' BEGIN SELECT RAISE(FAIL, 'injected D1 failure'); END");
  await deliver(mime('db-failure'));
  assert.equal(await count('inbound_jobs'),1);
  assert.equal(await count('emails'),1);
  await sql('DROP TRIGGER inject_failure');
  await forceDue('inbound_jobs');
  await cron();
  assert.equal(await count('emails'),2);
  assert.equal(await count('inbound_jobs'),0);
  console.log('通过：原文保存后的 D1 写入故障可恢复');

  // 成功渠道不重复发送；按实际接收时间过期，不信任发件人 Date。
  settings.telegram_enabled=true;
  settings.telegram_bot_token='local-token'; settings.telegram_chat_id='local-chat';
  settings.bark_enabled=true; settings.bark_device_key='local-key';
  await setSettings();
  barkFailures=1;
  await deliver(mime('otp',true));
  const otpId=await emailId('Verification code 482913');
  // email waitUntil 可以异步完成；定时触发也必须遵守租约。
  await cron();
  let jobs=(await db.prepare('SELECT * FROM notification_jobs ORDER BY channel').all()).results;
  assert.equal(jobs.find(j=>j.channel==='telegram').status,'ok');
  assert.equal(jobs.find(j=>j.channel==='bark').status,'pending');
  const telegramCalls=calls.filter(c=>c.url.includes('api.telegram.org')).length;
  await forceDue('notification_jobs');
  await Promise.all([cron(),cron()]);
  jobs=(await db.prepare('SELECT * FROM notification_jobs ORDER BY channel').all()).results;
  assert(jobs.every(j=>j.status==='ok'));
  assert.equal(calls.filter(c=>c.url.includes('api.telegram.org')).length,telegramCalls);
  assert.equal((await data(`/api/emails/${otpId}/export`)).push_status,'ok');
  await sql("UPDATE notification_jobs SET status='pending',attempts=3,available_at=0 WHERE channel='bark'");
  const beforeLimit=calls.length;
  await cron();
  assert.equal(calls.length,beforeLimit);
  assert.equal((await data(`/api/emails/${otpId}/export`)).push_status,'failed');
  await sql("UPDATE notification_jobs SET status='pending',attempts=0,available_at=0,expires_at=0 WHERE channel='bark'");
  await cron();
  assert.equal(calls.length,beforeLimit);
  console.log('通过：通知分渠道重试、成功不重发、最多 3 轮、10 分钟过期');

  const objectCount=objects.size;
  await data(`/api/emails/${id}`,{method:'DELETE'});
  assert(!(await data('/api/emails')).items.some(item=>item.id===id));
  assert((await data('/api/emails?filter=trash')).items.some(item=>item.id===id));
  assert.equal(objects.size,objectCount);
  assert.equal((await data(`/api/emails/${id}`)).is_read,false,'回收站详情不改变已读状态');
  await data(`/api/emails/${id}/restore`,{method:'POST'});
  assert((await data('/api/emails')).items.some(item=>item.id===id));
  assert.equal((await request(`/api/emails/${id}/purge`,{method:'DELETE'})).status,400);
  await data(`/api/emails/${id}`,{method:'DELETE'});

  // 完整导出包含回收站，原文和附件可逐字节核对；不修改已读状态。
  const backupDir=join(await mkdtemp(join(tmpdir(),'mailflare-backup-test-')),'snapshot');
  const { spawn }=await import('node:child_process');
  const baseUrl=String(await mf.ready);
  const exit=await new Promise((resolve,reject)=>{
    const child=spawn('bun',['run','scripts/backup.ts',backupDir],{env:{...process.env,MAILFLARE_URL:baseUrl,MAILFLARE_TOKEN:jwt()},stdio:'inherit'});
    child.once('error',reject); child.once('exit',resolve);
  });
  assert.equal(exit,0);
  const manifest=JSON.parse(await readFile(join(backupDir,'manifest.json'),'utf8'));
  assert.equal(manifest.count,3);
  assert.equal(await readFile(join(backupDir,id,'original.eml'),'utf8'),raw);
  assert.equal(await readFile(join(backupDir,id,'attachment-0'),'utf8'),'hello attachment');
  assert.equal((await data(`/api/emails/${id}/export`)).is_read,false);

  failDelete=true;
  assert.equal((await request(`/api/emails/${id}/purge`,{method:'DELETE'})).status,500);
  assert.equal((await data(`/api/emails/${id}/export`)).storage_status,'purging');
  assert.equal((await request(`/api/emails/${id}/restore`,{method:'POST'})).status,400);
  failDelete=false;
  await data(`/api/emails/${id}/purge`,{method:'DELETE'});
  assert.equal((await request(`/api/emails/${id}/export`)).status,404);
  assert(![...objects.keys()].some(key=>key.includes(id)));
  console.log('通过：回收站、恢复、彻底删除失败可续清理、备份');

  // MIME To 仍是 to@example.test，实际信封投给不同别名，必须分别保存。
  const aliasRaw = mime('multi-recipient');
  const aliasRows = async () => (await db.prepare("SELECT id,recipient FROM emails WHERE message_id='multi-recipient@example.test' ORDER BY recipient").all()).results;
  const beforeAliases = objects.size;
  await Promise.all([deliver(aliasRaw,'a@example.test'),deliver(aliasRaw,'b@example.test')]);
  const aliases = await aliasRows();
  assert.deepEqual(aliases.map(row=>row.recipient),['a@example.test','b@example.test']);
  assert.notEqual(aliases[0].id,aliases[1].id);
  assert.equal(objects.size,beforeAliases+4,'两个别名各有原文与附件');
  for (const alias of aliases) {
    assert.equal(await (await request(`/api/originals/${alias.id}`)).text(),aliasRaw);
    assert.equal(await (await request(`/api/attachments/${alias.id}_0`)).text(),'hello attachment');
    await deliver(aliasRaw,alias.recipient);
    // SMTP 重投可能改变 Received 等头；仍由 Message-ID + 信封收件人去重。
    await deliver(`Received: from retry.example.test\r\n${aliasRaw}`,alias.recipient);
  }
  assert.deepEqual(await aliasRows(),aliases);
  assert.equal(objects.size,beforeAliases+4,'重复投递清理新对象，不影响其他别名');
  assert.equal(await count('inbound_jobs'),0);

  failAttachment=true;
  await deliver(aliasRaw,'c@example.test');
  failAttachment=false;
  const thirdAlias=(await aliasRows()).find(row=>row.recipient==='c@example.test');
  assert(thirdAlias);
  assert.equal((await data(`/api/emails/${thirdAlias.id}/export`)).storage_status,'pending');
  await forceDue('inbound_jobs');
  await cron();
  assert.equal((await data(`/api/emails/${thirdAlias.id}/export`)).storage_status,'complete');
  assert.equal(await count('inbound_jobs'),0);

  await data(`/api/emails/${aliases[0].id}`,{method:'DELETE'});
  await data(`/api/emails/${aliases[0].id}/purge`,{method:'DELETE'});
  for (const alias of [aliases[1],thirdAlias]) {
    assert.equal(await (await request(`/api/originals/${alias.id}`)).text(),aliasRaw);
    assert.equal(await (await request(`/api/attachments/${alias.id}_0`)).text(),'hello attachment');
  }
  assert.equal((await aliasRows()).length,2);
  console.log('通过：同信多别名并发投递、同地址重投去重、附件恢复与删除互不影响');

  // 走真实 MIME -> D1 -> 详情/通知两条路径，外发仍完全由本地 mock 接管。
  for (const [name, subject, plain, html, expected] of [
    ['otp-four', 'Your verification code is 0042', 'Login requested', null, '0042'],
    ['otp-html', 'Sign in', null, '<p>Verification code</p><div>482913</div><div>10 minutes</div>', '482913'],
    ['otp-fallback', 'Login requested', 'Please use an HTML email client.', '<p>Verification code: 123&nbsp;456</p>', '123456'],
    ['otp-unicode', 'Login requested', null, '<p>A&B中文测试邮件</p><p>验证码：4937</p>', '4937'],
    ['otp-order', 'Login requested', '482913 是您的验证码，998877 是订单号', null, '482913'],
    ['otp-postal', 'Your postal code is 100001', 'Delivery address updated', null, null],
    ['otp-coupon', 'Coupon code: SAVE20', 'Save on your order', null, null],
    ['otp-ambiguous', 'Login requested', '验证码：111222；验证码：333444', null, null],
  ]) {
    const headers = ['From: sender@example.test', 'To: to@example.test', `Message-ID: <${name}@example.test>`, `Subject: ${subject}`, 'MIME-Version: 1.0'];
    const parts = [];
    if (plain !== null) parts.push(['Content-Type: text/plain; charset=utf-8', '', plain]);
    if (html !== null) parts.push(['Content-Type: text/html; charset=utf-8', '', html]);
    const raw = parts.length === 1 ? [...headers,...parts[0]].join('\r\n')
      : [...headers,'Content-Type: multipart/alternative; boundary="otp-test"','',...parts.flatMap(part=>['--otp-test',...part]),'--otp-test--',''].join('\r\n');
    const before = calls.length;
    await deliver(raw);
    await cron();
    const row = await db.prepare('SELECT id FROM emails WHERE message_id=?1').bind(`${name}@example.test`).first();
    const detail = await data(`/api/emails/${row.id}`);
    assert.equal(detail.otp_code,expected,name);
    const jobs = (await db.prepare('SELECT status FROM notification_jobs WHERE email_id=?1').bind(row.id).all()).results;
    const sent = calls.slice(before).filter(call=>call.payload);
    if (expected !== null) {
      assert.equal(jobs.length,2,name);
      assert(jobs.every(job=>job.status==='ok'),name);
      assert.equal(sent.length,2,name);
      for (const call of sent) assert((call.payload.title ?? call.payload.text).includes(expected),name);
    } else {
      assert.equal(jobs.length,0,name);
      assert.equal(sent.length,0,name);
    }
  }
  console.log('通过：4 位码、HTML 数字边界、纯文本到 HTML 回退、中文实体、详情与通知一致、邮编/优惠码/歧义不推送');

  // 广告邮件没有验证码，但旧版仍会扫描整段正文，在 & 后的中文上越界。
  // 将触发字符放在摘要范围之外，复现“列表可见，详情打不开”的内容形态。
  const newsletterText = 'Product updates and tips. '.repeat(30) + 'A&B中文测试邮件';
  const newsletterHtml = `<html><head><style>${'.newsletter { color: #123456; }'.repeat(1000)}</style></head><body><p>${newsletterText}</p>${'<div>更多产品介绍与使用提示。</div>'.repeat(500)}</body></html>`;
  const newsletterRaw = ['From: newsletter@example.test','To: to@example.test','Message-ID: <newsletter-unicode@example.test>',
    'Subject: Product newsletter','MIME-Version: 1.0','Content-Type: multipart/alternative; boundary="newsletter"','',
    '--newsletter','Content-Type: text/plain; charset=utf-8','',newsletterText,
    '--newsletter','Content-Type: text/html; charset=utf-8','',newsletterHtml,'--newsletter--',''].join('\r\n');
  await deliver(newsletterRaw);
  const newsletterId = await emailId('Product newsletter');
  assert((await data('/api/emails?q=Product%20newsletter')).items.some(email=>email.id===newsletterId && email.preview.includes('Product updates')));
  const newsletter = await data(`/api/emails/${newsletterId}`);
  assert.equal(newsletter.text_body, newsletterText);
  assert.equal(newsletter.html_body, newsletterHtml);
  assert.equal(newsletter.otp_code, null);
  assert.equal((await db.prepare('SELECT count(*) AS c FROM notification_jobs WHERE email_id=?1').bind(newsletterId).first()).c,0);
  console.log('通过：新收长广告邮件的列表摘要与详情、正文后部中文 & 字符、无验证码不推送');
} finally { await mf.dispose(); }
