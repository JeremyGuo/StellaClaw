import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import matplotlib.patches as mpatches
from matplotlib.patches import FancyBboxPatch, FancyArrowPatch
import matplotlib.patheffects as pe
import numpy as np

# ─────────────────────────────────────────────
# Shared style helpers
# ─────────────────────────────────────────────

def make_box(ax, x, y, w, h, facecolor, edgecolor='none',
             alpha=0.95, radius=0.02, zorder=3, lw=0, shadow=False):
    """Draw a rounded-rect card.  Optional subtle drop-shadow."""
    if shadow:
        s = FancyBboxPatch((x + 0.04, y - 0.04), w, h,
                           boxstyle=f"round,pad=0,rounding_size={radius}",
                           facecolor='black', edgecolor='none',
                           linewidth=0, alpha=0.25, zorder=zorder - 1)
        ax.add_patch(s)
    box = FancyBboxPatch((x, y), w, h,
                         boxstyle=f"round,pad=0,rounding_size={radius}",
                         facecolor=facecolor, edgecolor=edgecolor,
                         linewidth=lw, alpha=alpha, zorder=zorder)
    ax.add_patch(box)
    return box

def txt(ax, x, y, text, size=10, color='white', ha='center', va='center',
        bold=False, zorder=6, style='normal', family='sans-serif'):
    weight = 'bold' if bold else 'normal'
    ax.text(x, y, text, ha=ha, va=va, fontsize=size,
            color=color, fontweight=weight, fontstyle=style,
            fontfamily=family, zorder=zorder)

def draw_arrow(ax, x1, y1, x2, y2, color='white', lw=1.8, rad=0.0,
               style='-|>', mutation=16, zorder=4):
    arr = FancyArrowPatch((x1, y1), (x2, y2),
                          arrowstyle=style, color=color, linewidth=lw,
                          connectionstyle=f'arc3,rad={rad}',
                          mutation_scale=mutation, zorder=zorder)
    ax.add_patch(arr)


# ═════════════════════════════════════════════
# DIAGRAM 1 — architecture.png
# ═════════════════════════════════════════════

BG       = '#0f0f23'
CARD_BG  = '#1a1a35'
INDIGO   = '#818cf8'
TEAL     = '#2dd4bf'
PURPLE   = '#a78bfa'
AMBER    = '#fbbf24'
WHITE    = '#f1f5f9'
SUBTEXT  = '#94a3b8'
MUTED    = '#64748b'

fig1, ax = plt.subplots(figsize=(14, 9.5))
fig1.patch.set_facecolor(BG)
ax.set_facecolor(BG)
ax.set_xlim(0, 14)
ax.set_ylim(0, 10)
ax.axis('off')

# ── Title ──
txt(ax, 7, 9.55, 'ClawParty 2.0 — System Architecture',
    size=18, color=WHITE, bold=True)
txt(ax, 7, 9.2, 'Layered overview: channels → session → agents → scheduling',
    size=9, color=MUTED)

# ── LAYER 1: Channels  (y 8.2 – 9.0) ──
make_box(ax, 0.4, 8.15, 13.2, 0.85, CARD_BG, edgecolor='#2a2a4a', lw=1, radius=0.03, zorder=2)
txt(ax, 1.1, 8.85, 'CHANNELS', size=7.5, color=INDIGO, bold=True, ha='left')

chan_data = [
    ('Telegram Bot', INDIGO),
    ('CLI',          '#6366f1'),
    ('Future…',      '#4f46e5'),
]
cw = 3.5; gap = 0.5; sx = 1.05
for i, (cl, cc) in enumerate(chan_data):
    cx = sx + i * (cw + gap)
    make_box(ax, cx, 8.25, cw, 0.55, cc, radius=0.02, shadow=True)
    txt(ax, cx + cw / 2, 8.525, cl, size=10.5, bold=True, color=WHITE)

# ── LAYER 2: Session  (y 6.8 – 7.85) ──
make_box(ax, 0.4, 6.75, 13.2, 1.2, CARD_BG, edgecolor='#2a2a4a', lw=1, radius=0.03, zorder=2)
txt(ax, 1.1, 7.7, 'SESSION', size=7.5, color=TEAL, bold=True, ha='left')

make_box(ax, 3.0, 6.9, 8.0, 0.78, TEAL, alpha=0.88, radius=0.02, shadow=True)
txt(ax, 7.0, 7.29, 'Persistent State + Attachments', size=11, bold=True, color='#042f2e')

# ── LAYER 3: Agent Topology  (y 2.8 – 6.45) ──
make_box(ax, 0.4, 2.75, 13.2, 3.7, CARD_BG, edgecolor='#2a2a4a', lw=1, radius=0.03, zorder=2)
txt(ax, 1.1, 6.2, 'AGENT TOPOLOGY', size=7.5, color=PURPLE, bold=True, ha='left')

