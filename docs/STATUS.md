# Thoth Reader 구현 현황 (2026-11-14)

기준 명세: 상세 개발 명세서 v1.0. 이 문서는 실제로 실행한 검증만 기록한다.

## 실행한 검증

| 항목 | 명령 | 결과 |
| --- | --- | --- |
| Rust 단위·통합 테스트 | `cd src-tauri && cargo test --lib` | 222 통과, 0 실패, 3 ignored (실제 Keychain, fixture 생성기, 로컬 코퍼스) |
| 프론트엔드 테스트 | `npx vitest run` | 116 통과, 1 skipped (로컬 코퍼스) |
| 타입 검사 | `npx tsc -b --noEmit` | 오류 0 |
| 경합 반복 (AC05) | `ac05_cancel_completion_race_1000`, `ac05_manual_vs_auto_race_1000` | 각 1,000회, 불변식 위반 0 |
| 악성 EPUB corpus (T07/T08) | `epub_security::tests` | 32개 사례 차단 또는 무해화 |
| 실제 WKWebView 자기 테스트 | `scripts/selftest.sh` (debug 전용) | 아래 표 |
| 로컬 코퍼스 (사용자 제공 49권, 통계만) | `THOTH_CORPUS=… ` 하네스 | 아래 표 |

### 실제 앱(WKWebView) 자기 테스트 — debug 빌드, LM Studio `qwen3.8-27b`

| 측정 | EPUB 2권 | PDF 1권 |
| --- | --- | --- |
| 가져오기 | 29–195 ms | 48–138 ms |
| 열기 → 첫 본문 | 296–1,201 ms | 597 ms |
| 리플로우 행 표시 | — | 52 ms |
| 모드 전환 후 같은 블록 유지 (FR02) | 병렬↔교차↔원문 유지 | 원본↔리플로우↔교차 유지 |
| 첫 번역 표시 | 0.4–1.0 s | — |
| 수동 수정 저장 (DB 커밋 후 표시) | 53–104 ms | — |
| 재열기 시 캐시 번역 표시 | 203–405 ms, 새 모델 요청 0건 | — |
| 재열기 위치·수동 고정본 복원 | 일치 | — |
| 웹뷰 오류 / 느린 쿼리 | 0 / 0 | 0 / 0 |

debug 빌드 수치이며 명세 §10.3의 M1·M4 release 벤치마크가 아니다.

### 실제 앱 시나리오 (debug 전용 자기 테스트, `THOTH_SELFTEST_SCENARIO`)

| 시나리오 | 대상 | 결과 |
| --- | --- | --- |
| `modes20` — 문서 중간에서 모드 20회 전환 (T06) | EPUB 3권, PDF 2권 | 앵커 변화 0건, 기준선 오차 최대 1px (기준 48px), 오류 0건 |
| `security` — 악성 fixture를 원문 모드로 열어 DOM 검사 (T07/T08) | 32개 | 17개 열기 거부(사유 표시), 15개 무해화: 스크립트·이벤트 핸들러·외부 로드·앱 외부 요청 0건, iframe sandbox에 `allow-scripts` 없음 |
| `outage` — 캐시 생성 후 Provider 종료 (AC14) | EPUB 1권 | 원문 25행 표시, 캐시 번역 8개 표시, 새 요청은 3회 시도 후 `PROVIDER_UNAVAILABLE` 실패 표시 |

PDF 확대율·devicePixelRatio 변경(T04)과 테마·글꼴 변경 후 앵커(T06 일부)는 이 시나리오에 포함되지 않았다.

### 로컬 코퍼스 통계 (원문 미포함)

| | PDF 35권 (문서당 최대 40쪽 샘플) | EPUB 14권 (전체) |
| --- | --- | --- |
| 열림 | 35 | 14 (1권은 깨진 `container.xml`을 복구, `rootfile_mismatch` 제한 표시) |
| 페이지/섹션 실패 | 1권 (파일 자체의 xref 손상, 해당 페이지만 실패 처리) | 0 |
| 자동 번역 가능 본문 비율 중앙값 | 0.91 (0.95 이상 14권) | 1.00 |
| 처리 시간 | 중앙값 9.9 ms/쪽, 최대 93 ms/쪽 | 문서당 최대 0.78 s |
| 스캔+OCR 문서 | 1권 → 전 블록 `manual_only` | — |

