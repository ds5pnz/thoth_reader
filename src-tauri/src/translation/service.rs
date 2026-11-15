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
    /// Queued by a batch run: not dropped when it leaves the viewport.
    pinned: bool,
}

#[derive(Default)]
struct Queue {
    waiting: HashMap<String, Entry>,           // cache_key -> entry
    running: HashMap<String, (String, CancellationToken, Priority, String)>, // cache_key -> (job_id, cancel, priority, session_id)
    order: u64,
    /// cache_key -> when its last job failed; automatic demand backs off for a while.
    failed: HashMap<String, Instant>,
}

/// After a failure, viewport-driven (automatic) demand leaves the block alone this long;
/// an explicit request retries at once.
const AUTO_RETRY_COOLDOWN: Duration = Duration::from_secs(300);

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
    /// One range-translation run per session (session id -> run).
    batches: Mutex<HashMap<String, Arc<Batch>>>,
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
            batches: Mutex::new(HashMap::new()),
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
        if matches!(key, "glossary" | "promptTemplate" | "translationStyle") {
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
            return Err(AppError::new(ErrorCode::AuthFailed, "인증에 실패해 Provider를 중지했습니다. 설정에서 키 또는 로그인 상태를 확인해 주세요."));
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
                Some(k) => translations::cache_lookup(&self.db.reader, k).await?.map(|r| translations::DisplayRevision { warning: translations::warning_of(&r.metadata), marks: crate::translation::inline_marks::marks_of(&r.metadata), revision_id: r.id, text: r.text, origin: r.origin, profile_id: r.profile_id, cache_key: r.cache_key }),
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
            // Text without any letters (ornaments, numbers, punctuation) is never auto-sent.
            if eff != TranslationPolicy::Auto || !b.source_text.chars().any(|c| c.is_alphabetic()) {
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
            if prio == Priority::P0 || force {
                q.failed.remove(&pj.cache_key);
            } else if q.failed.get(&pj.cache_key).map(|t| t.elapsed() < AUTO_RETRY_COOLDOWN).unwrap_or(false) {
                // Scrolling must not re-send a block that just failed (billing, Retry-After).
                return Ok(Demand::Skipped("recently failed"));
            }
            if let Some(e) = q.waiting.get_mut(&pj.cache_key) {
                if prio < e.priority {
                    e.priority = prio;
                }
                e.force |= force;
                if e.generation != generation || !Arc::ptr_eq(&e.session, s) {
                    e.session = s.clone();
                    e.generation = generation;
                    // The entry now lives and dies with this session, not the one that created it.
                    e.cancel = s.cancel_token().child_token();
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
            if prio != Priority::P0 {
                return Ok(Demand::Skipped("waiting for manual retry"));
            }
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
            pinned: false,
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
            q.waiting.values().filter(|e| e.session.id == s.id && e.priority != Priority::P0 && !e.pinned).max_by_key(|e| (e.priority, e.order)).map(|e| e.cache_key.clone())
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
        let drop: Vec<String> = self.queue.lock().unwrap().waiting.values().filter(|e| e.session.id == s.id && e.priority != Priority::P0 && !e.pinned && !keep.contains(&e.cache_key)).map(|e| e.cache_key.clone()).collect();
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
            self.reap_cancelled().await;
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

    /// Waiting entries whose session went away would otherwise sit in the queue forever and
    /// swallow every later demand for the same cache key.
    async fn reap_cancelled(&self) {
        let dead: Vec<String> = {
            let mut q = self.queue.lock().unwrap();
            let keys: Vec<String> = q.waiting.iter().filter(|(_, e)| e.cancel.is_cancelled()).map(|(k, _)| k.clone()).collect();
            keys.iter().filter_map(|k| q.waiting.remove(k)).map(|e| e.job_id).collect()
        };
        for id in dead {
            let _ = translations::job_cancel(&self.db.writer, &id, "CANCELLED").await;
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
        // Leaves `running` even if the job panics, or the key could never be translated again.
        struct Running<'a>(&'a Mutex<Queue>, &'a str);
        impl Drop for Running<'_> {
            fn drop(&mut self) {
                let mut q = self.0.lock().unwrap_or_else(|p| p.into_inner());
                q.running.remove(self.1);
            }
        }
        let _running = Running(&self.queue, &e.cache_key);
        let result = self.execute(&e).await;
        match &result {
            Ok(()) => {
                self.queue.lock().unwrap().failed.remove(&e.cache_key);
            }
            Err(err) if err.code == ErrorCode::Cancelled => {}
            Err(err) => {
                tracing::debug!(target: "translation", "job {} ended: {:?}", e.job_id, err.code);
                self.queue.lock().unwrap().failed.insert(e.cache_key.clone(), Instant::now());
                // An error exit (e.g. a DB error at commit) must not leave the row active:
                // every later demand would merge into a job that no longer exists.
                let settled = sqlx::query("update translation_jobs set state = 'FAILED', error_code = coalesce(error_code, 'INTERNAL'), next_attempt_at = null, updated_at = ? where id = ? and state in ('QUEUED','RUNNING','STREAMING')")
                    .bind(now_ms())
                    .bind(&e.job_id)
                    .execute(&self.db.writer)
                    .await
                    .map(|r| r.rows_affected())
                    .unwrap_or(0);
                if settled > 0 {
                    e.session.emit_for_generation(e.generation, "job_state", Some(&e.job_id), Some(&e.block_id), serde_json::json!({"state": "FAILED", "errorCode": "INTERNAL"}));
                }
            }
        }
    }

    /// Cancelled while executing: settle the row and tell the view (pause keeps the generation).
    async fn cancelled(&self, e: &Entry) -> AppError {
        let _ = translations::job_cancel(&self.db.writer, &e.job_id, "CANCELLED").await;
        e.session.emit_for_generation(e.generation, "job_state", Some(&e.job_id), Some(&e.block_id), serde_json::json!({"state": "CANCELLED"}));
        AppError::new(ErrorCode::Cancelled, "취소됨")
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
                    return Err(self.cancelled(e).await);
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
                        return Err(self.cancelled(e).await);
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
                                            return Err(self.cancelled(e).await);
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
        let joined = prepare::join_parts(&outputs, &e.prepared.target_language);
        let (text, marks) = prepare::finish_text(&e.prepared, &joined);
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
            metadata: &serde_json::json!({"generation": e.generation, "force": e.force, "parts": e.prepared.parts.len(), "languageWarning": prepare::language_warning(&text, &e.prepared.target_language), "scriptWarning": prepare::script_warning(&e.prepared, &text), "marks": marks}),
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

// ---------------------------------------------------------------- selection translation

impl TranslationService {
    /// Translate a piece of text the reader selected. Nothing is stored: no revision, no
    /// cache entry, no job row — just the session's current profile applied to the text.
    pub async fn translate_selection(self: &Arc<Self>, s: &Arc<Session>, text: &str) -> AppResult<String> {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() || text.chars().count() > 4000 {
            return Err(AppError::limit("선택한 글이 비어 있거나 너무 깁니다(최대 4,000자)."));
        }
        if s.state.lock().unwrap().source_language.is_none() {
            return Err(AppError::invalid("원문 언어를 먼저 설정해 주세요."));
        }
        let p = self.session_profile(s).await?.ok_or_else(|| AppError::new(ErrorCode::ProviderUnavailable, "번역 Provider와 모델을 먼저 설정해 주세요."))?;
        let (_, provider) = self.provider_for(&p.profile.provider_id).await?;
        let protected = super::placeholder::protect(&text, &[])?;
        let messages = super::prompt::build_messages(&super::prompt::PromptInput {
            block_id: "selection".into(),
            segments: vec![super::prompt::PromptSegment { id: prepare::SEGMENT_ID.into(), text: protected.text.clone() }],
            source_language: p.profile.source_language.clone(),
            target_language: p.profile.target_language.clone(),
            context: super::prompt::PromptContext::empty(),
            glossary: p.glossary.iter().filter(|g| !g.source.is_empty() && text.to_lowercase().contains(&g.source.to_lowercase())).cloned().collect(),
            response_format: p.response_format,
            placeholder_example: if protected.has_placeholders() { Some(protected.example_token()) } else { None },
            style: p.style,
            inline_tags: false,
            page_break: false,
            color_tags: false,
            mode: p.prompt_mode,
        })?;
        let request = crate::providers::TranslationRequest {
            request_id: crate::domain::new_uuid(),
            job_id: String::new(),
            block_id: "selection".into(),
            part_index: 0,
            part_count: 1,
            model_id: p.profile.model_id.clone(),
            source_language: p.profile.source_language.clone(),
            target_language: p.profile.target_language.clone(),
            messages,
            options: p.options.clone(),
            response_format: p.response_format,
            stream: p.caps.streaming,
            expected_segment_ids: vec![prepare::SEGMENT_ID.into()],
        };
        // Wait for a slot like any other request, so a local model is not hit twice at once.
        let _global = self.global.clone().acquire_owned().await.map_err(|_| AppError::internal("번역기를 사용할 수 없습니다."))?;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<TranslationDelta>(64);
        let cancel = s.cancel_token().child_token();
        let call = tokio::spawn(async move { provider.translate(request, tx, cancel).await });
        while rx.recv().await.is_some() {}
        let out = call.await.unwrap_or(Err(ProviderError::Cancelled)).map_err(|e| e.to_app_error())?;
        let stripped = prepare::strip_leading_think(&out.text).map(|t| t.to_string());
        let out = match stripped {
            Some(t) => crate::providers::TranslationOutput { text: t, ..out },
            None => out,
        };
        let v = super::validation::validate_output(&out, p.response_format, &[super::validation::ExpectedSegment { id: prepare::SEGMENT_ID, protected: &protected }], &p.profile.target_language)
            .map_err(|e| AppError::new(ErrorCode::InvalidOutput, e.reason().to_string()))?;
        v.segments.into_iter().next().map(|x| x.text.trim().to_string()).filter(|t| !t.is_empty()).ok_or_else(|| AppError::new(ErrorCode::InvalidOutput, "빈 응답"))
    }
}

// ---------------------------------------------------------------- range translation

/// Progress of a "translate this range" run. Counts are paragraphs (translation units);
/// the time estimate is based on characters, which track the work far better.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct BatchStatus {
    /// running | done | cancelled | stopped
    pub state: String,
    pub sections_total: u32,
    pub sections_done: u32,
    /// Sections whose text is not extracted yet (their paragraphs are not in `total`).
    pub sections_waiting: u32,
    pub total: u32,
    /// Translated now or already translated before.
    pub done: u32,
    pub failed: u32,
    /// Not translatable (code, figures, excluded, manually edited…).
    pub skipped: u32,
    pub eta_seconds: Option<u32>,
    #[ts(type = "number")]
    pub started_at: i64,
    pub message: Option<String>,
}

struct Batch {
    cancel: CancellationToken,
    status: Mutex<BatchStatus>,
}

const BATCH_WINDOW: usize = 4;

impl TranslationService {
    pub fn batch_status(&self, s: &Session) -> Option<BatchStatus> {
        self.batches.lock().unwrap().get(&s.id).map(|b| b.status.lock().unwrap().clone())
    }

    pub fn batch_cancel(&self, s: &Session) -> bool {
        match self.batches.lock().unwrap().get(&s.id) {
            Some(b) => {
                b.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// Translate every translatable paragraph of `section_ids` (in reading order) with the
    /// session's current profile. Returns at once; progress arrives as `batch_progress` events.
    pub async fn batch_start(self: &Arc<Self>, s: &Arc<Session>, section_ids: Vec<String>) -> AppResult<BatchStatus> {
        if s.state.lock().unwrap().source_language.is_none() {
            return Err(AppError::invalid("원문 언어를 먼저 설정해 주세요."));
        }
        let profile = self.session_profile(s).await?.ok_or_else(|| AppError::new(ErrorCode::ProviderUnavailable, "번역 Provider와 모델을 먼저 설정해 주세요."))?;
        self.provider_for(&profile.profile.provider_id).await?;
        let ex = s.extraction()?;
        let wanted: HashSet<String> = section_ids.into_iter().collect();
        let rows: Vec<extraction::SectionRow> = extraction::list_sections(&self.db.reader, &ex).await?.into_iter().filter(|x| wanted.contains(&x.id)).collect();
        if rows.is_empty() {
            return Err(AppError::invalid("번역할 범위가 비어 있습니다."));
        }
        // Paragraph counts of everything already extracted are known up front, so the
        // percentage does not jump backwards as the run reaches later sections.
        let known: HashMap<String, u32> = rows.iter().filter(|x| x.state == "ready").map(|x| (x.id.clone(), x.block_count.max(0) as u32)).collect();
        let known_total: u32 = known.values().sum();
        let sections: Vec<String> = rows.into_iter().map(|x| x.id).collect();
        if let Some(old) = self.batches.lock().unwrap().get(&s.id) {
            old.cancel.cancel();
        }
        let status = BatchStatus {
            state: "running".into(),
            sections_total: sections.len() as u32,
            sections_done: 0,
            sections_waiting: sections.len() as u32 - known.len() as u32,
            total: known_total,
            done: 0,
            failed: 0,
            skipped: 0,
            eta_seconds: None,
            started_at: now_ms(),
            message: None,
        };
        let batch = Arc::new(Batch { cancel: s.cancel_token().child_token(), status: Mutex::new(status.clone()) });
        self.batches.lock().unwrap().insert(s.id.clone(), batch.clone());
        let (me, s2) = (self.clone(), s.clone());
        let generation = s.generation();
        tokio::spawn(async move {
            let end = me.batch_run(&s2, generation, &profile, &ex, sections, known, &batch).await;
            let snapshot = {
                let mut st = batch.status.lock().unwrap();
                st.eta_seconds = None;
                match end {
                    Ok(()) if batch.cancel.is_cancelled() => st.state = "cancelled".into(),
                    Ok(()) => st.state = "done".into(),
                    Err(e) => {
                        st.state = "stopped".into();
                        st.message = Some(e.message.clone());
                    }
                }
                st.clone()
            };
            s2.emit("batch_progress", None, None, serde_json::to_value(&snapshot).unwrap_or_default());
        });
        Ok(status)
    }

    async fn batch_run(self: &Arc<Self>, s: &Arc<Session>, generation: u64, profile: &ProfileContext, ex: &str, sections: Vec<String>, known: HashMap<String, u32>, batch: &Arc<Batch>) -> AppResult<()> {
        // In flight: block id -> (block, characters, re-queue attempts)
        let mut flying: HashMap<String, (Block, u64, u32)> = HashMap::new();
        let mut translated_chars: u64 = 0; // work actually done by this run
        let mut remaining_chars: u64 = 0; // known work still ahead (queued + in flight)
        let mut counted: HashSet<String> = HashSet::new(); // sections already added to the totals
        let mut work_started: Option<Instant> = None;
        let mut last_emit = Instant::now() - Duration::from_secs(5);
        let mut pending: std::collections::VecDeque<String> = sections.into_iter().collect();
        let mut stalled_since: Option<Instant> = None;
        let mut queue: std::collections::VecDeque<Block> = std::collections::VecDeque::new();

        loop {
            if batch.cancel.is_cancelled() {
                return Ok(());
            }
            if s.generation() != generation {
                return Err(AppError::invalid("번역 설정이 바뀌어 일괄 번역을 멈췄습니다."));
            }
            // 1. Bring in the next sections whose text is ready.
            while queue.len() < BATCH_WINDOW * 4 {
                let Some(sec) = pending.front().cloned() else { break };
                let state: Option<String> = sqlx::query_scalar("select state from sections where id = ?").bind(&sec).fetch_optional(&self.db.reader).await?;
                match state.as_deref() {
                    Some("ready") => {
                        pending.pop_front();
                        stalled_since = None;
                        let before = counted.len();
                        let mut after: Option<u32> = None;
                        loop {
                            let page = extraction::blocks_of_section(&self.db.reader, ex, &sec, after, 500).await?;
                            let Some(last) = page.last() else { break };
                            after = Some(last.ordinal);
                            let n = page.len();
                            for b in page {
                                if counted.insert(b.id.clone()) {
                                    remaining_chars += b.source_text.chars().count() as u64;
                                    queue.push_back(b);
                                }
                            }
                            if n < 500 {
                                break;
                            }
                        }
                        let added = (counted.len() - before) as u32;
                        let mut st = batch.status.lock().unwrap();
                        st.sections_done += 1;
                        match known.get(&sec) {
                            // counted up front: only correct a difference
                            Some(n) => st.total = (st.total + added).saturating_sub(*n),
                            None => {
                                st.total += added;
                                st.sections_waiting = st.sections_waiting.saturating_sub(1);
                            }
                        }
                    }
                    Some("failed") | None => {
                        pending.pop_front();
                        let mut st = batch.status.lock().unwrap();
                        st.sections_done += 1;
                        if !known.contains_key(&sec) {
                            st.sections_waiting = st.sections_waiting.saturating_sub(1);
                        }
                    }
                    // Text extraction (PDF pages, EPUB chapters) is still running: wait for it,
                    // but not forever.
                    _ => {
                        let since = *stalled_since.get_or_insert_with(Instant::now);
                        if queue.is_empty() && flying.is_empty() && since.elapsed() > Duration::from_secs(600) {
                            return Err(AppError::invalid("본문 추출이 끝나지 않아 일괄 번역을 멈췄습니다."));
                        }
                        break;
                    }
                }
            }
            // 2. Fill the window.
            while flying.len() < BATCH_WINDOW {
                let Some(b) = queue.pop_front() else { break };
                let chars = b.source_text.chars().count() as u64;
                let finished = |st: &mut BatchStatus, kind: u8| match kind {
                    0 => st.done += 1,
                    1 => st.skipped += 1,
                    _ => st.failed += 1,
                };
                let already = self.display_for_blocks(s, std::slice::from_ref(&b)).await?.pop().map(|d| matches!(d.status.as_str(), "current" | "manual")).unwrap_or(false);
                if already {
                    remaining_chars = remaining_chars.saturating_sub(chars);
                    finished(&mut batch.status.lock().unwrap(), 0);
                    continue;
                }
                match self.demand(s, generation, profile, &b, Priority::P1, false).await {
                    Ok(Demand::CacheHit) => {
                        remaining_chars = remaining_chars.saturating_sub(chars);
                        finished(&mut batch.status.lock().unwrap(), 0);
                    }
                    Ok(Demand::Queued(k)) | Ok(Demand::Running(k)) => {
                        if let Some(e) = self.queue.lock().unwrap().waiting.get_mut(&k) {
                            e.pinned = true;
                        }
                        work_started.get_or_insert_with(Instant::now);
                        flying.insert(b.id.clone(), (b, chars, 0));
                        self.notify.notify_one();
                    }
                    Ok(Demand::Skipped(_)) => {
                        remaining_chars = remaining_chars.saturating_sub(chars);
                        finished(&mut batch.status.lock().unwrap(), 1);
                    }
                    Err(_) => {
                        remaining_chars = remaining_chars.saturating_sub(chars);
                        finished(&mut batch.status.lock().unwrap(), 2);
                    }
                }
            }
            if flying.is_empty() && queue.is_empty() && pending.is_empty() {
                return Ok(());
            }
            // 3. See what finished.
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(400)) => {}
                _ = batch.cancel.cancelled() => return Ok(()),
            }
            let blocks: Vec<Block> = flying.values().map(|v| v.0.clone()).collect();
            if !blocks.is_empty() {
                for d in self.display_for_blocks(s, &blocks).await? {
                    let Some((b, chars, tries)) = flying.get(&d.block_id).cloned() else { continue };
                    if matches!(d.status.as_str(), "current" | "manual") {
                        flying.remove(&d.block_id);
                        translated_chars += chars;
                        remaining_chars = remaining_chars.saturating_sub(chars);
                        batch.status.lock().unwrap().done += 1;
                    } else if d.job.is_none() {
                        // No active job any more: failed, or dropped by a queue limit.
                        let retry = if tries < 2 { self.demand(s, generation, profile, &b, Priority::P1, false).await.ok() } else { None };
                        match retry {
                            Some(Demand::Queued(k)) | Some(Demand::Running(k)) => {
                                if let Some(e) = self.queue.lock().unwrap().waiting.get_mut(&k) {
                                    e.pinned = true;
                                }
                                flying.insert(d.block_id.clone(), (b, chars, tries + 1));
                                self.notify.notify_one();
                            }
                            Some(Demand::CacheHit) => {
                                flying.remove(&d.block_id);
                                translated_chars += chars;
                                remaining_chars = remaining_chars.saturating_sub(chars);
                                batch.status.lock().unwrap().done += 1;
                            }
                            _ => {
                                flying.remove(&d.block_id);
                                remaining_chars = remaining_chars.saturating_sub(chars);
                                batch.status.lock().unwrap().failed += 1;
                            }
                        }
                    }
                }
            }
            // 4. Estimate and report (at most twice a second).
            if last_emit.elapsed() >= Duration::from_millis(500) {
                last_emit = Instant::now();
                let snapshot = {
                    let mut st = batch.status.lock().unwrap();
                    st.eta_seconds = match work_started {
                        Some(t0) if translated_chars >= 400 && t0.elapsed() >= Duration::from_secs(5) => {
                            let rate = translated_chars as f64 / t0.elapsed().as_secs_f64(); // chars / s
                            // Pages not extracted yet are assumed to look like the ones seen so far.
                            let seen = (st.sections_total - st.sections_waiting).max(1) as f64;
                            let unseen = (translated_chars + remaining_chars) as f64 / seen * st.sections_waiting as f64;
                            Some(((remaining_chars as f64 + unseen) / rate.max(0.1)).round().min(359_999.0) as u32)
                        }
                        _ => None,
                    };
                    st.clone()
                };
                s.emit_for_generation(generation, "batch_progress", None, None, serde_json::to_value(&snapshot).unwrap_or_default());
            }
        }
    }
}

enum Demand {
    Queued(String),
    Running(String),
    CacheHit,
    Skipped(&'static str),
}