agent_specs = [
    (0.7,  3.0,  3.85, 'Main Foreground\nAgent', '(user-facing)',        PURPLE),
    (5.08, 3.0,  3.85, 'Main Background\nAgent', '(long-running tasks)', '#8b5cf6'),
    (9.45, 3.0,  3.85, 'Sub-Agent',              '(short-lived tasks)',  '#7c3aed'),
]
for bx, by, bw, name, desc, col in agent_specs:
    make_box(ax, bx, by, bw, 1.65, col, alpha=0.90, radius=0.02, shadow=True)
    txt(ax, bx + bw / 2, by + 1.05, name, size=10, bold=True, color=WHITE)
    txt(ax, bx + bw / 2, by + 0.35, desc, size=8, color='#e2d6ff')

# Workspace card (spans across all three agents conceptually – placed center)
ws_x = 2.2; ws_w = 9.6
make_box(ax, ws_x, 4.95, ws_w, 0.9, '#312e81', alpha=0.85, radius=0.02, shadow=True)
txt(ax, ws_x + ws_w / 2, 5.4, 'Workspace  (durable, per-agent, isolated filesystem)',
    size=10, bold=True, color='#c7d2fe')

# Arrows: agents → workspace
for bx, _, bw, *_ in agent_specs:
    draw_arrow(ax, bx + bw / 2, 4.65, bx + bw / 2, 4.95,
               color='#a5b4fc', lw=1.6, mutation=14)

# Dashed peer arrows between agents
for x1, x2 in [(4.55, 5.08), (8.93, 9.45)]:
    draw_arrow(ax, x1, 3.82, x2, 3.82, color='#6366f1', lw=1.4,
               style='<->', mutation=12)

# ── LAYER 4: Cron / Sink  (y 0.4 – 2.45) ──
make_box(ax, 0.4, 0.35, 13.2, 2.15, CARD_BG, edgecolor='#2a2a4a', lw=1, radius=0.03, zorder=2)
txt(ax, 1.1, 2.25, 'CRON / SINK', size=7.5, color=AMBER, bold=True, ha='left')

cron_data = [
    ('Scheduled Tasks',  AMBER,   '#451a03'),
    ('Direct Routing',   '#f59e0b','#451a03'),
    ('Broadcast Topics', '#d97706','#451a03'),
]
cw2 = 3.5; gap2 = 0.5; sx2 = 1.05
for i, (cl, cc, tc) in enumerate(cron_data):
    cx = sx2 + i * (cw2 + gap2)
    make_box(ax, cx, 0.55, cw2, 0.95, cc, radius=0.02, shadow=True)
    txt(ax, cx + cw2 / 2, 1.025, cl, size=10.5, bold=True, color=tc)

# ── Inter-layer arrows ──
draw_arrow(ax, 7.0, 8.15, 7.0, 7.95, color=TEAL,   lw=2.5, mutation=18)
draw_arrow(ax, 7.0, 6.75, 7.0, 6.45, color=PURPLE,  lw=2.5, mutation=18)
draw_arrow(ax, 7.0, 2.75, 7.0, 2.50, color=AMBER,   lw=2.5, mutation=18)

# ── Legend ──
legend_items = [(INDIGO, 'Channels'), (TEAL, 'Session'),
                (PURPLE, 'Agents'),   (AMBER, 'Cron / Sink')]
for i, (lc, lt) in enumerate(legend_items):
    lx = 1.5 + i * 3.0
    make_box(ax, lx, 0.05, 0.3, 0.18, lc, radius=0.008)
    txt(ax, lx + 0.42, 0.14, lt, size=8, color=SUBTEXT, ha='left')

fig1.tight_layout(pad=0.3)
fig1.savefig('/Users/jeremyguo/Projects/ClawParty2.0/docs/imgs/architecture.png',
             dpi=180, bbox_inches='tight', facecolor=BG)
plt.close(fig1)
print("architecture.png saved")


# ═════════════════════════════════════════════
# DIAGRAM 2 — workspace_lifecycle.png
# ═════════════════════════════════════════════

BG2      = '#0f172a'
CARD2    = '#1e293b'
GREEN    = '#22c55e'
GREEN_DK = '#064e3b'
BLUE     = '#3b82f6'
BLUE_DK  = '#1e3a5f'
CYAN2    = '#06b6d4'
GRAY2    = '#94a3b8'
LGRAY2   = '#cbd5e1'
WHITE2   = '#f8fafc'

fig2, ax2 = plt.subplots(figsize=(16, 10))
fig2.patch.set_facecolor(BG2)
ax2.set_facecolor(BG2)
ax2.set_xlim(0, 16)
ax2.set_ylim(0, 10)
ax2.axis('off')

# ── Title ──
txt(ax2, 8, 9.65, 'Workspace Sandbox — Lifecycle & Cross-Agent Reuse',
    size=17, color=WHITE2, bold=True)
txt(ax2, 8, 9.3, 'How workspaces are created, used, archived, and reused across agents',
    size=9, color=GRAY2)

# ── LEFT PANEL: Lifecycle flow ──
make_box(ax2, 0.3, 0.3, 8.7, 8.7, CARD2, edgecolor='#334155', lw=1, radius=0.03, zorder=2, alpha=0.6)
txt(ax2, 4.65, 8.75, 'WORKSPACE LIFECYCLE FLOW', size=8.5, color=GRAY2, bold=True)