정답 라벨이 없으므로 §10.4의 precision/recall/읽기 순서 정확도는 측정하지 않았다(합성 fixture에서만 1.0).

## 수용 기준 상태

| ID | 상태 | 근거 / 남은 일 |
| --- | --- | --- |
| AC01 | 부분 충족 | 가져오기·중복 판정·재열기 테스트 통과, 실기 확인. T01/T05는 합성 fixture 기준 |
| AC02 | 부분 충족 | 실기 `modes20`: EPUB 3권·PDF 2권에서 20회 전환 앵커 유지, 오차 ≤1px. 영상, 확대율·테마·글꼴 변경(T04/T06 일부) 미수행 |
| AC03 | 충족(테스트) | `prefetch_next_three`, `original_mode_stops_auto`, 실기에서 P1 8건·P2 3건 관측 |
| AC04 | 부분 충족 | OpenAI 호환(LM Studio) 실연 완료. **Ollama 실연 미수행**(미설치), 어댑터는 mock 서버 테스트만 |
| AC05 | 충족(테스트) | T09–T12, 경합 1,000회 ×2 |
| AC06 | 충족(테스트) | T13, T14(스트림 중 종료 후 재시작: 미리보기 제외, 수동 고정본 복원), 실기 재열기 확인 |
| AC07 | 충족(테스트) | golden key, T11/T15 |
| AC08 | 부분 충족 | T07/T08/T18 단위 테스트 + 실기 `security` 32개 위반 0건. OS 수준 네트워크 감시는 미수행 |
| AC09 | 부분 충족 | 마이그레이션 실패 주입(데이터 보존·롤백·복구 백업·읽기 전용), checksum 불일치 거부, 삭제 journal 복구 테스트. 디스크 부족(DB full) 주입 미수행 |
| AC10 | 부분 충족 | 검색·북마크·테마·위치 복원 구현, 검색은 SQL 테스트. native smoke는 위치 복원만 |
| AC11 | **미충족** | M1/M4 release 벤치마크와 라벨된 추출 품질 측정 필요 |
| AC12 | **미충족** | 키보드·ARIA 구현만. VoiceOver 실기 검증 필요 |
| AC13 | **미충족** | Developer ID 서명·notarization 자격 필요. 서명 없는 내부 빌드까지만 |
| AC14 | 부분 충족 | 실기 `outage`로 Provider 장애 시 원문·캐시 읽기 확인. 시스템 네트워크 차단, Ollama 중단은 미수행 |

## 알려진 제한

- PDF: 구조 트리 미사용, RTL·세로쓰기는 표시만, 그림 블록 미생성, 하이픈 처리는 휴리스틱. 첫 추출 배치(1쪽)는 running header 판정이 적용되지 않는다.
- EPUB 원문 모드는 epub.js `blobUrl` 방식(샌드박스 iframe, 스크립트 불가). `freezePrototype`는 xmldom 충돌로 비활성(ADR 0004).
- 추론(thinking) 모델은 연결 검사에서 `reasoning_effort: "none"`이 검증된 경우에만 비활성화한다. 검증되지 않으면 그대로 느리게 동작한다.
- 번역 품질 2인 검수(§10.4), @wdio/tauri-service 기반 E2E는 수행하지 않았다. 대신 debug 전용 자기 테스트 하네스를 사용한다.

## 로컬 검증 도구

```bash
THOTH_CORPUS=/path/to/books npx vitest run src/document/pdf/corpus.test.ts
```

```bash
cd src-tauri && THOTH_CORPUS=/path/to/books cargo test --release --lib epub_corpus -- --ignored --nocapture
```

```bash
THOTH_SELFTEST_PROVIDER=http://localhost:1235/v1 THOTH_SELFTEST_MODEL=<model> scripts/selftest.sh /tmp/thoth.log /tmp/thoth-data book.epub book.pdf
```
