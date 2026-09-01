# HTML report template

One self-contained dark HTML file in `/tmp`. No Tailwind, no CSS framework, no build step. Everything below is hand-written CSS in one `<style>` block plus Mermaid from a CDN. The palette, the accent bar on every `h2`, the terminal kicker and the pill badges are the house style. Do not substitute a framework for them.

## 1. Scaffold

Copy this verbatim. Only the `<title>`, the kicker line and the sections change.

```html
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Architecture review: {{repo}}</title>
<script type="module">
  import mermaid from "https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.esm.min.mjs";
  mermaid.initialize({
    startOnLoad: true,
    securityLevel: "loose",
    theme: "base",
    themeVariables: {
      darkMode: true,
      background: "#11151d",
      primaryColor: "#161b26",
      primaryTextColor: "#e6e9f0",
      primaryBorderColor: "#232838",
      secondaryColor: "#0d1119",
      tertiaryColor: "#0a0d13",
      lineColor: "#8b93a6",
      textColor: "#e6e9f0",
      fontSize: "13px",
      fontFamily: 'ui-monospace,SFMono-Regular,"SF Mono",Menlo,Consolas,monospace'
    }
  });
</script>
<style>
  :root{
    --bg:#0a0d13; --panel:#11151d; --panel-2:#161b26; --ink:#e6e9f0; --sub:#8b93a6;
    --border:#232838; --accent:#6c93ff; --mono-chip:#0d1119;
    --strong:#3ddc97;   --strong-bg:rgba(61,220,151,.08);
    --explore:#f2b84b;  --explore-bg:rgba(242,184,75,.08);
    --leak:#ff6b6b;     --leak-bg:rgba(255,107,107,.08);
    --spec:#8b93a6;     --spec-bg:rgba(139,147,166,.08);
  }
  *{box-sizing:border-box;}
  body{margin:0;background:var(--bg);color:var(--ink);font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;line-height:1.6;}
  ::selection{background:rgba(108,147,255,.28);}
  a{color:var(--accent);}
  :focus-visible{outline:2px solid var(--accent);outline-offset:2px;border-radius:3px;}
  .wrap{max-width:1080px;margin:0 auto;padding:44px 24px 90px;}

  .kicker{font-family:ui-monospace,SFMono-Regular,"SF Mono","Cascadia Code",Menlo,Consolas,monospace;font-size:12px;color:var(--accent);letter-spacing:.02em;margin:0 0 10px;}
  .kicker::before{content:"$ ";color:var(--sub);}
  header.top{margin-bottom:34px;padding-bottom:22px;border-bottom:1px solid var(--border);}
  header.top h1{font-size:25px;margin:0 0 6px;letter-spacing:-.01em;font-weight:650;}
  header.top p{color:var(--sub);margin:0 0 6px;font-size:13.5px;}

  section{margin-bottom:38px;}
  section h2{font-size:16px;margin:0 0 12px;font-weight:650;display:flex;align-items:center;gap:9px;}
  section h2::before{content:"";width:3px;height:15px;background:var(--accent);border-radius:2px;display:inline-block;flex:none;}
  section > p.lead{color:var(--sub);font-size:13px;margin:-4px 0 14px;}

  /* candidate card */
  article.cand{background:var(--panel);border:1px solid var(--border);border-left:3px solid var(--dot,var(--sub));border-radius:10px;padding:16px 18px;margin-bottom:14px;}
  article.cand.s-strong{--dot:var(--strong);} article.cand.s-explore{--dot:var(--explore);} article.cand.s-spec{--dot:var(--spec);}
  article.cand > h3{margin:0 0 8px;font-size:14.5px;font-weight:650;display:flex;align-items:center;gap:9px;flex-wrap:wrap;}
  article.cand > h3 .id{font-family:ui-monospace,Menlo,Consolas,monospace;color:var(--dot);font-size:12px;}
  .files{font-family:ui-monospace,Menlo,Consolas,monospace;font-size:11.5px;color:var(--sub);margin:0 0 12px;}
  .files div{margin-bottom:1px;}
  .ps{font-size:13px;margin:0 0 8px;}
  .ps b{color:var(--ink);font-weight:650;}
  .wins{margin:12px 0 0;padding-left:16px;font-size:12.8px;}
  .wins li{margin-bottom:4px;}

  /* stat strip */
  .stats{display:grid;grid-template-columns:repeat(4,1fr);gap:12px;margin-bottom:34px;}
  .stats .s{background:var(--panel);border:1px solid var(--border);border-radius:10px;padding:13px 15px;}
  .stats .s .v{font-size:21px;font-weight:650;letter-spacing:-.01em;}
  .stats .s .k{font-size:10.5px;color:var(--sub);text-transform:uppercase;letter-spacing:.04em;margin-top:2px;}

  table{width:100%;border-collapse:collapse;font-size:12.8px;background:var(--panel);border:1px solid var(--border);border-radius:10px;overflow:hidden;}
  th,td{padding:9px 11px;text-align:left;vertical-align:top;border-bottom:1px solid var(--border);}
  thead th{background:var(--panel-2);font-size:10.5px;text-transform:uppercase;letter-spacing:.04em;color:var(--sub);font-weight:650;}
  tbody tr:last-child td{border-bottom:none;}
  tbody tr:hover td{background:rgba(255,255,255,.015);}
  .tbl-wrap{overflow-x:auto;}
  code{font-family:ui-monospace,SFMono-Regular,"SF Mono","Cascadia Code",Menlo,Consolas,monospace;font-size:11.8px;background:var(--mono-chip);color:#a9c3ff;border:1px solid var(--border);padding:1px 5px;border-radius:5px;}
  pre.term{background:var(--mono-chip);border:1px solid var(--border);border-radius:8px;padding:11px 13px;overflow-x:auto;font-family:ui-monospace,Menlo,Consolas,monospace;font-size:11.8px;color:#a9c3ff;margin:10px 0;}

  .badge{display:inline-block;font-size:10px;font-weight:650;text-transform:uppercase;letter-spacing:.02em;padding:2px 8px;border-radius:99px;border:1px solid;white-space:nowrap;}
  .b-strong{background:var(--strong-bg);color:var(--strong);border-color:var(--strong);}
  .b-explore{background:var(--explore-bg);color:var(--explore);border-color:var(--explore);}
  .b-spec{background:var(--spec-bg);color:var(--spec);border-color:var(--spec);}
  .b-leak{background:var(--leak-bg);color:var(--leak);border-color:var(--leak);}

  .callout{border-left:3px solid var(--accent);background:var(--panel-2);padding:11px 15px;border-radius:0 8px 8px 0;font-size:12.8px;margin:12px 0 0;}
  .callout.good{border-left-color:var(--strong);}
  .callout.warn{border-left-color:var(--explore);}
  .callout.bad{border-left-color:var(--leak);}
  .callout b{color:var(--ink);}

  /* mass & depth: interface vs implementation */
  .mass{display:grid;grid-template-columns:1fr 1fr;gap:12px;margin:14px 0 0;}
  .mass .box{background:var(--panel-2);border:1px solid var(--border);border-radius:9px;padding:13px;}
  .mass .box h4{margin:0 0 10px;font-size:10.5px;text-transform:uppercase;letter-spacing:.04em;color:var(--sub);font-weight:650;}
  .bar{border-radius:6px;padding:8px;text-align:center;font-family:ui-monospace,Menlo,Consolas,monospace;font-size:11px;}
  .bar.iface-shallow{background:repeating-linear-gradient(135deg,rgba(242,184,75,.14),rgba(242,184,75,.14) 6px,rgba(242,184,75,.05) 6px,rgba(242,184,75,.05) 12px);border:1px solid var(--explore);color:var(--explore);padding:14px 8px;}
  .bar.iface-deep{background:linear-gradient(135deg,#1b2434,#232e44);border:1px solid var(--accent);color:var(--ink);}
  .bar.impl-thin{background:var(--mono-chip);border:1px solid var(--border);color:var(--sub);}
  .bar.impl-deep{background:linear-gradient(135deg,#0f172a,#1e293b);border:1px solid var(--border);color:var(--ink);padding:34px 8px;}
  .arrows{text-align:center;color:var(--sub);font-size:15px;line-height:1;margin:5px 0;}
  .mass .note{font-size:11.5px;color:var(--sub);margin:9px 0 0;}
  .mass .tally{font-family:ui-monospace,Menlo,Consolas,monospace;font-size:11px;color:var(--sub);margin-top:9px;}
  .mass .tally div{margin-bottom:1px;}

  .mermaid{background:var(--panel-2);border:1px solid var(--border);border-radius:9px;padding:10px;overflow-x:auto;}
  .diagrams{display:grid;grid-template-columns:1fr 1fr;gap:12px;margin-top:14px;}
  .diagrams h4{margin:0 0 8px;font-size:10.5px;text-transform:uppercase;letter-spacing:.04em;color:var(--sub);font-weight:650;}

  footer{color:var(--sub);font-size:11.5px;text-align:center;margin-top:44px;padding-top:18px;border-top:1px solid var(--border);}

  @media (max-width:800px){.stats{grid-template-columns:1fr 1fr;} .mass,.diagrams{grid-template-columns:1fr;}}
  @media (max-width:560px){.stats{grid-template-columns:1fr;} .wrap{padding:28px 16px 70px;} header.top h1{font-size:20px;}}
</style>
</head>
<body>
<div class="wrap">

<p class="kicker">architecture-review {{repo}} --hotspots --since=6w</p>
<header class="top">
  <h1>{{repo}} / architecture review</h1>
  <p>{{date}}, branch <code>{{branch}}</code> at <code>{{sha}}</code>. Legend: solid = module, dashed = seam, red = leak, dark = deep module.</p>
  <p>{{one paragraph: what was scoped, where the hotspots came from, what was deliberately not reviewed}}</p>
</header>

<!-- sections go here -->

<footer>{{provenance: what was confirmed by running something, what was read only, and whether the tree is clean}}</footer>
</div>
</body>
</html>
```