steps = [
    (8.0, GREEN,  '#052e16', 'New Session'),
    (6.8, GREEN,  '#052e16', 'Initialize workspace\nfrom rundir template'),
    (5.6, GREEN,  '#052e16', 'Main Agent works in\nworkspaces/<id>/files/'),
    (4.4, '#16a34a','#052e16','Session Closes'),
    (3.2, CYAN2,  '#083344', 'Summary Generated\n(summary.md written)'),
    (2.0, BLUE,   WHITE2,    'Workspace enters\nHistory Pool'),
]

SBW = 4.8; SBH = 0.85
step_cx = 4.65  # center x

for sy, bg_col, fg_col, stxt in steps:
    make_box(ax2, step_cx - SBW / 2, sy, SBW, SBH, bg_col,
             radius=0.022, shadow=True, alpha=0.92)
    txt(ax2, step_cx, sy + SBH / 2, stxt, size=10, bold=True, color=fg_col)

# Arrows between steps
for i in range(len(steps) - 1):
    y_from = steps[i][0]
    y_to   = steps[i + 1][0] + SBH
    draw_arrow(ax2, step_cx, y_from, step_cx, y_to + 0.03,
               color='#475569', lw=2.2, mutation=16)

# Step numbers
for i, (sy, *_) in enumerate(steps):
    circle_x = step_cx - SBW / 2 - 0.45
    circle_y = sy + SBH / 2
    circle = plt.Circle((circle_x, circle_y), 0.22, facecolor='#334155',
                         edgecolor='#475569', lw=1.2, zorder=5)
    ax2.add_patch(circle)
    txt(ax2, circle_x, circle_y, str(i + 1), size=9, bold=True, color=WHITE2)

# ── RIGHT PANEL: Tools ──
make_box(ax2, 9.3, 4.7, 6.4, 4.35, CARD2, edgecolor='#334155', lw=1, radius=0.03, zorder=2, alpha=0.6)
txt(ax2, 12.5, 8.8, 'CROSS-WORKSPACE REUSE TOOLS', size=8.5, color=GRAY2, bold=True)

tools = [
    (8.05, 'workspaces_list',         'List all history workspaces'),
    (7.05, 'workspace_content_list',  'Browse workspace contents'),
    (6.05, 'workspace_mount',         'Read-only snapshot mount'),
    (5.05, 'workspace_content_move',  'Move content across workspaces'),
]
TBW = 5.8; TBH = 0.72; TBX = 9.55
for ty, tname, tdesc in tools:
    make_box(ax2, TBX, ty, TBW, TBH, BLUE, alpha=0.85, radius=0.02, shadow=True)
    txt(ax2, TBX + TBW / 2, ty + TBH * 0.65, tname,
        size=10, bold=True, color=WHITE2, family='monospace')
    txt(ax2, TBX + TBW / 2, ty + TBH * 0.28, tdesc,
        size=8, color='#bfdbfe')

# ── RIGHT PANEL: Filesystem ──
make_box(ax2, 9.3, 0.3, 6.4, 4.15, CARD2, edgecolor='#334155', lw=1, radius=0.03, zorder=2, alpha=0.6)
txt(ax2, 12.5, 4.2, 'FILESYSTEM LAYOUT', size=8.5, color=GRAY2, bold=True)

fs_entries = [
    ('workdir/sandbox/workspaces.json',        GRAY2,  CARD2),
    ('workdir/sandbox/workspace_meta/',         GRAY2,  CARD2),
    ('    <id>/summary.md',                     LGRAY2, '#0f172a'),
    ('workdir/workspaces/',                     GRAY2,  CARD2),
    ('    <id>/files/          ← agent workdir',GREEN,  '#0f172a'),
    ('    <id>/mounts/         ← snapshots',    CYAN2,  '#0f172a'),
]
fy_start = 3.75; fy_step = 0.55
for i, (fe_text, fe_color, fe_bg) in enumerate(fs_entries):
    fy = fy_start - i * fy_step
    make_box(ax2, 9.55, fy - 0.18, 5.9, 0.42, fe_bg,
             radius=0.012, lw=0.8, edgecolor='#334155', alpha=0.8)
    txt(ax2, 9.75, fy + 0.03, fe_text,
        size=8.5, color=fe_color, ha='left', family='monospace')

# ── Connecting arrow ──
draw_arrow(ax2, step_cx + SBW / 2 + 0.1, 2.42, 9.3, 6.5,
           color='#64748b', lw=2.8, rad=-0.2, mutation=18)
txt(ax2, 8.5, 4.8, 'reuse via\ntools', size=9, color='#94a3b8', bold=True, style='italic')

# ── Bottom note ──
txt(ax2, 8, 0.1,
    'Each workspace is isolated.  Reuse is explicit and tool-mediated.',
    size=8.5, color='#475569', style='italic')

fig2.tight_layout(pad=0.3)
fig2.savefig('/Users/jeremyguo/Projects/ClawParty2.0/docs/imgs/workspace_lifecycle.png',
             dpi=180, bbox_inches='tight', facecolor=BG2)
plt.close(fig2)
print("workspace_lifecycle.png saved")
