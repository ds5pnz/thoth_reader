//! Translation service: visible-first scheduling, prefetch, cache hits, revision display,
//! generation isolation and retry policy (spec §6, §7).

use super::prepare::{self, PreparedJob, ProfileContext};
use crate::document::session::{ReadingMode, Session};
use crate::domain::{now_ms, Block, TranslationPolicy};
use crate::error::{AppError, AppResult, ErrorCode};
use crate::providers::{ProviderConfig, ProviderError, RetryClass, RetryDecision, TranslationDelta, TranslationProvider};
use crate::security::keychain::SecretStore;
use crate::storage::repo::{extraction, misc, translations};
use crate::storage::Db;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;
use ts_rs::TS;

pub const MAX_QUEUED_PER_SESSION: usize = 100;
pub const PREFETCH_BLOCKS: u32 = 3;
pub const PREFETCH_MAX_U16: u32 = 6000;
pub const GLOBAL_MAX: usize = 4;

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct JobBrief {
    pub job_id: String,
    pub state: String,
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct BlockDisplay {
    pub block_id: String,
    pub effective_policy: TranslationPolicy,
    pub user_override: Option<TranslationPolicy>,
    pub preference_version: i64,
    pub head_version: i64,
    /// none | current | stale | manual | excluded
    pub status: String,
    pub shown: Option<translations::DisplayRevision>,
    pub has_candidate: bool,
    pub job: Option<JobBrief>,
}

/// Builds a provider client from its stored config. Swappable in tests.
pub type ProviderFactory = Arc<dyn Fn(&ProviderConfig, Arc<dyn SecretStore>) -> AppResult<Arc<dyn TranslationProvider>> + Send + Sync>;

pub fn default_factory() -> ProviderFactory {
    Arc::new(|cfg, secrets| {
        let p: Arc<dyn TranslationProvider> = crate::providers::build_provider(cfg, secrets)?;
        Ok(p)
    })
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Priority {
    P0 = 0,
    P1 = 1,
    P2 = 2,
}

struct Entry {
    job_id: String,
    cache_key: String,
    block_id: String,
    priority: Priority,
    order: u64,
    session: Arc<Session>,
    generation: u64,
    prepared: Arc<PreparedJob>,
    force: bool,
    cancel: CancellationToken,
    not_before: Option<Instant>,
}

#[derive(Default)]
struct Queue {
    waiting: HashMap<String, Entry>,           // cache_key -> entry
    running: HashMap<String, (String, CancellationToken, Priority, String)>, // cache_key -> (job_id, cancel, priority, session_id)
    order: u64,
}

pub struct TranslationService {
    pub db: Db,
    pub secrets: Arc<dyn SecretStore>,
    factory: ProviderFactory,
    providers: Mutex<HashMap<String, (i64, Arc<dyn TranslationProvider>)>>,
    provider_permits: Mutex<HashMap<String, Arc<Semaphore>>>,
    global: Arc<Semaphore>,
    queue: Mutex<Queue>,
    notify: Notify,
    shutdown: CancellationToken,
    /// Providers stopped after 401/403 until the user re-checks auth.
    auth_blocked: Mutex<HashSet<String>>,
    pub http_attempts: AtomicU64,
    jitter: Arc<dyn Fn() -> f64 + Send + Sync>,
}

impl TranslationService {
    pub fn new(db: Db, secrets: Arc<dyn SecretStore>, factory: ProviderFactory) -> Arc<Self> {
        Arc::new(Self {
            db,
            secrets,
            factory,
            providers: Mutex::new(HashMap::new()),
            provider_permits: Mutex::new(HashMap::new()),
            global: Arc::new(Semaphore::new(2)),
            queue: Mutex::new(Queue::default()),
            notify: Notify::new(),
            shutdown: CancellationToken::new(),
            auth_blocked: Mutex::new(HashSet::new()),
            http_attempts: AtomicU64::new(0),
            jitter: Arc::new(crate::providers::random_jitter),
        })
    }

    pub fn with_jitter(self: Arc<Self>, j: Arc<dyn Fn() -> f64 + Send + Sync>) -> Arc<Self> {
        let mut s = Arc::try_unwrap(self).ok().expect("unique");
        s.jitter = j;
        Arc::new(s)
    }

    /// Restart recovery + dispatcher start. Must run after migrations.
    pub async fn start(self: &Arc<Self>) -> AppResult<()> {
        if self.db.read_only_reason.is_none() {
            translations::jobs_interrupt_all(&self.db.writer).await?;
        }
        let me = self.clone();
        tokio::spawn(async move { me.dispatch_loop().await });
        Ok(())
    }

    /// Stop feeding new work, then cancel requests (spec §7.3 shutdown).
    pub async fn shutdown(&self) {
        self.shutdown.cancel();
        let running: Vec<(String, CancellationToken)> = self.queue.lock().unwrap().running.values().map(|(id, c, _, _)| (id.clone(), c.clone())).collect();
        for (_, c) in &running {
            c.cancel();
        }
        let waiting: Vec<String> = self.queue.lock().unwrap().waiting.values().map(|e| e.job_id.clone()).collect();
        for id in waiting.iter().chain(running.iter().map(|(id, _)| id)) {
            let _ = translations::job_cancel(&self.db.writer, id, "SHUTDOWN").await;
        }
    }

    // ------------------------------------------------------------ settings / profiles

    pub async fn session_defaults(&self) -> AppResult<(String, Option<String>, Option<String>)> {
        let target = misc::setting_get(&self.db.reader, "defaultTargetLanguage").await?.and_then(|v| v.0.as_str().map(|s| s.to_string())).unwrap_or_else(|| "ko".into());
        let source = misc::setting_get(&self.db.reader, "defaultSourceLanguage").await?.and_then(|v| v.0.as_str().map(|s| s.to_string()));
        let profile = misc::setting_get(&self.db.reader, "activeProfileId").await?.and_then(|v| v.0.as_str().map(|s| s.to_string()));
        Ok((target, source, profile))
    }

    pub async fn on_settings_changed(&self, key: &str) {
        if matches!(key, "glossary" | "promptTemplate") {
            // Context changes the cache key; queued work for the old key is dropped.
            let keys: Vec<String> = self.queue.lock().unwrap().waiting.keys().cloned().collect();
            for k in keys {
                self.drop_waiting(&k, "SETTINGS_CHANGED").await;
            }
        }
    }

    async fn provider_for(&self, provider_id: &str) -> AppResult<(translations::ProviderRow, Arc<dyn TranslationProvider>)> {
        let row = translations::provider_get(&self.db.reader, provider_id).await?.ok_or_else(|| AppError::not_found("Provider"))?;
        if !row.enabled {
            return Err(AppError::new(ErrorCode::ProviderUnavailable, "Provider가 활성화되지 않았습니다. 설정에서 활성화해 주세요."));
        }
        if self.auth_blocked.lock().unwrap().contains(provider_id) {
            return Err(AppError::new(ErrorCode::AuthFailed, "인증에 실패해 Provider를 중지했습니다. 설정에서 키를 확인해 주세요."));
        }
        let ver = row.created_at ^ row.cache_epoch ^ (row.capabilities.to_string().len() as i64) ^ (row.key_configured as i64);
        if let Some((v, p)) = self.providers.lock().unwrap().get(provider_id) {
            if *v == ver {
                return Ok((row, p.clone()));
            }
        }
        let cfg = prepare::provider_config(&row)?;
        let p = (self.factory)(&cfg, self.secrets.clone())?;
        self.providers.lock().unwrap().insert(provider_id.into(), (ver, p.clone()));
        self.provider_permits.lock().unwrap().insert(provider_id.into(), Arc::new(Semaphore::new(cfg.concurrency.clamp(1, 4) as usize)));
        Ok((row, p))
    }

    pub fn invalidate_provider(&self, provider_id: &str) {
        self.providers.lock().unwrap().remove(provider_id);
        self.auth_blocked.lock().unwrap().remove(provider_id);
    }

    /// Effective profile for a session: the active profile adapted to the document's
    /// source language (profiles are immutable; a different language is a new row).
    pub async fn session_profile(&self, s: &Session) -> AppResult<Option<ProfileContext>> {
        let (pid, src, tgt) = {
            let st = s.state.lock().unwrap();
            (st.profile_id.clone(), st.source_language.clone(), st.target_language.clone())
        };
        let Some(pid) = pid else { return Ok(None) };
        let Some(base) = translations::profile_get(&self.db.reader, &pid).await? else { return Ok(None) };
        let Some(src) = src else { return Ok(None) };
        prepare::profile_context(&self.db, &base, &src, &tgt).await.map(Some)
    }

    // ------------------------------------------------------------ display

    pub async fn display_for_blocks(&self, s: &Session, blocks: &[Block]) -> AppResult<Vec<BlockDisplay>> {
        let lang = s.state.lock().unwrap().target_language.clone();
        let ids: Vec<String> = blocks.iter().map(|b| b.id.clone()).collect();
        let heads: HashMap<String, _> = translations::heads_with_text(&self.db.reader, &ids, &lang).await?.into_iter().map(|h| (h.0.block_id.clone(), h)).collect();
        let jobs: HashMap<String, translations::JobRow> = translations::jobs_active_for_blocks(&self.db.reader, &ids).await?.into_iter().map(|j| (j.block_id.clone(), j)).collect();
        let profile = self.session_profile(s).await.ok().flatten();
        let ctx_index = prepare::SectionContext::new(blocks);
        let mut out = Vec::with_capacity(blocks.len());
        for b in blocks {
            let (ov, pv) = translations::preference_get(&self.db.reader, &b.id).await?;
            let user = ov.as_deref().and_then(TranslationPolicy::parse);
            let effective = TranslationPolicy::effective(b.translation_policy, b.quality.level, user);
            let current_key = match &profile {
                Some(p) if effective != TranslationPolicy::Preserve => prepare::prepare(&self.db, s, p, b, Some(&ctx_index)).await.ok().map(|pj| pj.cache_key.clone()),
                _ => None,
            };
            let (head, manual, generated) = match heads.get(&b.id) {
                Some((h, m, g)) => (h.clone(), m.clone(), g.clone()),
                None => (translations::HeadRow { block_id: b.id.clone(), target_language: lang.clone(), ..Default::default() }, None, None),
            };
            let cached = match &current_key {
                Some(k) => translations::cache_lookup(&self.db.reader, k).await?.map(|r| translations::DisplayRevision { revision_id: r.id, text: r.text, origin: r.origin, profile_id: r.profile_id, cache_key: r.cache_key }),
                None => None,
            };
            let mut has_candidate = false;
            let (status, shown) = if let Some(m) = manual {
                if let Some(c) = &cached {
                    let parent: Option<String> = sqlx::query_scalar("select parent_revision_id from translation_revisions where id = ?").bind(&m.revision_id).fetch_optional(&self.db.reader).await?.flatten();
                    has_candidate = parent.as_deref() != Some(c.revision_id.as_str());
                }
                ("manual", Some(m))
            } else if let Some(c) = cached {
                ("current", Some(c))
            } else if let Some(g) = generated {
                ("stale", Some(g))
            } else if effective != TranslationPolicy::Auto {
                ("excluded", None)
            } else {
                ("none", None)
            };
            let status = if effective == TranslationPolicy::Preserve && shown.is_none() { "excluded" } else { status };
            out.push(BlockDisplay {
                block_id: b.id.clone(),
                effective_policy: effective,
                user_override: user,
                preference_version: pv,
                head_version: head.version,
                status: status.into(),
                shown,
                has_candidate,
                job: jobs.get(&b.id).map(|j| JobBrief { job_id: j.id.clone(), state: j.state.clone(), error_code: j.error_code.clone() }),
            });
        }
        Ok(out)
    }

    pub async fn emit_head(&self, s: &Session, block_id: &str) {
        if let Ok(Some(b)) = extraction::get_block(&self.db.reader, block_id).await {
            if let Ok(mut d) = self.display_for_blocks(s, std::slice::from_ref(&b)).await {
                if let Some(d) = d.pop() {
                    s.emit("translation_committed", None, Some(block_id), serde_json::json!({"display": d}));
                }
            }
        }
    }

    // ------------------------------------------------------------ demand

    /// Visible report (P1) + prefetch (P2). Returns (acceptedSequence, queued, cacheHits).
    pub async fn set_viewport(self: &Arc<Self>, s: &Arc<Session>, generation: u64, sequence: u64, visible: Vec<String>, forward: bool) -> AppResult<serde_json::Value> {
        s.check_generation(generation)?;
        {
            let mut st = s.state.lock().unwrap();
            if sequence <= st.last_viewport_seq {
                return Ok(serde_json::json!({"acceptedSequence": st.last_viewport_seq, "queued": 0, "cacheHits": 0, "ignored": true}));
            }
            st.last_viewport_seq = sequence;
            st.visible = visible.clone();
            st.direction_forward = forward;
        }
        let (mode, paused, src) = {
            let st = s.state.lock().unwrap();
            (st.mode, st.auto_paused, st.source_language.clone())
        };
        let mut reason: Option<&str> = None;
        if mode == ReadingMode::Original {
            reason = Some("original_mode");
        } else if paused {
            reason = Some("paused");
        } else if src.is_none() {
            reason = Some("source_language_required");
        }
        let profile = if reason.is_none() { self.session_profile(s).await? } else { None };
        if reason.is_none() && profile.is_none() {
            reason = Some("no_profile");
        }
        let Some(profile) = profile else {
            self.prune_session_waiting(s, &HashSet::new()).await;
            return Ok(serde_json::json!({"acceptedSequence": sequence, "queued": 0, "cacheHits": 0, "blockedReason": reason}));
        };
        if let Err(e) = self.provider_for(&profile.profile.provider_id).await {
            return Ok(serde_json::json!({"acceptedSequence": sequence, "queued": 0, "cacheHits": 0, "blockedReason": e.code.as_str()}));
        }
        let ex = s.extraction()?;
        let mut wanted: HashSet<String> = HashSet::new();
        let (mut queued, mut hits) = (0u32, 0u32);
        let mut last_visible: Option<Block> = None;
        for id in visible.iter().take(100) {
            let Ok(b) = extraction::get_block_in_extraction(&self.db.reader, id, &ex).await else { continue };
            last_visible = Some(b.clone());
            match self.demand(s, generation, &profile, &b, Priority::P1, false).await {
                Ok(Demand::Queued(k)) => {
                    wanted.insert(k);
                    queued += 1;
                }
                Ok(Demand::CacheHit) => hits += 1,
                Ok(Demand::Running(k)) => {
                    wanted.insert(k);
                }
                _ => {}
            }
        }
        // P2 prefetch: next 3 leaves in the reading direction, ≤ 6,000 U16 total.
        if let Some(anchor) = if forward { last_visible } else { None } {
            let mut budget = PREFETCH_MAX_U16;
            for b in extraction::next_blocks(&self.db.reader, &anchor, PREFETCH_BLOCKS).await? {
                let len = crate::domain::utf16::len_u16(&b.source_text);
                if len > budget {
                    break;
                }
                budget -= len;
                if let Ok(Demand::Queued(k)) | Ok(Demand::Running(k)) = self.demand(s, generation, &profile, &b, Priority::P2, false).await {
                    wanted.insert(k);
                }
            }
        }
        self.prune_session_waiting(s, &wanted).await;
        self.notify.notify_one();
        Ok(serde_json::json!({"acceptedSequence": sequence, "queued": queued, "cacheHits": hits}))
    }

    /// Explicit request (P0). force=true bypasses the cache and creates a new revision.
    pub async fn request(self: &Arc<Self>, s: &Arc<Session>, block_id: &str, force: bool) -> AppResult<serde_json::Value> {
        let ex = s.extraction()?;
        let b = extraction::get_block_in_extraction(&self.db.reader, block_id, &ex).await?;
        if !extraction::is_leaf(&self.db.reader, &b.id).await? {
            return Err(AppError::invalid("하위 문단이 있는 블록은 번역 단위가 아닙니다."));
        }
        let src_missing = s.state.lock().unwrap().source_language.is_none();
        if src_missing {
            return Err(AppError::invalid("원문 언어를 먼저 설정해 주세요."));
        }
        let profile = self.session_profile(s).await?.ok_or_else(|| AppError::new(ErrorCode::ProviderUnavailable, "번역 Provider와 모델을 먼저 설정해 주세요."))?;
        self.provider_for(&profile.profile.provider_id).await?;
        let generation = s.generation();
        match self.demand(s, generation, &profile, &b, Priority::P0, force).await? {
            Demand::CacheHit => {
                let d = self.display_for_blocks(s, std::slice::from_ref(&b)).await?.pop();
                Ok(serde_json::json!({"kind": "cache_hit", "display": d}))
            }
            Demand::Queued(_) | Demand::Running(_) => {
                self.notify.notify_one();
                let job = translations::jobs_active_for_blocks(&self.db.reader, &[b.id.clone()]).await?.into_iter().next();
                Ok(serde_json::json!({"kind": "job", "jobId": job.map(|j| j.id)}))
            }
            Demand::Skipped(r) => Err(AppError::invalid(r)),
        }
    }

    async fn demand(self: &Arc<Self>, s: &Arc<Session>, generation: u64, profile: &ProfileContext, b: &Block, prio: Priority, force: bool) -> AppResult<Demand> {
        let lang = profile.profile.target_language.clone();
        if prio != Priority::P0 {
            let (ov, _) = translations::preference_get(&self.db.reader, &b.id).await?;
            let eff = TranslationPolicy::effective(b.translation_policy, b.quality.level, ov.as_deref().and_then(TranslationPolicy::parse));
            if eff != TranslationPolicy::Auto || b.source_text.trim().is_empty() {
                return Ok(Demand::Skipped("auto translation not allowed"));
            }
            if !extraction::is_leaf(&self.db.reader, &b.id).await? {
                return Ok(Demand::Skipped("container"));
            }
            // Manual pin: no automatic refresh.
            if translations::head_get(&self.db.reader, &b.id, &lang).await?.manual_revision_id.is_some() {
                return Ok(Demand::Skipped("manual pinned"));
            }
        } else if b.translation_policy == TranslationPolicy::Preserve && b.kind.default_policy() == TranslationPolicy::Preserve {
            return Ok(Demand::Skipped("이 블록은 원문 보존 대상입니다."));
        }
        let pj = Arc::new(prepare::prepare(&self.db, s, profile, b, None).await?);
        if !force {
            if let Some(rev) = translations::cache_lookup(&self.db.reader, &pj.cache_key).await? {
                let _ = translations::cache_touch(&self.db.writer, &pj.cache_key).await;
                if translations::select_generated_candidate(&self.db.writer, &b.id, &lang, &rev.id).await? || prio == Priority::P0 {
                    self.emit_head(s, &b.id).await;
                } else {
                    self.emit_head(s, &b.id).await;
                }
                return Ok(Demand::CacheHit);
            }
        }
        // Merge with an active job for the same key (no duplicate HTTP).
        {
            let mut q = self.queue.lock().unwrap();
            if let Some(e) = q.waiting.get_mut(&pj.cache_key) {
                if prio < e.priority {
                    e.priority = prio;
                }
                e.force |= force;
                if e.generation != generation || !Arc::ptr_eq(&e.session, s) {
                    e.session = s.clone();
                    e.generation = generation;
                }
                return Ok(Demand::Queued(pj.cache_key.clone()));
            }
            if let Some(r) = q.running.get_mut(&pj.cache_key) {
                if prio < r.2 {
                    r.2 = prio;
                }
                return Ok(Demand::Running(pj.cache_key.clone()));
            }
        }
        // Session queue cap: drop P2 first.
        if prio == Priority::P2 && self.session_waiting(s) >= MAX_QUEUED_PER_SESSION {
            return Ok(Demand::Skipped("queue full"));
        }
        if self.session_waiting(s) >= MAX_QUEUED_PER_SESSION {
            self.evict_lowest(s).await;
        }
        let snapshot = pj.snapshot_json();
        let (job, created) = translations::job_insert_or_merge(&self.db.writer, &b.id, &profile.profile.id, &pj.cache_key, &crate::domain::new_uuid(), &s.id, generation as i64, prio as i64, &snapshot).await?;
        if !created && job.state == "RETRY_WAIT" && job.next_attempt_at.is_none() {
            // Manual resume of a job waiting for user action.
            translations::job_transition(&self.db.writer, &job.id, "RETRY_WAIT", "QUEUED", None, None, false, None).await?;
        } else if !created && job.state != "QUEUED" {
            return Ok(Demand::Running(pj.cache_key.clone()));
        }
        let cancel = s.cancel_token().child_token();
        let mut q = self.queue.lock().unwrap();
        q.order += 1;
        let order = q.order;
        q.waiting.insert(pj.cache_key.clone(), Entry {
            job_id: job.id.clone(),
            cache_key: pj.cache_key.clone(),
            block_id: b.id.clone(),
            priority: prio,
            order,
            session: s.clone(),
            generation,
            prepared: pj.clone(),
            force,
            cancel,
            not_before: None,
        });
        drop(q);
        s.emit_for_generation(generation, "job_state", Some(&job.id), Some(&b.id), serde_json::json!({"state": "QUEUED", "priority": prio as i64}));
        Ok(Demand::Queued(pj.cache_key.clone()))
    }

    fn session_waiting(&self, s: &Session) -> usize {
        self.queue.lock().unwrap().waiting.values().filter(|e| e.session.id == s.id).count()
    }

    async fn evict_lowest(&self, s: &Session) {
        let victim = {
            let q = self.queue.lock().unwrap();
            q.waiting.values().filter(|e| e.session.id == s.id && e.priority != Priority::P0).max_by_key(|e| (e.priority, e.order)).map(|e| e.cache_key.clone())
        };
        if let Some(k) = victim {
            self.drop_waiting(&k, "QUEUE_LIMIT").await;
        }
    }

    async fn drop_waiting(&self, key: &str, reason: &str) {
        let e = self.queue.lock().unwrap().waiting.remove(key);
        if let Some(e) = e {
            let _ = translations::job_cancel(&self.db.writer, &e.job_id, reason).await;
            e.session.emit_for_generation(e.generation, "job_state", Some(&e.job_id), Some(&e.block_id), serde_json::json!({"state": "CANCELLED", "errorCode": reason}));
        }
    }

    /// Waiting P1/P2 jobs that left the visible/prefetch area are cancelled; running ones are not.
    async fn prune_session_waiting(&self, s: &Session, keep: &HashSet<String>) {
        let drop: Vec<String> = self.queue.lock().unwrap().waiting.values().filter(|e| e.session.id == s.id && e.priority != Priority::P0 && !keep.contains(&e.cache_key)).map(|e| e.cache_key.clone()).collect();
        for k in drop {
            self.drop_waiting(&k, "OUT_OF_VIEW").await;
        }
    }

    pub async fn cancel_job(&self, s: &Session, job_id: &str) -> AppResult<serde_json::Value> {
        let job = translations::job_get(&self.db.reader, job_id).await?.ok_or_else(|| AppError::not_found("작업"))?;
        if job.session_id != s.id {
            return Err(AppError::new(ErrorCode::PermissionDenied, "다른 세션의 작업입니다."));
        }
        let key = job.cache_key.clone();
        let waiting = self.queue.lock().unwrap().waiting.remove(&key);
        if let Some(e) = waiting {
            e.cancel.cancel();
        }
        if let Some((_, c, _, _)) = self.queue.lock().unwrap().running.get(&key) {
            c.cancel();
        }
        let prev = translations::job_cancel(&self.db.writer, job_id, "USER").await?;
        if prev.is_some() {
            s.emit("job_state", Some(job_id), Some(&job.block_id), serde_json::json!({"state": "CANCELLED", "errorCode": "USER"}));
        }
        let after = translations::job_get(&self.db.reader, job_id).await?.map(|j| j.state).unwrap_or_default();
        Ok(serde_json::json!({"cancelled": prev.is_some(), "state": after}))
    }

    pub async fn cancel_block_auto(&self, s: &Session, block_id: &str) {
        let keys: Vec<String> = self.queue.lock().unwrap().waiting.values().filter(|e| e.session.id == s.id && e.block_id == block_id && e.priority != Priority::P0).map(|e| e.cache_key.clone()).collect();
        for k in keys {
            self.drop_waiting(&k, "EXCLUDED").await;
        }
    }

    /// Session closed / generation change: cancel everything of that session.
    pub async fn cancel_session(&self, s: &Session) -> AppResult<u32> {
        let (waiting, running): (Vec<Entry>, Vec<(String, CancellationToken)>) = {
            let mut q = self.queue.lock().unwrap();
            let keys: Vec<String> = q.waiting.values().filter(|e| e.session.id == s.id).map(|e| e.cache_key.clone()).collect();
            let w = keys.into_iter().filter_map(|k| q.waiting.remove(&k)).collect();
            let r = q.running.values().filter(|r| r.3 == s.id).map(|r| (r.0.clone(), r.1.clone())).collect();
            (w, r)
        };
        let mut n = 0;
        for e in waiting {
            e.cancel.cancel();
            if translations::job_cancel(&self.db.writer, &e.job_id, "GENERATION").await?.is_some() {
                n += 1;
            }
        }
        for (id, c) in running {
            c.cancel();
            if translations::job_cancel(&self.db.writer, &id, "GENERATION").await?.is_some() {
                n += 1;
            }
        }
        Ok(n)
    }

    pub async fn set_mode(&self, s: &Arc<Session>, mode: ReadingMode) -> AppResult<()> {
        s.state.lock().unwrap().mode = mode;
        if mode == ReadingMode::Original {
            // Stop automatic work; explicit P0 requests continue. Cache is untouched.
            self.prune_session_waiting(s, &HashSet::new()).await;
        }
        Ok(())
    }

    pub async fn set_paused(&self, s: &Arc<Session>, paused: bool, cancel_running: bool) -> AppResult<u32> {
        s.state.lock().unwrap().auto_paused = paused;
        let mut n = 0;
        if paused {
            self.prune_session_waiting(s, &HashSet::new()).await;
            if cancel_running {
                let running: Vec<(String, CancellationToken)> = self.queue.lock().unwrap().running.values().filter(|r| r.3 == s.id && r.2 != Priority::P0).map(|r| (r.0.clone(), r.1.clone())).collect();
                for (id, c) in running {
                    c.cancel();
                    if translations::job_cancel(&self.db.writer, &id, "PAUSED").await?.is_some() {
                        n += 1;
                    }
                }
            }
        }
        self.notify.notify_one();
        Ok(n)
    }

    /// Provider/model/language switch => new generation; old work cancelled.
    pub async fn switch_profile(&self, s: &Arc<Session>, profile_id: Option<String>, target: Option<String>, source: Option<String>) -> AppResult<u64> {
        self.cancel_session(s).await?;
        {
            let mut st = s.state.lock().unwrap();
            if let Some(p) = profile_id {
                st.profile_id = Some(p);
            }
            if let Some(t) = target {
                st.target_language = t;
            }
            if let Some(src) = source {
                st.source_language = Some(src);
            }
            st.last_viewport_seq = 0;
        }
        Ok(s.bump_generation())
    }

    // ------------------------------------------------------------ dispatch

    async fn dispatch_loop(self: Arc<Self>) {
        loop {
            if self.shutdown.is_cancelled() {
                return;
            }
            let next = self.pick_next();
            match next {
                Some((entry, provider_permit, global_permit)) => {
                    let me = self.clone();
                    tokio::spawn(async move {
                        me.run_entry(entry).await;
                        drop(provider_permit);
                        drop(global_permit);
                        me.notify.notify_one();
                    });
                }
                None => {
                    tokio::select! {
                        _ = self.notify.notified() => {}
                        _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                        _ = self.shutdown.cancelled() => return,
                    }
                }
            }
        }
    }

    /// Highest priority, then FIFO. While any P0 waits, P2 is not fed.
    fn pick_next(&self) -> Option<(Entry, tokio::sync::OwnedSemaphorePermit, tokio::sync::OwnedSemaphorePermit)> {
        let mut q = self.queue.lock().unwrap();
        let now = Instant::now();
        let has_p0 = q.waiting.values().any(|e| e.priority == Priority::P0);
        let mut cands: Vec<(&String, &Entry)> = q
            .waiting
            .iter()
            .filter(|(_, e)| e.not_before.map(|t| t <= now).unwrap_or(true))
            .filter(|(_, e)| !(has_p0 && e.priority == Priority::P2))
            .filter(|(_, e)| !e.cancel.is_cancelled())
            .collect();
        cands.sort_by_key(|(_, e)| (e.priority, e.order));
        for (k, e) in cands {
            let pid = e.prepared.provider_id.clone();
            let sem = self.provider_permits.lock().unwrap().get(&pid).cloned();
            let Some(sem) = sem else { continue };
            let Ok(gp) = self.global.clone().try_acquire_owned() else { return None };
            let Ok(pp) = sem.try_acquire_owned() else {
                drop(gp);
                continue;
            };
            let key = k.clone();
            let entry = q.waiting.remove(&key).unwrap();
            q.running.insert(key, (entry.job_id.clone(), entry.cancel.clone(), entry.priority, entry.session.id.clone()));
            return Some((entry, pp, gp));
        }
        None
    }

    async fn run_entry(self: &Arc<Self>, e: Entry) {
        let result = self.execute(&e).await;
        self.queue.lock().unwrap().running.remove(&e.cache_key);
        if let Err(err) = result {
            tracing::debug!(target: "translation", "job {} ended: {:?}", e.job_id, err.code);
        }
    }

    async fn execute(self: &Arc<Self>, e: &Entry) -> AppResult<()> {
        let s = &e.session;
        if !translations::job_transition(&self.db.writer, &e.job_id, "QUEUED", "RUNNING", None, None, false, None).await? {
            return Ok(()); // cancelled meanwhile
        }
        s.emit_for_generation(e.generation, "job_state", Some(&e.job_id), Some(&e.block_id), serde_json::json!({"state": "RUNNING"}));
        let (_, provider) = match self.provider_for(&e.prepared.provider_id).await {
            Ok(v) => v,
            Err(err) => {
                translations::job_transition(&self.db.writer, &e.job_id, "RUNNING", "FAILED", Some(err.code.as_str()), None, false, None).await?;
                s.emit_for_generation(e.generation, "job_state", Some(&e.job_id), Some(&e.block_id), serde_json::json!({"state": "FAILED", "errorCode": err.code.as_str()}));
                return Err(err);
            }
        };
        let mut outputs: Vec<String> = Vec::with_capacity(e.prepared.parts.len());
        let mut state = "RUNNING";
        for (pi, part) in e.prepared.parts.iter().enumerate() {
            let mut attempt: u32 = 0;
            loop {
                if e.cancel.is_cancelled() {
                    let _ = translations::job_cancel(&self.db.writer, &e.job_id, "CANCELLED").await;
                    return Err(AppError::new(ErrorCode::Cancelled, "취소됨"));
                }
                attempt += 1;
                self.http_attempts.fetch_add(1, Ordering::SeqCst);
                sqlx::query("update translation_jobs set attempt_count = attempt_count + 1, last_part_index = ?, updated_at = ? where id = ?")
                    .bind(pi as i64)
                    .bind(now_ms())
                    .bind(&e.job_id)
                    .execute(&self.db.writer)
                    .await?;
                let (tx, mut rx) = tokio::sync::mpsc::channel::<TranslationDelta>(64);
                let mut req = part.request.clone();
                req.job_id = e.job_id.clone();
                req.request_id = crate::domain::new_uuid();
                let cancel = e.cancel.clone();
                let prov = provider.clone();
                let call = tokio::spawn(async move { prov.translate(req, tx, cancel).await });
                // Forward deltas in 50ms / 2KiB batches; only for the live generation.
                let mut buf = String::new();
                let mut last_flush = Instant::now();
                let mut first = true;
                while let Some(d) = rx.recv().await {
                    if first && state == "RUNNING" {
                        if translations::job_transition(&self.db.writer, &e.job_id, "RUNNING", "STREAMING", None, None, false, None).await? {
                            state = "STREAMING";
                            s.emit_for_generation(e.generation, "job_state", Some(&e.job_id), Some(&e.block_id), serde_json::json!({"state": "STREAMING"}));
                        }
                    }
                    if first {
                        s.emit_for_generation(e.generation, "translation_delta", Some(&e.job_id), Some(&e.block_id), serde_json::json!({"text": "", "partIndex": pi, "reset": pi == 0 && attempt == 1}));
                    }
                    first = false;
                    buf.push_str(&d.text);
                    if buf.len() >= 2048 || last_flush.elapsed() >= Duration::from_millis(50) {
                        s.emit_for_generation(e.generation, "translation_delta", Some(&e.job_id), Some(&e.block_id), serde_json::json!({"text": std::mem::take(&mut buf), "partIndex": pi}));
                        last_flush = Instant::now();
                    }
                }
                if !buf.is_empty() {
                    s.emit_for_generation(e.generation, "translation_delta", Some(&e.job_id), Some(&e.block_id), serde_json::json!({"text": buf, "partIndex": pi}));
                }
                let res = call.await.unwrap_or(Err(ProviderError::Cancelled));
                match res {
                    Ok(out) => match prepare::validate_part(&e.prepared, pi, &out) {
                        Ok(text) => {
                            outputs.push(text);
                            break;
                        }
                        Err(reason) => {
                            return self.fail(e, state, "INVALID_OUTPUT", false, Some(&reason)).await;
                        }
                    },
                    Err(ProviderError::Cancelled) => {
                        let _ = translations::job_cancel(&self.db.writer, &e.job_id, "CANCELLED").await;
                        return Err(AppError::new(ErrorCode::Cancelled, "취소됨"));
                    }
                    Err(err) => {
                        let code = err.to_app_error().code;
                        match err.retry_class() {
                            RetryClass::StopProvider => {
                                self.auth_blocked.lock().unwrap().insert(e.prepared.provider_id.clone());
                                return self.fail(e, state, code.as_str(), false, None).await;
                            }
                            RetryClass::SentUnknown => return self.fail(e, state, code.as_str(), true, None).await,
                            RetryClass::DoNotRetry => return self.fail(e, state, code.as_str(), err.maybe_sent(), None).await,
                            RetryClass::RetrySafe => match crate::providers::retry_decision(&err, attempt, (self.jitter)()) {
                                RetryDecision::RetryAfter(d) => {
                                    let at = now_ms() + d.as_millis() as i64;
                                    translations::job_transition(&self.db.writer, &e.job_id, state, "RETRY_WAIT", Some(code.as_str()), Some(at), false, Some(pi as i64)).await?;
                                    s.emit_for_generation(e.generation, "job_state", Some(&e.job_id), Some(&e.block_id), serde_json::json!({"state": "RETRY_WAIT", "errorCode": code.as_str(), "attempt": attempt}));
                                    tokio::select! {
                                        _ = tokio::time::sleep(d) => {}
                                        _ = e.cancel.cancelled() => {
                                            let _ = translations::job_cancel(&self.db.writer, &e.job_id, "CANCELLED").await;
                                            return Err(AppError::new(ErrorCode::Cancelled, "취소됨"));
                                        }
                                    }
                                    if !translations::job_transition(&self.db.writer, &e.job_id, "RETRY_WAIT", "QUEUED", None, None, false, None).await?
                                        || !translations::job_transition(&self.db.writer, &e.job_id, "QUEUED", "RUNNING", None, None, false, None).await?
                                    {
                                        return Err(AppError::new(ErrorCode::Cancelled, "취소됨"));
                                    }
                                    state = "RUNNING";
                                    continue;
                                }
                                RetryDecision::ManualResume => {
                                    translations::job_transition(&self.db.writer, &e.job_id, state, "RETRY_WAIT", Some(code.as_str()), None, false, Some(pi as i64)).await?;
                                    s.emit_for_generation(e.generation, "job_state", Some(&e.job_id), Some(&e.block_id), serde_json::json!({"state": "RETRY_WAIT", "errorCode": code.as_str(), "manual": true}));
                                    return Err(err.to_app_error());
                                }
                                RetryDecision::GiveUp => return self.fail(e, state, code.as_str(), false, None).await,
                            },
                        }
                    }
                }
            }
        }
        let text = prepare::join_parts(&outputs, &e.prepared.target_language);
        let is_current = {
            let st = s.state.lock().unwrap();
            e.generation == s.generation() && st.profile_id.is_some()
        } && self.session_profile(s).await.ok().flatten().map(|p| p.profile.id == e.prepared.profile_id).unwrap_or(false);
        let committed = translations::commit_generated(&self.db.writer, translations::CommitGenerated {
            job_id: &e.job_id,
            block_id: &e.block_id,
            target_language: &e.prepared.target_language,
            source_hash: &e.prepared.source_hash,
            profile_id: &e.prepared.profile_id,
            cache_key: &e.cache_key,
            descriptor: &e.prepared.descriptor,
            text: &text,
            metadata: &serde_json::json!({"generation": e.generation, "force": e.force, "parts": e.prepared.parts.len(), "languageWarning": prepare::language_warning(&text, &e.prepared.target_language)}),
            is_current_profile: is_current,
            force: e.force,
        })
        .await;
        match committed {
            Ok(_) => {
                // Completion event only after the DB commit, and only for the live generation.
                if e.generation == s.generation() {
                    self.emit_head(s, &e.block_id).await;
                }
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    async fn fail(&self, e: &Entry, from: &str, code: &str, sent_unknown: bool, reason: Option<&str>) -> AppResult<()> {
        if let Some(r) = reason {
            tracing::warn!(target: "translation", "invalid output for job {}: {}", e.job_id, crate::diagnostics::redact(r));
        }
        translations::job_transition(&self.db.writer, &e.job_id, from, "FAILED", Some(code), None, false, None).await?;
        e.session.emit_for_generation(e.generation, "job_state", Some(&e.job_id), Some(&e.block_id), serde_json::json!({"state": "FAILED", "errorCode": code, "sentUnknown": sent_unknown}));
        Err(AppError::new(ErrorCode::InvalidOutput, code.to_string()))
    }

    pub fn queue_stats(&self) -> serde_json::Value {
        let q = self.queue.lock().unwrap();
        serde_json::json!({"waiting": q.waiting.len(), "running": q.running.len()})
    }
}

enum Demand {
    Queued(String),
    Running(String),
    CacheHit,
    Skipped(&'static str),
}
