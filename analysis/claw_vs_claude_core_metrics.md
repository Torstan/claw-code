# Claw vs. Claude Code Core Metrics Comparison (Grouped by Request Summary)

## Conclusion

After aggregating records with the same shared `Request summary`, Claw has a much lower weighted input-side cost than Claude Code, while producing longer outputs and having a slightly lower cache hit rate.

| Key metric | Claude Code | Claw | Claw / Claude | Conclusion |
|---|---:|---:|---:|---|
| **Core input tokens** | **25,199.65** | **8,524.60** | **0.34x (-66.17%)** | **Claw has a significantly lower weighted input-side cost** |
| **Core total tokens** | **27,351.65** | **13,903.60** | **0.51x (-49.17%)** | **Claw uses roughly half of Claude Code's total core tokens** |
| **Core cache hit** | **95.20%** | **92.73%** | **0.97x (-2.59%)** | **Claw's cache hit rate is 2.47 percentage points lower** |
| **Core output tokens** | **2,152** | **5,379** | **2.50x (+149.95%)** | **Claw produces longer outputs, increasing output-side cost** |
| **Request tokens** | **1,557** | **250** | **0.16x (-83.94%)** | **Claw uses fewer raw request input tokens** |

Key takeaways:

- **Claw's `core input tokens` are 33.83% of Claude Code's**, showing a clear input-side cost advantage.
- **Claw's `core total tokens` are 50.83% of Claude Code's**, so total core usage remains lower even though Claw outputs more tokens.
- **Claw's cache hit rate is 92.73%, while Claude Code's is 95.20%**, so Claw is lower by **2.47 pp**.
- **Claw's output tokens are 2.50x Claude Code's**, meaning Claw gives longer answers; whether that is beneficial depends on answer quality.

## Experiment Method

Data sources:

- Claw: `claw.all.1027.orig.json`
- Claude Code: `claude.req.all2351.json`

Comparison method:

- Claude Code records are first grouped by full `Request summary`; req/resp/cache/core data under the same summary are summed.
- Claw records are grouped by the same `Request summary` key before comparison.
- The core comparison only includes `Request summary` values that exist in both datasets.
- Claw-only requests are listed separately and excluded from the shared-request comparison.

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
| **Input token advantage** | Claw uses **8,524.60 core input tokens** vs. Claude Code's **25,199.65**, or **0.34x (-66.17%)**. Raw request tokens are also much lower: **250** vs. **1,557**. | **P0:** Claw has fewer auxiliary calls than Claude Code in this sample. Claude Code includes Haiku/Opus helper calls such as title, clarification, and memory-related calls under the same request summaries.<br>**P1:** Claw uses a smaller and more constrained prompt/tool surface.<br>**P2:** Prompt layering keeps stable context compact and avoids repeatedly inflating the cache-sensitive prefix. |
| **Cache hit-rate change** | Claw does **not** improve cache hit rate in this sample. It is **92.73%** vs. Claude Code's **95.20%**, lower by **2.47 pp**. | **P0:** Session affinity via `X-Claude-Code-Session-Id` keeps requests in a stable cache domain.<br>**P1:** Only the latest user message receives `cache_control`, keeping older conversation prefix bytes stable.<br>**P2:** Stable/dynamic/attachment prompt layering reduces accidental cache breaks. These optimizations keep Claw close to Claude Code, but not above it in this dataset. |
| **Output token increase** | Claw outputs **5,379 core output tokens** vs. Claude Code's **2,152**, or **2.50x (+149.95%)**. | **P0:** Claw's system prompt lack of output standard, e.g. "Your responses should be short and concise." |

### Shared Request Summary Comparison (Claude Code Aggregated as Baseline)

| Request summary | Claude N | Claw N | Claude hit | Claw hit | Hit delta | Claude core input | Claw core input | Core input ratio | Claude core output | Claw core output | Core output ratio | Claude core total | Claw core total | Core total ratio |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| As an senior engineer, should I learn rust? | 2 | 2 | 98.72% | 94.49% | -4.23 pp | 3,550.65 | 2,683.40 | 0.76x (-24.43%) | 748 | 1,800 | 2.41x (+140.64%) | 4,298.65 | 4,483.40 | 1.04x (+4.30%) |
| what is the difference between cpp and rust? | 4 | 2 | 92.82% | 93.09% | +0.27 pp | 18,166.70 | 2,545.75 | 0.14x (-85.99%) | 870 | 1,126 | 1.29x (+29.43%) | 19,036.70 | 3,671.75 | 0.19x (-80.71%) |
| which one is more useful in AI? | 2 | 2 | 98.76% | 90.60% | -8.16 pp | 3,482.30 | 3,295.45 | 0.95x (-5.37%) | 534 | 2,453 | 4.59x (+359.36%) | 4,016.30 | 5,748.45 | 1.43x (+43.13%) |
| **Total** | **8** | **6** | **95.20%** | **92.73%** | **-2.47 pp** | **25,199.65** | **8,524.60** | **0.34x (-66.17%)** | **2,152** | **5,379** | **2.50x (+149.95%)** | **27,351.65** | **13,903.60** | **0.51x (-49.17%)** |

