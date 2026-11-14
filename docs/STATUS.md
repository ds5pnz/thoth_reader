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

### PDF 번역 페이지 레이아웃 감사 (`scripts/audit.sh`, ADR 0005)

PDF 10권에서 고정 시드로 고른 무작위 10쪽씩 100쪽. 번역문은 로컬 mock 서버가 원문 길이의 약 0.64배인
한글 채움 문장으로 돌려주며(실제 번역 아님), 앱이 쪽마다 수치만 기록한다. 창 폭 1,280px 기준.

| 항목 | 수정 전 | 수정 후 |
| --- | --- | --- |
| 문제 있는 쪽 | 20 / 100 | 0 / 100 |
| 번역 블록끼리 겹침 | 48쌍 | 0쌍 (문단 안의 작은 블록 1쌍은 위에 그려짐) |
| 잘린 번역 | 5 | 0 |
| 원문 크기의 60% 미만으로 축소 | 0~3 | 0 |
| 90% 미만으로 축소 | 12~67 | 13 / 1,051 |
| 페이지 밖으로 넘침 | 0 | 0 |

고친 원인: 위아래 문단 줄 영역의 수 px 겹침, 글자 크기 반올림으로 인한 넘침, 일부 PDF의 과대 보고된
글꼴 높이, 화면 갱신마다 페이지 픽셀을 다시 읽던 성능 문제. 자리가 너무 작아 읽을 수 없게 되는 블록은
원문을 유지하고 점선으로 표시한다. 실제 번역은 채움 문장과 길이·줄바꿈이 다를 수 있고, 눈으로 본 검증은 아니다.

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

## 서재 화면 (표지 · 메타데이터 · 정렬)

- EPUB: 가져올 때 안전 패키지를 만들면서 `dc:creator` / `dc:publisher` / `dc:date`와 표지(EPUB2 `meta name="cover"`, EPUB3 `cover-image`, 없으면 첫 본문 이미지)를 수집한다. 이전에 가져온 EPUB은 앱 시작 시 한 번 백필한다.
- PDF: 서재에서 1쪽을 한 번 렌더링해 420px JPEG 표지로 저장하고 Info의 Title/Author를 읽는다(리더 세션을 열지 않으며, 2분짜리 단일 파일 읽기 토큰 사용). 암호화·손상 PDF는 일반 표지.
- 표지는 `covers/<documentId>.<ext>`에 저장되고 `thoth-doc://localhost/cover/<documentId>`로만 제공된다(매직 바이트 검사, 20 MiB 상한, 문서 삭제 시 함께 삭제).
- 정렬: 최근 읽은 순(읽지 않은 책은 최근 가져온 순으로 뒤에) / 제목 순(가나다 → ABC, 숫자 자연 정렬). 선택은 유지된다.
- 자가 검증(`THOTH_SELFTEST_SCENARIO=library`, EPUB 8 + PDF 8): 표지 이미지 16/16, 깨진 이미지 0, 저자 표시 15/16, 표지 비율 2:3 16/16, 제목 정렬·최근 정렬 확인.
