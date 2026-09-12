# OpenCode source adaptation

NOSH's terminal UI adapts **actual OpenCode source**, not a backend dependency.
The original SolidJS components were ported to imperative `@opentui/core` 0.5.11.
NOSH keeps its Node daemon and Bun terminal process. No OpenCode SDK, agent,
provider, configuration, persistence, or shell-command runtime is imported.

## Verified revision

- Repository: https://github.com/anomalyco/opencode (MIT).
- Current `dev` revision inspected: [`b3f1a96c6dd7adeb28b36dd11add1998fc84d67b`](https://github.com/anomalyco/opencode/commit/b3f1a96c6dd7adeb28b36dd11add1998fc84d67b).
- Verified using GitHub `GET /repos/anomalyco/opencode/commits/dev` and immutable raw source URLs on 2026-09-10.
- Previous audit revision `d6855b6b47a8433462ac6aeeba882ccf734cb7f1` was also verified through the commit API.
  The home, session, prompt, sidebar, logo, and theme sources are byte-identical
  at those two revisions. Current source snapshots and GitHub API responses are
  recorded outside the product in `audit-results/opencode-upstream/`.

## Source map (all links pin the exact inspected revision)

| Upstream source | Adaptation in NOSH |
| --- | --- |
| [routes/home.tsx](https://github.com/anomalyco/opencode/blob/b3f1a96c6dd7adeb28b36dd11add1998fc84d67b/packages/tui/src/routes/home.tsx) | `src/ui.ts`: centered logo, flexible upper/lower space, 75-column prompt, compact footer. |
| [component/prompt/index.tsx](https://github.com/anomalyco/opencode/blob/b3f1a96c6dd7adeb28b36dd11add1998fc84d67b/packages/tui/src/component/prompt/index.tsx) | `src/ui.ts`, `src/composer.ts`: left-only prompt accent, filled surface, multiline editor, role/model/thinking metadata, key hints. |
| [routes/session/index.tsx](https://github.com/anomalyco/opencode/blob/b3f1a96c6dd7adeb28b36dd11add1998fc84d67b/packages/tui/src/routes/session/index.tsx) | `src/ui.ts`, `src/visual.ts`: borderless main transcript, padded user blocks, distinct assistant/tool/status content, bottom-sticky scrolling, MarkdownRenderable. |
| [routes/session/sidebar.tsx](https://github.com/anomalyco/opencode/blob/b3f1a96c6dd7adeb28b36dd11add1998fc84d67b/packages/tui/src/routes/session/sidebar.tsx) | `src/ui.ts`: right filled sidebar, independent scroll, title/footer. Research graph, jobs, and approvals replace OpenCode's coding context. |
| [theme/assets/opencode.json](https://github.com/anomalyco/opencode/blob/b3f1a96c6dd7adeb28b36dd11add1998fc84d67b/packages/tui/src/theme/assets/opencode.json) | `src/theme.ts`: resolved dark palette. Muted text is lifted from `#808080` to `#909090`; NOSH uses upstream primary orange as its prompt accent. |
| [theme/index.ts](https://github.com/anomalyco/opencode/blob/b3f1a96c6dd7adeb28b36dd11add1998fc84d67b/packages/tui/src/theme/index.ts) | `src/theme.ts`: selected Markdown/syntax scope definitions, reduced to the scopes needed by this native scene. |
| [ui/border.ts](https://github.com/anomalyco/opencode/blob/b3f1a96c6dd7adeb28b36dd11add1998fc84d67b/packages/tui/src/ui/border.ts) | `src/theme.ts`: copied `SplitBorder.customBorderChars` merged constant (renamed `splitBorder`). |
| [component/logo.tsx](https://github.com/anomalyco/opencode/blob/b3f1a96c6dd7adeb28b36dd11add1998fc84d67b/packages/tui/src/component/logo.tsx) | Inspected for block-logo presentation. NOSH's five-row lettering is original, not the OpenCode wordmark. |

The responsive NOSH sidebar hides below 80 columns. This differs deliberately
from the upstream coding UI's wider sidebar and small-screen overlay.
NOSH inspector cards and explicit versioned confirmation flows are local code.
Unavailable OpenCode backend commands are not presented as working features.
The original project's MIT notice below applies to adapted portions, while
NOSH's own project license is unchanged.

## Upstream MIT license

Source: [LICENSE](https://github.com/anomalyco/opencode/blob/b3f1a96c6dd7adeb28b36dd11add1998fc84d67b/LICENSE)

```text
MIT License

Copyright (c) 2025 opencode

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