### Shared Request Summary Aggregate (Claude Code as Baseline)

| Metric | Claude Code | Claw | Claw - Claude | Claw / Claude | Notes |
|---|---:|---:|---:|---:|---|
| Record count | 8 | 6 | -2 | 0.75x (-25.00%) | Total sample records; Claude Code has more records because it includes Haiku/Opus helper calls under the same summaries. |
| Request tokens | 1,557 | 250 | -1,307 | 0.16x (-83.94%) | Raw input tokens. |
| Response tokens | 2,152 | 5,379 | 3,227 | 2.50x (+149.95%) | Output tokens. |
| Cache create tokens | 7,311 | 3,276 | -4,035 | 0.45x (-55.19%) | Newly created cache tokens. |
| Cache read tokens | 145,039 | 41,796 | -103,243 | 0.29x (-71.18%) | Cache-read tokens. |
| Core cache hit | 95.20% | 92.73% | -2.47 pp | 0.97x (-2.59%) | Recomputed from aggregated read/create tokens. |
| Core input tokens | 25,199.65 | 8,524.60 | -16,675.05 | 0.34x (-66.17%) | Input-side cost after applying the core formula. |
| Core output tokens | 2,152 | 5,379 | 3,227 | 2.50x (+149.95%) | Same as aggregated output tokens. |
| Core total tokens | 27,351.65 | 13,903.60 | -13,448.05 | 0.51x (-49.17%) | Core input + core output. |

### Claude Code Aggregated by Request Summary

| Request summary | N | LLM | Request tokens | Response tokens | Cache create | Cache read | Core cache hit | Core input | Core output | Core total |
|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| As an senior engineer, should I learn rust? | 2 | claude-haiku-4-5-20251001 x1, claude-opus-4-6 x1 | 13 | 748 | 395 | 30,439 | 98.72% | 3,550.65 | 748 | 4,298.65 |
| what is the difference between cpp and rust? | 4 | claude-haiku-4-5-20251001 x1, claude-opus-4-6 x3 | 1,533 | 870 | 6,540 | 84,587 | 92.82% | 18,166.70 | 870 | 19,036.70 |
| which one is more useful in AI? | 2 | claude-haiku-4-5-20251001 x1, claude-opus-4-6 x1 | 11 | 534 | 376 | 30,013 | 98.76% | 3,482.30 | 534 | 4,016.30 |

### Claw Aggregated by Request Summary

| Request summary | N | LLM | Request tokens | Response tokens | Cache create | Cache read | Core cache hit | Core input | Core output | Core total |
|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| As an senior engineer, should I learn rust? | 2 | Claude Opus 4.6 x2 | 86 | 1,800 | 876 | 15,024 | 94.49% | 2,683.40 | 1,800 | 4,483.40 |
| Is Python hard for a teenager? | 1 | Claude Opus 4.6 x1 | 39 | 465 | 670 | 8,732 | 92.87% | 1,749.70 | 465 | 2,214.70 |
| source /xxxx/bin/activate | 1 | Claude Opus 4.6 x1 | 52 | 117 | 1,143 | 9,269 | 89.02% | 2,407.65 | 117 | 2,524.65 |
| what is the difference between cpp and rust? | 2 | Claude Opus 4.6 x2 | 86 | 1,126 | 947 | 12,760 | 93.09% | 2,545.75 | 1,126 | 3,671.75 |
| which one is more intresting to learn? | 1 | Claude Opus 4.6 x1 | 41 | 543 | 1,516 | 8,131 | 84.29% | 2,749.10 | 543 | 3,292.10 |
| which one is more useful in AI? | 2 | Claude Opus 4.6 x2 | 78 | 2,453 | 1,453 | 14,012 | 90.60% | 3,295.45 | 2,453 | 5,748.45 |

### Claw-only Request Summaries

These summaries do not have matching groups in the Claude Code sample, so they are excluded from the shared-request comparison.

| Request summary | N | LLM | Request tokens | Response tokens | Cache create | Cache read | Core cache hit | Core input | Core output | Core total |
|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| Is Python hard for a teenager? | 1 | Claude Opus 4.6 x1 | 39 | 465 | 670 | 8,732 | 92.87% | 1,749.70 | 465 | 2,214.70 |
| source /xxx/bin/activate | 1 | Claude Opus 4.6 x1 | 52 | 117 | 1,143 | 9,269 | 89.02% | 2,407.65 | 117 | 2,524.65 |
| which one is more intresting to learn? | 1 | Claude Opus 4.6 x1 | 41 | 543 | 1,516 | 8,131 | 84.29% | 2,749.10 | 543 | 3,292.10 |
