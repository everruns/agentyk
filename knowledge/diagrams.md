---
type: Design
title: Diagram specification
description: Source, rendering, placement, visual language, accessibility, and review rules for technical diagrams.
tags: [documentation, diagrams, svg, mermaid, visual-design]
timestamp: 2026-07-25
---

# Diagram specification

## Status

Implemented.

## Intent

Technical diagrams should be readable, diffable, consistent across public
documentation, and independent of a particular documentation renderer. This
spec is adopted from
[`everruns/everruns/specs/diagrams.md`](https://github.com/everruns/everruns/blob/main/specs/diagrams.md),
with one deliberate exception: additional colors are allowed when they carry
necessary semantic information.

## Source and placement

- The rendered format is hand-authored SVG.
- Every SVG must have a co-located Mermaid `.mmd` source describing the same
  entities, relationships, and flow.
- The `.mmd` is the source of truth for information architecture; the SVG is
  the source of truth for presentation.
- Update the `.mmd` first, then update the SVG to match.
- Co-locate both files with the Markdown page that embeds them.
- Embed the SVG with a relative path:
  `![descriptive alt text](./<diagram-name>.svg)`.
- A diagram used by one page takes the page slug as its name. Multiple
  diagrams on one page add a short suffix, such as
  `architecture-layers.svg`.
- Shared diagrams may remain beside their primary owner page.

## Dimensions

- Use an 800px-wide `viewBox`; vary height to fit the content tightly,
  typically between 300 and 500px.
- Set `fill="none"` on the root `<svg>`.
- Do not set root `width` or `height`; the browser controls responsive scale.
- Add an explicit white background rectangle without a stroke so the diagram
  blends into the page.
- Verify that 10px text remains readable when the diagram is displayed at
  approximately 400px wide.

## Color

The default palette is grayscale with Navy as the accent. Do not use gradients.

| Element | Hex | Name |
| --- | --- | --- |
| Background | `#FFFFFF` | White |
| Box fill | `#F5F5F5` | Smoke |
| Box stroke and primary text | `#0A0A0A` | Obsidian |
| Secondary text | `#404040` | Slate |
| Muted text and section headers | `#A0A0A0` | Silver |
| Primary arrows and step badges | `#0A1636` | Navy |
| Step badge text | `#FFFFFF` | White |
| Secondary connectors | `#A0A0A0` | Silver |
| Annotation box stroke | `#404040` | Slate |

Additional colors are permitted only when they materially distinguish
semantic states or categories that the default palette cannot communicate
clearly. When used:

- color must carry meaning, never decoration;
- the distinction must also have a text label, shape, icon, or line pattern so
  color is not the only signal;
- the diagram must include a legend when the meaning is not self-evident;
- text and essential geometry must retain accessible contrast;
- use the smallest number of additional colors needed.

## Typography

Each SVG embeds its own style block and has no external CSS:

```xml
<style>
  text { font-family: 'Geist', 'Inter', system-ui, sans-serif; }
  .label { font-size: 13px; fill: #0A0A0A; font-weight: 600; }
  .sublabel { font-size: 11px; fill: #404040; font-weight: 400; }
  .header-text { font-size: 11px; fill: #A0A0A0; font-weight: 400; letter-spacing: 0.08em; text-transform: uppercase; }
  .mono { font-family: 'Geist Mono', 'SF Mono', monospace; font-size: 10px; fill: #404040; }
  .step-num { font-size: 10px; fill: #FFFFFF; font-weight: 600; }
  .arrow-label { font-size: 10px; fill: #0A1636; font-weight: 500; }
</style>
```

| Class | Purpose | Size | Weight |
| --- | --- | --- | --- |
| `.label` | Box title | 13px | 600 |
| `.sublabel` | Box description | 11px | 400 |
| `.header-text` | Category label above a box | 11px uppercase | 400 |
| `.mono` | Technical detail | 10px monospace | 400 |
| `.step-num` | Number inside a step badge | 10px | 600 |
| `.arrow-label` | Text beside an arrow | 10px | 500 |

## Geometry

- Use 0px corner radius everywhere.
- Boxes use a 1px Obsidian stroke.
- Primary arrows use a 1.5px Navy stroke.
- Arrowheads are inline, filled Navy polygons 10px wide. Do not use SVG
  markers; rasterizers render them inconsistently.
- Secondary connectors use a 1px Silver dashed line:
  `stroke-dasharray="3 3"`.
- Annotation boxes use a white fill and 1px Slate dashed stroke:
  `stroke-dasharray="4 3"`.
- Step badges are 18px square, Navy-filled, with centered white text.

## Layout

1. Write the flow as numbered steps first. Nouns become boxes and steps become
   arrows.
2. Put the primary flow left-to-right; place secondary flows below it.
3. Use generous spacing and route arrows through gaps, never through boxes.
4. Use right-angle paths for long connections rather than diagonals across
   rows or columns.
5. Put step badges on arrows, not inside boxes.
6. Keep arrow labels at least 10px from the nearest box and clear of other
   text.
7. Put uppercase Silver section headers above boxes with at least 15px of
   clearance.
8. Widen boxes for long labels instead of clipping or shrinking text.
9. Prefer 3–5 boxes and 2–4 arrows. Split a complex explanation into multiple
   diagrams.
10. Every element must carry information; do not add decorative elements.

## Building blocks

Entity:

```xml
<rect x="40" y="60" width="200" height="100" fill="#F5F5F5" stroke="#0A0A0A" stroke-width="1"/>
<text x="140" y="50" text-anchor="middle" class="header-text">CATEGORY</text>
<text x="140" y="100" text-anchor="middle" class="label">Entity name</text>
<text x="140" y="120" text-anchor="middle" class="sublabel">Short description</text>
<text x="140" y="140" text-anchor="middle" class="mono">Technical detail</text>
```

Arrow with step badge:

```xml
<line x1="240" y1="95" x2="555" y2="95" stroke="#0A1636" stroke-width="1.5"/>
<polygon points="555,90 565,95 555,100" fill="#0A1636"/>
<rect x="370" y="74" width="18" height="18" fill="#0A1636"/>
<text x="379" y="87" text-anchor="middle" class="step-num">1</text>
<text x="395" y="87" class="arrow-label">Step description</text>
```

Annotation:

```xml
<rect x="280" y="170" width="240" height="90" fill="#FFFFFF" stroke="#404040" stroke-width="1" stroke-dasharray="4 3"/>
<text x="400" y="193" text-anchor="middle" class="sublabel" font-weight="500">Annotation</text>
<text x="400" y="213" text-anchor="middle" class="mono">Technical detail</text>
```

## Review

After creating or changing an SVG:

1. validate that its XML parses;
2. rasterize it to an 800px-wide PNG;
3. visually inspect the PNG;
4. repeat until it has no overlaps, clipped labels, stray arrow crossings,
   crowded badges, or unbalanced whitespace.

Example:

```sh
xmllint --noout docs/<name>.svg
uvx --from cairosvg cairosvg docs/<name>.svg \
  -o /tmp/<name>.png --output-width 800
```

Do not ship an SVG that has not passed raster review.

## Template

```xml
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 800 {HEIGHT}" fill="none" role="img" aria-labelledby="title desc">
  <title id="title">Diagram title</title>
  <desc id="desc">Concise description of the diagram.</desc>
  <style>
    text { font-family: 'Geist', 'Inter', system-ui, sans-serif; }
    .label { font-size: 13px; fill: #0A0A0A; font-weight: 600; }
    .sublabel { font-size: 11px; fill: #404040; font-weight: 400; }
    .header-text { font-size: 11px; fill: #A0A0A0; font-weight: 400; letter-spacing: 0.08em; text-transform: uppercase; }
    .mono { font-family: 'Geist Mono', 'SF Mono', monospace; font-size: 10px; fill: #404040; }
    .step-num { font-size: 10px; fill: #FFFFFF; font-weight: 600; }
    .arrow-label { font-size: 10px; fill: #0A1636; font-weight: 500; }
  </style>
  <rect width="800" height="{HEIGHT}" fill="#FFFFFF"/>
</svg>
```