## 2. Page order

1. `header.top` with kicker.
2. **Confirmation table.** One row per candidate, saying how each claim was checked. Put it first. A reviewer decides how much to trust the rest from this table.
3. **Corrections table**, on a re-review only. Every number this review changed since the last pass: the claim, what was said before, what was confirmed, and why the first answer was wrong. Skip the section on a first pass rather than leaving it empty.
4. **Stat strip.** Four numbers that carry the argument. Real counts, never rounded prose.
5. **Candidates.** One `<section>` per candidate holding one `article.cand`.
6. **Top recommendation.**
7. `footer`.

## 3. Confirmation table

Every candidate is checked before it ships. A claim read off the source and a claim proven by a failing test are not the same claim, and the report says which it is.

```html
<section id="confirm">
  <h2>Every candidate is confirmed, not read off the source</h2>
  <div class="tbl-wrap"><table>
    <thead><tr><th style="width:52px">ID</th><th style="width:170px">How</th><th>What the check showed</th></tr></thead>
    <tbody>
      <tr><td><code>C1</code></td><td><span class="badge b-strong">Confirmed by test</span></td><td>A temporary test reproduced the defect and reported a worse number than this review first claimed. Reverted.</td></tr>
      <tr><td><code>C3</code></td><td><span class="badge b-explore">Corrected</span></td><td>The first count said 35. A naive grep had swept up nested arms. The real shape is 14 plus 14.</td></tr>
      <tr><td><code>C6</code></td><td><span class="badge b-spec">Read only</span></td><td>Confirmed by reading. Nothing was run.</td></tr>
    </tbody>
  </table></div>
  <div class="callout good">Probes that touched the tree, and that each one was reverted. State <code>git status</code> plainly.</div>
</section>
```

