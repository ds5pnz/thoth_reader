import { useEffect, useState } from 'react';
import { ModalBackdrop } from '../../app/Modal';
import { call, errorMessage, mutate } from '../../ipc/client';
import { useSettings } from '../../app/settings';
import type { ProviderRow } from '../../ipc/generated/ProviderRow';
import type { ProbeResult } from '../../ipc/generated/ProbeResult';
import type { ProfileRow } from '../../ipc/generated/ProfileRow';
import type { ChatgptSignInResult } from '../../ipc/generated/ChatgptSignInResult';
import type { CodexStatus } from '../../ipc/generated/CodexStatus';
import './settings.css';

type Tab = 'translation' | 'reading' | 'glossary' | 'data';

export type ProviderVariant = 'ollama' | 'openai_compatible' | 'chatgpt' | 'codex';
/** The provider's real kind: account-backed kinds are stored as a variant of `openai_compatible`. */
export function variantOf(p: Pick<ProviderRow, 'kind' | 'capabilities'>): ProviderVariant {
  const v = (p.capabilities as { variant?: unknown } | null)?.variant;
  if (v === 'chatgpt' || v === 'codex') return v;
  return p.kind === 'ollama' ? 'ollama' : 'openai_compatible';
}
const VARIANT_LABEL: Record<ProviderVariant, string> = { ollama: 'Ollama', openai_compatible: 'OpenAI 호환', chatgpt: 'ChatGPT 요금제', codex: 'Codex' };
const CHATGPT_USAGE_URL = 'https://chatgpt.com/settings/usage';
interface AccountInfo { email?: string | null; signedIn?: boolean; planEnabled?: boolean; plan?: string | null; type?: string | null }
const accountOf = (p: ProviderRow): AccountInfo => ((p.capabilities as { account?: AccountInfo } | null)?.account ?? {});
const openUrl = (url: string) => void call('open_external', { url }).catch(() => {});

export function SettingsDialog({ onClose }: { onClose: () => void }) {
  const [tab, setTab] = useState<Tab>('translation');
  return (
    <ModalBackdrop role="dialog" aria-labelledby="settings-title" onClose={onClose}>
      <div className="modal settings">
        <header>
          <h2 id="settings-title">설정</h2>
          <div className="seg" role="tablist">
            {([['translation', '번역'], ['reading', '읽기'], ['glossary', '용어집'], ['data', '데이터']] as [Tab, string][]).map(([k, l]) => (
              <button key={k} role="tab" aria-selected={tab === k} aria-pressed={tab === k} onClick={() => setTab(k)}>{l}</button>
            ))}
          </div>
          <button className="btn ghost small" onClick={onClose} aria-label="닫기">닫기</button>
        </header>
        <div className="body">
          {tab === 'translation' && <TranslationSettings />}
          {tab === 'reading' && <ReadingSettings />}
          {tab === 'glossary' && <GlossarySettings />}
          {tab === 'data' && <DataSettings />}
        </div>
      </div>
    </ModalBackdrop>
  );
}

