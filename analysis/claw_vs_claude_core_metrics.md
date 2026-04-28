# Claw vs. Claude Code Core Metrics Comparison (Grouped by Request Summary)

## Conclusion

After updating Claw with the new `clawd.1735.all.json` dataset, Claw keeps a much lower weighted input-side cost and now also has lower output tokens than Claude Code on the shared request set. Its cache hit rate remains lower than Claude Code in this sample.

| Key metric | Claude Code | Claw | Claw / Claude | Conclusion |
|---|---:|---:|---:|---|
| **Core input tokens** | **25,199.65** | **5,732.55** | **0.23x (-77.25%)** | **Claw has a significantly lower weighted input-side cost** |
| **Core total tokens** | **27,351.65** | **6,926.55** | **0.25x (-74.68%)** | **Claw uses far fewer total core tokens** |
| **Core cache hit** | **95.20%** | **87.42%** | **0.92x (-8.17%)** | **Claw's cache hit rate is 7.78 percentage points lower** |
| **Core output tokens** | **2,152** | **1,194** | **0.55x (-44.52%)** | **Claw outputs fewer tokens in the current dataset** |
| **Request tokens** | **1,557** | **125** | **0.08x (-91.97%)** | **Claw uses fewer raw request input tokens** |

Key takeaways:

- **Claw's `core input tokens` are 22.75% of Claude Code's**, showing a clear input-side cost advantage.
- **Claw's `core total tokens` are 25.32% of Claude Code's**, so total core usage is much lower.
- **Claw's output tokens are 55.48% of Claude Code's**, so output-side usage is now lower than Claude Code on the shared request set.
- **Claw's cache hit rate is 87.42%, while Claude Code's is 95.20%**, so Claw is lower by **7.78 pp**.

## Experiment Method

Data sources:

- Claw: `clawd.1735.all.json`
- Claude Code: `analysis/claude.req.all2351.json`

Comparison method:

- Claude Code records are first grouped by meaningful user request summary; follow-up tool/memory calls are attributed to the user request that triggered them.
- Claw records are grouped by the same request-summary key before comparison.
- The core comparison only includes `Request summary` values that exist in both datasets.
- Claw-only requests, if any, are listed separately and excluded from the shared-request comparison.

Core metric formulas:

- `Core input tokens = input_tokens + cache_creation_input_tokens * 1.25 + cache_read_input_tokens * 0.1`
- `Core output tokens = output_tokens`
- `Core total tokens = Core input tokens + Core output tokens`
- `Core cache hit = cache_read_input_tokens / (cache_read_input_tokens + cache_creation_input_tokens)`
- `Core cache hit` is not summed directly. It is recomputed from the aggregated `cache_read_input_tokens` and `cache_creation_input_tokens`.

## Experiment Data

### Experiment Data Analysis

| Area | Observed result | Main drivers, ranked by impact |
|---|---|---|
| **Input token advantage** | Claw uses **5,732.55 core input tokens** vs. Claude Code's **25,199.65**, or **0.23x (-77.25%)**. Raw request tokens are also much lower: **125** vs. **1,557**. | **P0:** Claw has fewer auxiliary calls than Claude Code in this sample. Claude Code includes Haiku/Opus helper calls under the same request summaries.<br>**P1:** Claw uses a smaller and more constrained prompt/tool surface.<br>**P2:** Prompt layering keeps stable context compact and avoids repeatedly inflating the cache-sensitive prefix. |
| **Cache hit-rate change** | Claw does **not** improve cache hit rate in this sample. It is **87.42%** vs. Claude Code's **95.20%**, lower by **7.78 pp**. | **P0:** Claw has fewer repeated auxiliary calls, so it accumulates less cache-read volume than Claude Code: **20,038** vs. **145,039** cache-read tokens.<br>**P1:** Claw creates proportionally more cache tokens relative to reads: **2,883 create / 20,038 read** vs. Claude Code's **7,311 create / 145,039 read**.<br>**P2:** Session affinity and prompt layering still keep cache reuse high, but they do not exceed Claude Code's hit rate in this dataset. |
| **Output token reduction** | Claw outputs **1,194 core output tokens** vs. Claude Code's **2,152**, or **0.55x (-44.52%)**. | **P0:** Current Claw responses are shorter on all three shared request summaries.<br>**P1:** The shared Claw records are direct answer calls with no extra memory/tool-call output attributed to the same summaries.<br>**P2:** Lower output tokens reduce total core cost without relying only on input-side savings. |

### Shared Request Summary Comparison (Claude Code Aggregated as Baseline)