Rules:

- Report a corrected number as a correction, in the card too, and in the corrections table on a re-review. A silently fixed count reads as a count nobody checked.
- Correct your own withdrawn claims as well as your wrong ones. A cost objection dropped on reasoning is unfinished until there is a number.
- If a claim could not be checked, say `Read only` rather than dressing it up.
- Revert every probe and say so.

## 4. Candidate card

```html
<section id="c1">
  <h2>C1. Parse geometry properties once into the retained node</h2>
  <article class="cand s-strong">
    <h3><span class="id">C1</span> Parse geometry properties once into the retained node
      <span class="badge b-strong">Strong</span>
      <span class="badge b-spec">Deepening</span>
    </h3>

    <div class="files">
      <div>renderer/src/layout/scene.rs</div>
      <div>renderer/src/layout/node/style.rs</div>
    </div>

    <p class="ps"><b>Problem.</b> One sentence naming the root cause of the friction.</p>
    <p class="ps"><b>Solution.</b> One sentence naming the exact change to the interface.</p>

    <div class="callout bad">
      <b>Confirmed. A temporary test made the passes disagree.</b>
      <pre class="term">margin.left read 4 times in one pass; row sized 30, child placed at x=40</pre>
      One short paragraph reading the output back in plain terms.
    </div>

    <!-- mass diagram or mermaid pair goes here -->

    <ul class="wins">
      <li>Interface shrinks; implementation absorbs the parse.</li>
      <li>Locality: geometry validation in one module.</li>
      <li>Leverage: one parse, twelve readers.</li>
    </ul>

    <div class="callout warn"><b>ADR conflict, ADR-0023.</b> One line. Why the friction justifies reopening it.</div>
  </article>
</section>
```