function TranslationSettings() {
  const { settings, set } = useSettings();
  const [providers, setProviders] = useState<ProviderRow[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [probe, setProbe] = useState<Record<string, ProbeResult | 'running'>>({});
  const [err, setErr] = useState<string | null>(null);
  const [consentFor, setConsentFor] = useState<ProviderRow | null>(null);
  const [adding, setAdding] = useState<null | 'ollama' | 'openai_compatible'>(null);
  const [signingIn, setSigningIn] = useState(false);
  const [welcome, setWelcome] = useState(false);

  // "Continue with ChatGPT": the browser opens and this waits until the user is done there.
  const signIn = async (providerId: string | null) => {
    setErr(null);
    setSigningIn(true);
    try {
      const r = await call<ChatgptSignInResult>('chatgpt_sign_in', { providerId });
      await load();
      setSelected(r.provider.id);
      if (!r.planEnabled) setErr('로그인은 되었지만 ChatGPT 요금제 사용이 허용되지 않았습니다. 번역에 쓰려면 다시 로그인해 요금제 사용에 동의해 주세요.');
      else if (r.newRegistration) setWelcome(true);
    } catch (e) {
      if ((e as { code?: string })?.code !== 'CANCELLED') setErr(errorMessage(e));
    } finally { setSigningIn(false); }
  };
  const connectCodex = async () => {
    setErr(null);
    try {
      const p = await call<ProviderRow>('codex_connect');
      await load();
      setSelected(p.id);
    } catch (e) { setErr(errorMessage(e)); }
  };

  const load = async () => {
    try {
      const p = await call<ProviderRow[]>('provider_list');
      setProviders(p);
      if (!selected && p[0]) setSelected(p[0].id);
    } catch (e) { setErr(errorMessage(e)); }
  };
  useEffect(() => { void load(); }, []); // eslint-disable-line react-hooks/exhaustive-deps

  const runProbe = async (p: ProviderRow) => {
    setProbe((x) => ({ ...x, [p.id]: 'running' }));
    try {
      const r = await call<ProbeResult>('provider_probe', { providerId: p.id, modelId: null });
      setProbe((x) => ({ ...x, [p.id]: r }));
    } catch (e) {
      setErr(errorMessage(e));
      setProbe((x) => { const y = { ...x }; delete y[p.id]; return y; });
    }
  };

  const enable = async (p: ProviderRow, enabled: boolean) => {
    // First use of a remote endpoint requires explicit consent (spec §1.2).
    const isLocal = p.endpoint.startsWith('http://127.0.0.1') || p.endpoint.startsWith('http://localhost') || p.endpoint.startsWith('http://[::1]');
    if (enabled && !isLocal && consentFor?.id !== p.id) { setConsentFor(p); return; }
    try {
      await mutate('provider_save', { providerId: p.id, enabled });
      setConsentFor(null);
      await load();
    } catch (e) { setErr(errorMessage(e)); }
  };

  const sel = providers.find((p) => p.id === selected) ?? null;
  return (
    <div className="settings-grid">
      <section>
        <h3>Provider</h3>
        {err && <p className="badge danger" role="alert">{err}</p>}
        <ul className="prov-list">
          {providers.map((p) => (
            <li key={p.id} className={p.id === selected ? 'sel' : ''}>
              <button className="prov-pick" onClick={() => setSelected(p.id)}>
                <strong>{p.label}</strong>
                <span className="faint">{VARIANT_LABEL[variantOf(p)]} · {variantOf(p) === 'chatgpt' || variantOf(p) === 'codex' ? (accountOf(p).email ?? p.endpoint) : p.endpoint}</span>
              </button>
              <label className="switch">
                <input type="checkbox" checked={p.enabled} onChange={(e) => void enable(p, e.target.checked)} /> 활성
              </label>
            </li>
          ))}
        </ul>
        <div className="row">
          <button className="btn small" onClick={() => setAdding('ollama')}>Ollama 추가</button>
          <button className="btn small" onClick={() => setAdding('openai_compatible')}>OpenAI 호환 API 추가</button>
        </div>
        <div className="card account-add">
          <h4>ChatGPT 요금제로 번역</h4>
          <p className="faint">ChatGPT 요금제나 크레딧에 포함된 사용량으로 번역합니다. API 키가 필요 없고, 앱 구독이나 별도 요금은 없습니다. 번역할 문단은 OpenAI로 전송됩니다.</p>
          <div className="row">
            {signingIn
              ? <><span className="muted">브라우저에서 로그인을 마쳐 주세요…</span><button className="btn small" onClick={() => void call('chatgpt_sign_in_cancel').catch(() => {})}>취소</button></>
              : <button className="btn small primary" onClick={() => void signIn(null)}>Continue with ChatGPT</button>}
            <button className="btn small" onClick={() => void connectCodex()} title="이 Mac에 설치되어 로그인된 Codex를 통해 번역합니다">Codex 연결</button>
          </div>
        </div>
        {welcome && (
          <div className="consent welcome" role="alertdialog" aria-labelledby="welcome-title">
            <h4 id="welcome-title">ChatGPT 요금제를 사용합니다</h4>
            <p>이 앱의 번역 요청은 ChatGPT 요금제 사용량을 씁니다. 사용량과 이 앱의 한도는 ChatGPT 설정에서 관리할 수 있습니다.</p>
            <div className="row">
              <button className="btn" onClick={() => openUrl(CHATGPT_USAGE_URL)}>사용량 관리</button>
              <button className="btn primary" onClick={() => setWelcome(false)}>확인</button>
            </div>
          </div>
        )}
        {adding && <AddProvider kind={adding} onDone={async (id) => { setAdding(null); await load(); if (id) setSelected(id); }} />}
        {consentFor && (
          <div className="consent" role="alertdialog" aria-labelledby="consent-title">
            <h4 id="consent-title">외부 API로 본문이 전송됩니다</h4>
            <p>활성화하면 화면에 보이는 문단과 앞 문단·제목 문맥이 다음 주소로 전송됩니다.</p>
            <code>{consentFor.endpoint}</code>
            <p className="muted">{variantOf(consentFor) === 'codex' ? '이 Mac의 Codex를 통해 OpenAI로 전송되며, 사용량은 Codex 쪽 한도에서 차감됩니다.' : variantOf(consentFor) === 'chatgpt' ? 'OpenAI로 전송되며, 사용량은 ChatGPT 요금제에서 차감됩니다. 로그인 정보는 macOS 키체인에만 저장됩니다.' : '로컬 Ollama가 실패해도 외부 API로 자동 전환하지 않습니다. API 키는 macOS 키체인에만 저장됩니다.'}</p>
            <div className="row">
              <button className="btn" onClick={() => setConsentFor(null)}>취소</button>
              <button className="btn primary" onClick={() => void enable(consentFor, true)}>이해했으며 활성화</button>
            </div>
          </div>
        )}
      </section>
      {sel && (
        <section>
          <ProviderDetail key={sel.id} provider={sel} probe={probe[sel.id]} onProbe={() => void runProbe(sel)} onChanged={load} onSignIn={() => void signIn(sel.id)} signingIn={signingIn} />
          <ProfileEditor provider={sel} probe={probe[sel.id]} activeProfileId={settings.activeProfileId ?? null} onActivate={(id) => void set('activeProfileId', id)} />
        </section>
      )}
    </div>
  );
}

function AddProvider({ kind, onDone }: { kind: 'ollama' | 'openai_compatible'; onDone: (id: string | null) => void }) {
  const [label, setLabel] = useState(kind === 'ollama' ? 'Ollama (로컬)' : '');
  const [endpoint, setEndpoint] = useState(kind === 'ollama' ? 'http://127.0.0.1:11434' : 'https://');
  const [ns, setNs] = useState(kind === 'ollama' ? 'local-default' : 'default');
  const [secret, setSecret] = useState('');
  const [allowPrivate, setAllowPrivate] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const save = async () => {
    try {
      const r = await mutate<ProviderRow>('provider_save', { providerId: null, kind, label, endpoint, accountNamespace: ns, allowPrivateHttp: allowPrivate, enabled: kind === 'ollama', newSecret: secret || undefined });
      setSecret('');
      onDone(r.id);
    } catch (e) { setErr(errorMessage(e)); }
  };
  return (
    <div className="card">
      <label>이름<input className="input" value={label} onChange={(e) => setLabel(e.target.value)} /></label>
      <label>Endpoint{kind === 'openai_compatible' && <span className="faint"> (예: https://host/v1)</span>}<input className="input" value={endpoint} onChange={(e) => setEndpoint(e.target.value)} spellCheck={false} /></label>
      <label>계정 namespace<span className="faint"> (키를 바꿔도 같은 namespace면 캐시 유지)</span><input className="input" value={ns} onChange={(e) => setNs(e.target.value)} /></label>
      {kind === 'openai_compatible' && <label>API 키<input className="input" type="password" autoComplete="off" value={secret} onChange={(e) => setSecret(e.target.value)} /></label>}
      <label className="switch"><input type="checkbox" checked={allowPrivate} onChange={(e) => setAllowPrivate(e.target.checked)} /> 사설망 HTTP endpoint 허용</label>
      {err && <p className="badge danger">{err}</p>}
      <div className="row"><button className="btn" onClick={() => onDone(null)}>취소</button><button className="btn primary" onClick={() => void save()}>저장</button></div>
    </div>
  );
}

function ProviderDetail({ provider, probe, onProbe, onChanged, onSignIn, signingIn }: { provider: ProviderRow; probe: ProbeResult | 'running' | undefined; onProbe: () => void; onChanged: () => Promise<void>; onSignIn: () => void; signingIn: boolean }) {
  const [secret, setSecret] = useState('');
  const [msg, setMsg] = useState<string | null>(null);
  const variant = variantOf(provider);
  const account = accountOf(provider);
  const [codex, setCodex] = useState<CodexStatus | null>(null);
  useEffect(() => {
    if (variant !== 'codex') return;
    let live = true;
    void call<CodexStatus>('codex_status').then((s) => { if (live) setCodex(s); }).catch(() => {});
    return () => { live = false; };
  }, [variant, provider.id]);
  const signOut = async () => {
    try {
      const r = await call<{ revoked: boolean }>('chatgpt_sign_out', { providerId: provider.id });
      setMsg(r.revoked ? '로그아웃했습니다.' : '이 Mac에서는 로그아웃했지만 OpenAI 쪽 세션 종료는 확인하지 못했습니다. ChatGPT 설정에서 이 앱의 연결을 끊을 수 있습니다.');
      await onChanged();
    } catch (e) { setMsg(errorMessage(e)); }
  };
  const saveKey = async () => {
    try {
      await mutate('provider_save', { providerId: provider.id, newSecret: secret });
      setSecret('');
      setMsg('키를 키체인에 저장했습니다.');
      await onChanged();
    } catch (e) { setMsg(errorMessage(e)); }
  };
  const newEpoch = async () => {
    try {
      await mutate('provider_new_cache_epoch', { providerId: provider.id });
      setMsg('이 Provider의 캐시를 새로 시작합니다. 이전 번역은 이력에 남습니다.');
      await onChanged();
    } catch (e) { setMsg(errorMessage(e)); }
  };
  return (
    <div className="card">
      <h3>{provider.label}</h3>
      <p className="faint">{provider.endpoint} · namespace {provider.accountNamespace} · 캐시 epoch {provider.cacheEpoch}</p>
      {variant === 'chatgpt' && (
        <div className="account">
          <div className="row">
            <span className={`badge ${account.signedIn && account.planEnabled ? 'ok' : 'warn'}`}>{account.signedIn ? (account.planEnabled ? 'ChatGPT 요금제 사용 중' : '요금제 사용 미허용') : '로그아웃됨'}</span>
            {account.email && <span className="muted">{account.email}</span>}
          </div>
          <div className="row">
            <button className="btn small" onClick={() => openUrl(CHATGPT_USAGE_URL)}>사용량 관리</button>
            <button className="btn small" disabled={signingIn} onClick={onSignIn}>{account.signedIn ? '다시 로그인' : 'Continue with ChatGPT'}</button>
            {account.signedIn && <button className="btn small ghost" onClick={() => void signOut()}>로그아웃</button>}
          </div>
          <p className="faint small-note">사용량은 ChatGPT 요금제에서 차감되며, 한도에 닿으면 번역이 멈춥니다. 다른 결제 수단으로 자동 전환하지 않습니다. 이 앱은 ChatGPT 대화 내용에 접근하지 않습니다.</p>
        </div>
      )}
      {variant === 'codex' && (
        <div className="account">
          <div className="row">
            <span className={`badge ${codex ? (codex.installed && codex.signedIn ? 'ok' : 'warn') : ''}`}>{!codex ? 'Codex 확인 중…' : !codex.installed ? 'Codex를 찾을 수 없음' : codex.signedIn ? 'Codex 로그인됨' : 'Codex 로그인 필요'}</span>
            {codex?.email && <span className="muted">{codex.email}{codex.plan ? ` · ${codex.plan}` : ''}</span>}
          </div>
          <p className="faint small-note">이 Mac에 설치된 Codex의 로그인을 그대로 씁니다. 이 앱은 Codex의 토큰을 읽지 않으며, Codex는 명령 실행이나 파일 접근 없이 번역만 하도록 제한해서 부릅니다. 사용량은 Codex 쪽 한도에서 차감됩니다. 로그인은 터미널에서 <code>codex login</code>으로 합니다.</p>
        </div>
      )}
      {variant === 'openai_compatible' && (
        <div className="row">
          <span className={`badge ${provider.keyConfigured ? 'ok' : 'warn'}`}>{provider.keyConfigured ? '키 설정됨' : '키 없음'}</span>
          <input className="input" type="password" placeholder="새 API 키" autoComplete="off" value={secret} onChange={(e) => setSecret(e.target.value)} />
          <button className="btn small" disabled={!secret} onClick={() => void saveKey()}>키 저장</button>
        </div>
      )}
      <div className="row">
        <button className="btn small" onClick={onProbe} disabled={probe === 'running'}>{probe === 'running' ? '검사 중…' : '연결 검사'}</button>
        <button className="btn small ghost" onClick={() => void newEpoch()}>캐시 새로 시작</button>
      </div>
      <p className="faint small-note">{variant === 'chatgpt' || variant === 'codex' ? '연결 검사는 모델 목록만 확인하며 문서 본문이나 시험 문구를 보내지 않습니다.' : '연결 검사는 문서 본문을 보내지 않으며, 응답 형식 확인을 위해 짧은 고정 문구를 보낼 수 있습니다.'}</p>
      {probe && probe !== 'running' && (
        probe.ok ? <p className="badge ok">연결됨 · 모델 {probe.models.length}개{probe.sentTestPrompt ? ' · 고정 문구 검사 수행' : ''}</p>
          : <p className="badge danger">{probe.error?.message ?? '연결 실패'}</p>
      )}
      {msg && <p className="muted">{msg}</p>}
    </div>
  );
}

/** Model ids of translation-only models, which need the simple request. */
export function isTranslationModel(id: string): boolean {
  return /translategemma|hy-?mt|hunyuan-?mt|madlad|nllb|seamless|opus-?mt|towerinstruct|(^|[^a-z])mt\d*([^a-z]|$)/i.test(id);
}

export type PromptModeName = 'structured' | 'simple' | 'rosetta';
/** Request style a model needs, judged from its id. */
export function defaultPromptMode(id: string): PromptModeName {
  if (/rosetta/i.test(id)) return 'rosetta';
  return isTranslationModel(id) ? 'simple' : 'structured';
}

function ProfileEditor({ provider, probe, activeProfileId, onActivate }: { provider: ProviderRow; probe: ProbeResult | 'running' | undefined; activeProfileId: string | null; onActivate: (id: string) => void }) {
  const { settings, set } = useSettings();
  const models = probe && probe !== 'running' ? probe.models : [];
  const [model, setModel] = useState('');
  const [src, setSrc] = useState(settings.defaultSourceLanguage ?? 'en');
  const [tgt, setTgt] = useState(settings.defaultTargetLanguage ?? 'ko');
  const [temp, setTemp] = useState('0.2');
  const [ctx, setCtx] = useState('');
  // Translation-only models answer a short plain request; chat models get the structured one.
  const [mode, setMode] = useState<PromptModeName | null>(null);
  const promptMode = mode ?? defaultPromptMode(model);
  const [msg, setMsg] = useState<string | null>(null);
  const create = async () => {
    try {
      // Re-check the chosen model (fixed test phrase only) so its capabilities —
      // streaming, structured output, verified reasoning toggle — enter the profile.
      setMsg('선택한 모델을 고정 문구로 검사하는 중…');
      await call('provider_probe', { providerId: provider.id, modelId: model.trim() }).catch(() => null);
      const m = models.find((x) => x.id === model);
      const p = await mutate<ProfileRow>('profile_create', {
        providerId: provider.id, modelId: model.trim(), sourceLanguage: src, targetLanguage: tgt,
        temperature: temp === '' ? null : Number(temp), seed: null, maxOutputTokens: null,
        contextLimit: ctx ? Number(ctx) : (m?.contextLength ?? null), modelDigest: m?.digest ?? null,
        promptMode,
      });
      onActivate(p.id);
      await set('defaultSourceLanguage', src);
      await set('defaultTargetLanguage', tgt);
      setMsg('이 설정을 현재 번역 프로필로 사용합니다. 열린 문서는 새 설정으로 갱신됩니다.');
    } catch (e) { setMsg(errorMessage(e)); }
  };
  return (
    <div className="card">
      <h3>번역 프로필</h3>
      <label>모델
        {models.length > 0 ? (
          <select className="input" value={model} onChange={(e) => setModel(e.target.value)}>
            <option value="">선택…</option>
            {models.map((m) => <option key={m.id} value={m.id}>{m.id}{m.contextLength ? ` (ctx ${m.contextLength})` : ''}</option>)}
          </select>
        ) : (
          <input className="input" placeholder="모델 ID (연결 검사로 목록을 불러올 수 있습니다)" value={model} onChange={(e) => setModel(e.target.value)} spellCheck={false} />
        )}
      </label>
      <div className="row">
        <label>원문 언어<input className="input" value={src} onChange={(e) => setSrc(e.target.value)} size={8} /></label>
        <label>대상 언어<input className="input" value={tgt} onChange={(e) => setTgt(e.target.value)} size={8} /></label>
        <label>temperature<input className="input" value={temp} onChange={(e) => setTemp(e.target.value)} size={5} /></label>
        <label>문맥 한도(토큰)<input className="input" value={ctx} onChange={(e) => setCtx(e.target.value)} size={7} placeholder="자동" /></label>
      </div>
      <label>요청 방식
        <div className="seg">
          <button type="button" aria-pressed={promptMode === 'structured'} onClick={() => setMode('structured')}>표준</button>
          <button type="button" aria-pressed={promptMode === 'simple'} onClick={() => setMode('simple')}>단순</button>
          <button type="button" aria-pressed={promptMode === 'rosetta'} onClick={() => setMode('rosetta')}>Rosetta</button>
        </div>
        <span className="faint">표준은 일반 대화형 모델용입니다. TranslateGemma, Hunyuan-MT 같은 번역 전용 모델은 단순을, YanoljaNEXT Rosetta는 Rosetta를 쓰세요(둘 다 앞 문단 문맥은 보내지 않습니다). 모델 이름으로 자동 선택되며, 바꾼 뒤 프로필 적용을 눌러야 반영됩니다.</span>
      </label>
      <label>번역 문체 (한국어)
        <div className="seg">
          <button type="button" aria-pressed={(settings.translationStyle ?? 'polite') === 'polite'} onClick={() => void set('translationStyle', 'polite')}>합니다체</button>
          <button type="button" aria-pressed={settings.translationStyle === 'plain'} onClick={() => void set('translationStyle', 'plain')}>한다체</button>
        </div>
        <span className="faint">문단마다 따로 번역하므로 문체를 고정합니다. 바꾸면 보이는 문단부터 새 문체로 다시 번역되고 이전 번역은 이력에 남습니다.</span>
      </label>
      <button className="btn primary" disabled={!model.trim()} onClick={() => void create()}>프로필 적용</button>
      {activeProfileId && <p className="faint">현재 프로필 ID {activeProfileId.slice(0, 8)}…</p>}
      {msg && <p className="muted">{msg}</p>}
    </div>
  );
}

function ReadingSettings() {
  const { settings, set } = useSettings();
  return (
    <div className="card">
      <label>테마
        <div className="seg">
          {(['system', 'light', 'dark'] as const).map((t) => (
            <button key={t} aria-pressed={(settings.theme ?? 'system') === t} onClick={() => void set('theme', t)}>{t === 'system' ? '시스템' : t === 'light' ? '밝게' : '어둡게'}</button>
          ))}
        </div>
      </label>
      <label>글자 크기 {settings.fontSize ?? 17}px
        <input type="range" min={12} max={28} value={settings.fontSize ?? 17} onChange={(e) => void set('fontSize', Number(e.target.value))} />
      </label>
      <label>줄 간격 {settings.lineHeight ?? 1.65}
        <input type="range" min={1.2} max={2.2} step={0.05} value={settings.lineHeight ?? 1.65} onChange={(e) => void set('lineHeight', Number(e.target.value))} />
      </label>
      <label>글꼴
        <select className="input" value={settings.fontFamily ?? ''} onChange={(e) => void set('fontFamily', e.target.value)}>
          <option value="">기본 (세리프)</option>
          <option value='-apple-system, "Apple SD Gothic Neo", sans-serif'>산세리프</option>
          <option value='"Nanum Myeongjo", "AppleMyungjo", serif'>명조</option>
        </select>
      </label>
      <label className="switch"><input type="checkbox" checked={settings.syncScroll ?? true} onChange={(e) => void set('syncScroll', e.target.checked)} /> PDF 원본 보기에서 번역 패널 자동 동기화</label>
      <label className="switch"><input type="checkbox" checked={settings.autoTranslate ?? true} onChange={(e) => void set('autoTranslate', e.target.checked)} /> 화면에 보이는 문단 자동 번역</label>
    </div>
  );
}

function GlossarySettings() {
  const { settings, set } = useSettings();
  const [rows, setRows] = useState(settings.glossary ?? []);
  const [msg, setMsg] = useState<string | null>(null);
  const save = async () => {
    try {
      await set('glossary', rows.filter((r) => r.source.trim()));
      setMsg('저장했습니다. 용어집이 바뀐 문단은 새 캐시 키로 번역됩니다.');
    } catch (e) { setMsg(errorMessage(e)); }
  };
  return (
    <div className="card">
      <p className="muted">문단에 실제로 등장한 용어만 요청에 포함됩니다.</p>
      <table className="gloss">
        <thead><tr><th>원문 용어</th><th>번역</th><th /></tr></thead>
        <tbody>
          {rows.map((r, i) => (
            <tr key={i}>
              <td><input className="input" value={r.source} onChange={(e) => setRows(rows.map((x, j) => (j === i ? { ...x, source: e.target.value } : x)))} /></td>
              <td><input className="input" value={r.target} onChange={(e) => setRows(rows.map((x, j) => (j === i ? { ...x, target: e.target.value } : x)))} /></td>
              <td><button className="btn small ghost" onClick={() => setRows(rows.filter((_, j) => j !== i))}>삭제</button></td>
            </tr>
          ))}
        </tbody>
      </table>
      <div className="row">
        <button className="btn small" onClick={() => setRows([...rows, { source: '', target: '' }])}>용어 추가</button>
        <button className="btn small primary" onClick={() => void save()}>저장</button>
      </div>
      {msg && <p className="muted">{msg}</p>}
    </div>
  );
}

function DataSettings() {
  const [usage, setUsage] = useState<{ translationBytes: number; dbBytes: number; warningBytes: number; overWarning: boolean } | null>(null);
  useEffect(() => { void call<typeof usage>('storage_usage').then(setUsage).catch(() => {}); }, []);
  const mb = (n: number) => `${(n / 1024 / 1024).toFixed(1)} MiB`;
  return (
    <div className="card">
      <h3>저장 공간</h3>
      {usage ? (
        <>
          <p>번역과 이력 {mb(usage.translationBytes)} · 데이터베이스 {mb(usage.dbBytes)}</p>
          {usage.overWarning && <p className="badge warn">번역 및 이력이 {mb(usage.warningBytes)}를 넘었습니다. 이력은 자동 삭제하지 않습니다.</p>}
        </>
      ) : <p className="faint">계산 중…</p>}
      <h3>개인정보</h3>
      <p className="muted">원문과 번역 데이터베이스는 이 Mac의 앱 지원 폴더에 암호화 없이 저장됩니다. API 키만 macOS 키체인에 저장합니다. 문서와 이력 삭제는 라이브러리의 문서 삭제로 수행합니다.</p>
      <p className="muted">캐시 비우기는 캐시 인덱스만 지우므로 이력 데이터가 남아 용량이 크게 줄지 않을 수 있습니다. 문서의 캐시는 리더 메뉴에서 비울 수 있습니다.</p>
    </div>
  );
}