| Request summary | Claude N | Claw N | Claude hit | Claw hit | Hit delta | Claude core input | Claw core input | Core input ratio | Claude core output | Claw core output | Core output ratio | Claude core total | Claw core total | Core total ratio |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| As an senior engineer, should I learn rust? | 4 | 1 | 98.92% | 83.48% | -15.44 pp | 10,592.80 | 2,511.10 | 0.24x (-76.29%) | 839 | 449 | 0.54x (-46.48%) | 11,431.80 | 2,960.10 | 0.26x (-74.11%) |
| what is the difference between cpp and rust? | 2 | 1 | 79.21% | 88.86% | +9.64 pp | 11,124.55 | 1,648.20 | 0.15x (-85.18%) | 779 | 376 | 0.48x (-51.73%) | 11,903.55 | 2,024.20 | 0.17x (-82.99%) |
| which one is more useful in AI? | 2 | 1 | 98.76% | 90.60% | -8.16 pp | 3,482.30 | 1,573.25 | 0.45x (-54.82%) | 534 | 369 | 0.69x (-30.90%) | 4,016.30 | 1,942.25 | 0.48x (-51.64%) |
| **Total** | **8** | **3** | **95.20%** | **87.42%** | **-7.78 pp** | **25,199.65** | **5,732.55** | **0.23x (-77.25%)** | **2,152** | **1,194** | **0.55x (-44.52%)** | **27,351.65** | **6,926.55** | **0.25x (-74.68%)** |

### Shared Request Summary Aggregate (Claude Code as Baseline)

| Metric | Claude Code | Claw | Claw - Claude | Claw / Claude | Notes |
|---|---:|---:|---:|---:|---|
| Record count | 8 | 3 | -5 | 0.38x (-62.50%) | Total sample records; Claude Code has more records because it includes Haiku/Opus helper calls under the same summaries. |
| Request tokens | 1,557 | 125 | -1,432 | 0.08x (-91.97%) | Raw input tokens. |
| Response tokens | 2,152 | 1,194 | -958 | 0.55x (-44.52%) | Output tokens. |
| Cache create tokens | 7,311 | 2,883 | -4,428 | 0.39x (-60.57%) | Newly created cache tokens. |
| Cache read tokens | 145,039 | 20,038 | -125,001 | 0.14x (-86.18%) | Cache-read tokens. |
| Core cache hit | 95.20% | 87.42% | -7.78 pp | 0.92x (-8.17%) | Recomputed from aggregated read/create tokens. |
| Core input tokens | 25,199.65 | 5,732.55 | -19,467.10 | 0.23x (-77.25%) | Input-side cost after applying the core formula. |
| Core output tokens | 2,152 | 1,194 | -958 | 0.55x (-44.52%) | Same as aggregated output tokens. |
| Core total tokens | 27,351.65 | 6,926.55 | -20,425.10 | 0.25x (-74.68%) | Core input + core output. |

### Claude Code Aggregated by Request Summary

| Request summary | N | LLM | Request tokens | Response tokens | Cache create | Cache read | Core cache hit | Core input | Core output | Core total |
|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| As an senior engineer, should I learn rust? | 4 | claude-haiku-4-5-20251001 x1, claude-opus-4-6 x3 | 92 | 839 | 1,006 | 92,433 | 98.92% | 10,592.80 | 839 | 11,431.80 |
| what is the difference between cpp and rust? | 2 | claude-haiku-4-5-20251001 x1, claude-opus-4-6 x1 | 1,454 | 779 | 5,929 | 22,593 | 79.21% | 11,124.55 | 779 | 11,903.55 |
| which one is more useful in AI? | 2 | claude-haiku-4-5-20251001 x1, claude-opus-4-6 x1 | 11 | 534 | 376 | 30,013 | 98.76% | 3,482.30 | 534 | 4,016.30 |

### Claw Aggregated by Request Summary

| Request summary | N | LLM | Request tokens | Response tokens | Cache create | Cache read | Core cache hit | Core input | Core output | Core total |
|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| As an senior engineer, should I learn rust? | 1 | Claude Opus 4.6 x1 | 43 | 449 | 1,406 | 7,106 | 83.48% | 2,511.10 | 449 | 2,960.10 |
| what is the difference between cpp and rust? | 1 | Claude Opus 4.6 x1 | 43 | 376 | 784 | 6,252 | 88.86% | 1,648.20 | 376 | 2,024.20 |
| which one is more useful in AI? | 1 | Claude Opus 4.6 x1 | 39 | 369 | 693 | 6,680 | 90.60% | 1,573.25 | 369 | 1,942.25 |