Badge vocabulary. Strength is `b-strong` (Strong), `b-explore` (Worth exploring), `b-spec` (Speculative). The card's left rule follows it through `s-strong` / `s-explore` / `s-spec`. A second badge names the kind: Deepening, Dependency, Seam.

Wins are six words or fewer and use the strict vocabulary from `SKILL.md`. "Locality: bugs concentrate in one module." "Leverage: one interface, N call sites." "Interface shrinks; implementation absorbs complexity."

## 5. Diagram patterns

Pick the one that exposes the flaw. Never both for the same point.

### Mass and depth

For a shallow module. A tall interface over a thin implementation is the shape to show.

```html
<div class="mass">
  <div class="box">
    <h4>Before. Shallow, re-entered</h4>
    <div class="bar iface-shallow">interface: 12 parsers &times; (map, key)</div>
    <div class="arrows">&#8595;&#8595;&#8595;&#8595;&#8595;&#8595;</div>
    <div class="bar impl-thin">implementation: 33 call sites</div>
    <div class="tally"><div>parse_align &nbsp;10&times;</div><div>parse_edge_insets &nbsp;9&times;</div></div>
  </div>
  <div class="box">
    <h4>After. Deep, entered once</h4>
    <div class="bar iface-deep">interface: LayoutStyle</div>
    <div class="arrows">&#8595;</div>
    <div class="bar impl-deep">implementation:<br>one parse per node per pass<br>ranges, validation, rejection</div>
    <p class="note">Every reader takes a struct. No reader takes a key.</p>
  </div>
</div>
```

### Mermaid pair

For call flow and leaked abstractions across a seam. Mermaid inherits the dark theme from the `themeVariables` in the scaffold, so write no colours inline beyond the `leak` class.

```html
<div class="diagrams">
  <div><h4>Before</h4><pre class="mermaid">
flowchart TB
  A[Scene::apply] --> B[resolve_and_reconcile]
  B --> B1[identity + lease]
  B --> B2[size + position]
  B --> B3[re-parse properties]
  classDef leak stroke:#ff6b6b,stroke-width:2px;
  class B,B3 leak
  </pre></div>
  <div><h4>After</h4><pre class="mermaid">
flowchart TB
  A[Scene::apply] --> R[reconcile: identity + lease]
  R --> S[LayoutStyle per node]
  S -.seam.-> T[solver]
  T --> O[rect + content extent]
  </pre></div>
</div>
```

Use `-.label.->` for a seam and the `leak` classDef for a leak. Those two are the whole visual grammar.

## 6. Top recommendation

One card at the end. Name the target, link to it, and give the reason in one sentence tied to a number the report already established.

```html
<section id="top">
  <h2>Top recommendation</h2>
  <article class="cand s-strong">
    <h3><span class="id">C1</span> <a href="#c1">Parse geometry properties once into the retained node</a></h3>
    <p class="ps">Why this one has the most leverage for the least work, in one sentence, citing a number from its own card.</p>
    <p class="ps">If the review answered a direct question the user asked, answer it again here in two sentences.</p>
  </article>
</section>
```

## 7. Prose rules

The `unslop` skill applies to every word in the file.

- No em dashes. End the sentence or use a comma.
- Sentence case headings. No title case, no emoji.
- Active voice. Name the actor.
- Numbers, not adjectives. "33 call sites", not "many call sites". "742 tests pass", not "tests pass cleanly".
- Banned nouns: component, service, unit, API, boundary, wrapper. Say module, interface, seam, adapter, depth, leverage, locality.
- No hedging and no closing summary paragraph. The top recommendation is the ending.
