import { useEffect, useState } from 'react';
import { call, errorMessage, mutate } from '../../ipc/client';
import { useSettings } from '../../app/settings';
import type { ProviderRow } from '../../ipc/generated/ProviderRow';
import type { ProbeResult } from '../../ipc/generated/ProbeResult';
import type { ProfileRow } from '../../ipc/generated/ProfileRow';
import './settings.css';

type Tab = 'translation' | 'reading' | 'glossary' | 'data';

export function SettingsDialog({ onClose }: { onClose: () => void }) {
  const [tab, setTab] = useState<Tab>('translation');
  return (
    <div className="modal-backdrop" role="dialog" aria-modal="true" aria-labelledby="settings-title" onKeyDown={(e) => { if (e.key === 'Escape') onClose(); }}>
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
    </div>
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
      const r = await call<ProbeResult>('provider_probe', { providerId: p.id });
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
                <span className="faint">{p.kind === 'ollama' ? 'Ollama' : 'OpenAI 호환'} · {p.endpoint}</span>
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
        {adding && <AddProvider kind={adding} onDone={async (id) => { setAdding(null); await load(); if (id) setSelected(id); }} />}
        {consentFor && (
          <div className="consent" role="alertdialog" aria-labelledby="consent-title">
            <h4 id="consent-title">외부 API로 본문이 전송됩니다</h4>
            <p>활성화하면 화면에 보이는 문단과 앞 문단·제목 문맥이 다음 주소로 전송됩니다.</p>
            <code>{consentFor.endpoint}</code>
            <p className="muted">로컬 Ollama가 실패해도 외부 API로 자동 전환하지 않습니다. API 키는 macOS 키체인에만 저장됩니다.</p>
            <div className="row">
              <button className="btn" onClick={() => setConsentFor(null)}>취소</button>
              <button className="btn primary" onClick={() => void enable(consentFor, true)}>이해했으며 활성화</button>
            </div>
          </div>
        )}
      </section>
      {sel && (
        <section>
          <ProviderDetail key={sel.id} provider={sel} probe={probe[sel.id]} onProbe={() => void runProbe(sel)} onChanged={load} />
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

function ProviderDetail({ provider, probe, onProbe, onChanged }: { provider: ProviderRow; probe: ProbeResult | 'running' | undefined; onProbe: () => void; onChanged: () => Promise<void> }) {
  const [secret, setSecret] = useState('');
  const [msg, setMsg] = useState<string | null>(null);
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
      {provider.kind === 'openai_compatible' && (
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
      <p className="faint small-note">연결 검사는 문서 본문을 보내지 않으며, 응답 형식 확인을 위해 짧은 고정 문구를 보낼 수 있습니다.</p>
      {probe && probe !== 'running' && (
        probe.ok ? <p className="badge ok">연결됨 · 모델 {probe.models.length}개{probe.sentTestPrompt ? ' · 고정 문구 검사 수행' : ''}</p>
          : <p className="badge danger">{probe.error?.message ?? '연결 실패'}</p>
      )}
      {msg && <p className="muted">{msg}</p>}
    </div>
  );
}

function ProfileEditor({ provider, probe, activeProfileId, onActivate }: { provider: ProviderRow; probe: ProbeResult | 'running' | undefined; activeProfileId: string | null; onActivate: (id: string) => void }) {
  const { settings, set } = useSettings();
  const models = probe && probe !== 'running' ? probe.models : [];
  const [model, setModel] = useState('');
  const [src, setSrc] = useState(settings.defaultSourceLanguage ?? 'en');
  const [tgt, setTgt] = useState(settings.defaultTargetLanguage ?? 'ko');
  const [temp, setTemp] = useState('0.2');
  const [ctx, setCtx] = useState('');
  const [msg, setMsg] = useState<string | null>(null);
  const create = async () => {
    try {
      const m = models.find((x) => x.id === model);
      const p = await mutate<ProfileRow>('profile_create', {
        providerId: provider.id, modelId: model.trim(), sourceLanguage: src, targetLanguage: tgt,
        temperature: temp === '' ? null : Number(temp), seed: null, maxOutputTokens: null,
        contextLimit: ctx ? Number(ctx) : (m?.contextLength ?? null), modelDigest: m?.digest ?? null,
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
