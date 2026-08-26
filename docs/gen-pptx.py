# -*- coding: utf-8 -*-
"""Generate KV Cache design PPTX — single slide, white background."""

from pptx import Presentation
from pptx.util import Inches, Pt, Emu
from pptx.dml.color import RGBColor
from pptx.enum.text import PP_ALIGN, MSO_ANCHOR

prs = Presentation()
prs.slide_width = Inches(13.33)
prs.slide_height = Inches(7.5)

slide = prs.slides.add_slide(prs.slide_layouts[6])  # blank

# Colors
BLUE = RGBColor(0x25, 0x63, 0xEB)
GREEN = RGBColor(0x2E, 0x7D, 0x32)
ORANGE = RGBColor(0xE6, 0x51, 0x00)
RED = RGBColor(0xC6, 0x28, 0x28)
GRAY = RGBColor(0x88, 0x88, 0x88)
DARK = RGBColor(0x11, 0x11, 0x11)
LIGHT_BLUE = RGBColor(0xEF, 0xF6, 0xFF)
WHITE = RGBColor(0xFF, 0xFF, 0xFF)
LIGHT_GRAY = RGBColor(0xF0, 0xF0, 0xF0)

def add_textbox(slide, left, top, width, height, lines, font_size=11):
    """lines: list of (text, bold, color, size_override)"""
    txBox = slide.shapes.add_textbox(left, top, width, height)
    tf = txBox.text_frame
    tf.word_wrap = True
    tf.margin_left = Emu(0)
    tf.margin_top = Emu(0)
    tf.margin_right = Emu(0)
    tf.margin_bottom = Emu(0)
    for i, item in enumerate(lines):
        text, bold, color = item[0], item[1], item[2]
        sz = item[3] if len(item) > 3 else font_size
        p = tf.paragraphs[0] if i == 0 else tf.add_paragraph()
        p.space_after = Pt(2)
        p.space_before = Pt(0)
        run = p.add_run()
        run.text = text
        run.font.size = Pt(sz)
        run.font.bold = bold
        run.font.color.rgb = color
        run.font.name = "Microsoft YaHei"
    return txBox

def add_rounded_rect(slide, left, top, width, height, fill_color, line_color=None):
    from pptx.enum.shapes import MSO_SHAPE
    shape = slide.shapes.add_shape(MSO_SHAPE.ROUNDED_RECTANGLE, left, top, width, height)
    shape.fill.solid()
    shape.fill.fore_color.rgb = fill_color
    if line_color:
        shape.line.color.rgb = line_color
        shape.line.width = Pt(0.75)
    else:
        shape.line.fill.background()
    shape.shadow.inherit = False
    return shape

def add_card(slide, left, top, width, height, title, title_color=BLUE):
    """Add a card with title bar, return content area top position."""
    add_rounded_rect(slide, left, top, width, height, WHITE, RGBColor(0xE0, 0xE0, 0xE0))
    # Title
    add_textbox(slide, left + Inches(0.12), top + Inches(0.06), width - Inches(0.24), Inches(0.28),
                [(title, True, title_color, 12)])
    return top + Inches(0.34)  # content start

def add_diag_box(slide, left, top, width, lines, height=None):
    """Add a monospace diagram box."""
    if height is None:
        height = Inches(0.18 * len(lines) + 0.1)
    add_rounded_rect(slide, left, top, width, height, RGBColor(0xF8, 0xF9, 0xFA), RGBColor(0xEE, 0xEE, 0xEE))
    txBox = slide.shapes.add_textbox(left + Inches(0.1), top + Inches(0.05), width - Inches(0.2), height - Inches(0.1))
    tf = txBox.text_frame
    tf.word_wrap = False
    tf.margin_left = Emu(0)
    tf.margin_top = Emu(0)
    for i, (text, color) in enumerate(lines):
        p = tf.paragraphs[0] if i == 0 else tf.add_paragraph()
        p.space_after = Pt(0)
        run = p.add_run()
        run.text = text
        run.font.size = Pt(9)
        run.font.name = "Consolas"
        run.font.color.rgb = color
    return height

def add_table(slide, left, top, width, rows, col_widths=None):
    """rows: list of list of (text, color, bold). First row = header."""
    n_rows = len(rows)
    n_cols = len(rows[0])
    from pptx.util import Inches as In
    height = In(0.22 * n_rows + 0.05)
    tbl_shape = slide.shapes.add_table(n_rows, n_cols, left, top, width, height)
    tbl = tbl_shape.table
    if col_widths:
        for i, w in enumerate(col_widths):
            tbl.columns[i].width = w
    for r, row_data in enumerate(rows):
        for c, (text, color, bold) in enumerate(row_data):
            cell = tbl.cell(r, c)
            cell.margin_left = Inches(0.05)
            cell.margin_top = Emu(0)
            cell.margin_bottom = Emu(0)
            tf = cell.text_frame
            p = tf.paragraphs[0]
            p.space_after = Pt(0)
            run = p.add_run()
            run.text = text
            run.font.size = Pt(9)
            run.font.bold = bold
            run.font.color.rgb = color
            run.font.name = "Microsoft YaHei"
            if r == 0:
                cell.fill.solid()
                cell.fill.fore_color.rgb = RGBColor(0xF0, 0xF4, 0xF8)
            else:
                cell.fill.solid()
                cell.fill.fore_color.rgb = WHITE
    return height

# ── Title ──
add_textbox(slide, Inches(0.4), Inches(0.2), Inches(12.5), Inches(0.4),
            [("KV Cache 复用设计 — dsh-lite 上下文编排", True, DARK, 20)])
# Underline
from pptx.enum.shapes import MSO_SHAPE
line = slide.shapes.add_connector(1, Inches(0.4), Inches(0.62), Inches(12.9), Inches(0.62))
line.line.color.rgb = BLUE
line.line.width = Pt(2)

# ── Flow chain ──
add_textbox(slide, Inches(0.4), Inches(0.68), Inches(12.5), Inches(0.3),
            [("① 前缀匹配  →  ② 首选 Append  →  ③ 压缩重排  →  ④ 固定插入代价对比  →  ⑤ Block 对齐缓解  →  ⑥ 命中率验证", True, BLUE, 10)])

# ── Column layout ──
col_w = Inches(6.1)
col_l = Inches(6.55)
col_r = Inches(0.4)
card_gap = Inches(0.12)

# ═══ LEFT COLUMN ═══

# Card ①: Prefix matching
c1_top = Inches(1.05)
c1_h = Inches(1.1)
ct = add_card(slide, col_r, c1_top, col_w, c1_h, "① 前缀匹配：追加只 miss 新增，插入打断后缀")
add_diag_box(slide, col_r + Inches(0.12), ct, col_w - Inches(0.24), [
    ("[A][B][C][D]  ← 命中（复用 KV）   [E] ← miss（重算）", DARK),
])
add_textbox(slide, col_r + Inches(0.12), ct + Inches(0.28), col_w - Inches(0.24), Inches(0.3),
            [("API 逐 token 比对前缀，一致则复用 KV，否则重算。", False, RGBColor(0x44,0x44,0x44), 10)])

# Card ②: Append strategy
c2_top = c1_top + c1_h + card_gap
c2_h = Inches(2.15)
ct = add_card(slide, col_r, c2_top, col_w, c2_h, "② 首选策略：所有动态内容追加尾部")
add_textbox(slide, col_r + Inches(0.12), ct, col_w - Inches(0.24), Inches(0.2),
            [("Memory / skill / todo 动态注入如何不打断缓存？", False, GRAY, 9)])
add_diag_box(slide, col_r + Inches(0.12), ct + Inches(0.22), col_w - Inches(0.24), [
    ("[sys][tools][user1][asst1]  ← 前缀全命中   [memory] ← 只 miss 新增", DARK),
])
add_table(slide, col_r + Inches(0.12), ct + Inches(0.55), col_w - Inches(0.24), [
    [("动态内容", BLUE, True), ("注入方式", BLUE, True), ("缓存", BLUE, True)],
    [("Memory recall", DARK, False), ("tool result 追加", DARK, False), ("✅", GREEN, True)],
    [("Todo guidance", DARK, False), ("user msg 追加", DARK, False), ("✅", GREEN, True)],
    [("Workflow 指令", DARK, False), ("user msg 追加", DARK, False), ("✅", GREEN, True)],
], [Inches(2.2), Inches(2.6), Inches(0.8)])
# Key box
key_top = ct + Inches(1.45)
add_rounded_rect(slide, col_r + Inches(0.12), key_top, col_w - Inches(0.24), Inches(0.4), LIGHT_BLUE, BLUE)
add_textbox(slide, col_r + Inches(0.2), key_top + Inches(0.04), col_w - Inches(0.4), Inches(0.35),
            [("原则：一律追加尾部，不插入中间，不修改已有。压缩是唯一重排时机。", True, RGBColor(0x1E,0x40,0xAF), 9)])

# Card ③: Compaction
c3_top = c2_top + c2_h + card_gap
c3_h = Inches(1.15)
ct = add_card(slide, col_r, c3_top, col_w, c3_h, "③ 压缩：唯一重排窗口")
add_diag_box(slide, col_r + Inches(0.12), ct, col_w - Inches(0.24), [
    ("正常: [sys][tools][user1][asst1][user2]  miss 最后一轮", DARK),
    ("压缩: [sys][tools][Summary][recent]  前缀命中 →恢复append", DARK),
], Inches(0.5))
add_textbox(slide, col_r + Inches(0.12), ct + Inches(0.55), col_w - Inches(0.24), Inches(0.35),
            [("Summary 放头部，之后追加不破坏其缓存。不在压缩前重排：被移动消息马上要压缩掉，重排代价无回收窗口。", False, RGBColor(0x44,0x44,0x44), 9)])

# Card ④: Hit rate
c4_top = c3_top + c3_h + card_gap
c4_h = Inches(1.45)
ct = add_card(slide, col_r, c4_top, col_w, c4_h, "④ 实际命中率")
add_table(slide, col_r + Inches(0.12), ct, col_w - Inches(0.24), [
    [("阶段", BLUE, True), ("命中率", BLUE, True), ("原因", BLUE, True)],
    [("首轮", DARK, False), ("60-80%", DARK, False), ("system+tools 命中（预热），对话 miss", DARK, False)],
    [("第二轮起", DARK, False), ("85-99%", GREEN, True), ("只 miss 最后一轮", DARK, False)],
    [("压缩后", DARK, False), ("70-85%", DARK, False), ("前缀仍命中，Summary miss", DARK, False)],
    [("skill 切换", DARK, False), ("50-70%", DARK, False), ("identity 命中，persona 后缀 miss", DARK, False)],
], [Inches(1.3), Inches(1.1), Inches(3.2)])
add_textbox(slide, col_r + Inches(0.12), ct + Inches(1.15), col_w - Inches(0.24), Inches(0.2),
            [("数据来自 API 返回 prompt_cache_hit_tokens，非 agent 估算", False, GRAY, 8)])

# ═══ RIGHT COLUMN ═══

# Card ⑤: Fixed insert vs Append
c5_top = Inches(1.05)
c5_h = Inches(3.1)
ct = add_card(slide, col_l, c5_top, col_w, c5_h, "⑤ 固定位置插入 vs Append：代价对比")
add_textbox(slide, col_l + Inches(0.12), ct, col_w - Inches(0.24), Inches(0.2),
            [("设备状态注入、会话恢复 context 必须插在固定位置，损失多大？", False, GRAY, 9)])

# VS cards
vs_w = Inches(2.85)
vs_gap = Inches(0.15)
# Append card (green)
vs1_l = col_l + Inches(0.12)
vs_t = ct + Inches(0.25)
add_rounded_rect(slide, vs1_l, vs_t, vs_w, Inches(0.8), RGBColor(0xF6,0xFF,0xF6), RGBColor(0xC8,0xE6,0xC9))
add_textbox(slide, vs1_l + Inches(0.08), vs_t + Inches(0.04), vs_w - Inches(0.16), Inches(0.2),
            [("Append 尾部追加", True, GREEN, 9)])
add_diag_box(slide, vs1_l + Inches(0.08), vs_t + Inches(0.24), vs_w - Inches(0.16), [
    ("[sys][tools][user1][asst1][memory]", DARK),
], Inches(0.25))
add_textbox(slide, vs1_l + Inches(0.08), vs_t + Inches(0.52), vs_w - Inches(0.16), Inches(0.25),
            [("前缀全命中，只 miss 新增。命中率持续爬升。", False, RGBColor(0x55,0x55,0x55), 8)])

# Fixed insert card (red)
vs2_l = vs1_l + vs_w + vs_gap
add_rounded_rect(slide, vs2_l, vs_t, vs_w, Inches(0.8), RGBColor(0xFF,0xF8,0xF8), RGBColor(0xFF,0xCD,0xD2))
add_textbox(slide, vs2_l + Inches(0.08), vs_t + Inches(0.04), vs_w - Inches(0.16), Inches(0.2),
            [("固定位置插入", True, RED, 9)])
add_diag_box(slide, vs2_l + Inches(0.08), vs_t + Inches(0.24), vs_w - Inches(0.16), [
    ("[sys][memory][user1][asst1]", DARK),
], Inches(0.25))
add_textbox(slide, vs2_l + Inches(0.08), vs_t + Inches(0.52), vs_w - Inches(0.16), Inches(0.25),
            [("插入点后全部 miss。已有对话缓存全失效。", False, RGBColor(0x55,0x55,0x55), 8)])

# Scenario table
tbl_top = vs_t + Inches(0.9)
add_table(slide, col_l + Inches(0.12), tbl_top, col_w - Inches(0.24), [
    [("场景", BLUE, True), ("能否 append", BLUE, True), ("说明", BLUE, True)],
    [("Memory recall", DARK, False), ("✅ 可追加", GREEN, False), ("工具调用结果天然在尾部", DARK, False)],
    [("Skill 切换", DARK, False), ("⚠️ 改 system", ORANGE, False), ("persona 变化，后缀失效（低频）", DARK, False)],
    [("设备状态注入", DARK, False), ("❌ 需固定位置", RED, False), ("必须在 system 后、消息前", DARK, False)],
    [("会话恢复 context", DARK, False), ("❌ 需固定位置", RED, False), ("历史 context 需在对话前", DARK, False)],
], [Inches(1.7), Inches(1.5), Inches(2.5)])

# Card ⑥: Block alignment
c6_top = c5_top + c5_h + card_gap
c6_h = Inches(3.0)
ct = add_card(slide, col_l, c6_top, col_w, c6_h, "⑥ Block 对齐：缓解固定插入的损失")
add_textbox(slide, col_l + Inches(0.12), ct, col_w - Inches(0.24), Inches(0.2),
            [("Block 是 prefix matching 最小粒度，对齐 section 边界能否减少连带 miss？", False, GRAY, 9)])
add_diag_box(slide, col_l + Inches(0.12), ct + Inches(0.22), col_w - Inches(0.24), [
    ("未对齐: blk0  blk1  blk2  blk3", DARK),
    ("        [identity+rules|tools|persona|custom]", DARK),
    ("                      ↑ section 跨 block 边界", RED),
    ("", DARK),
    ("对齐后: blk0  blk1  blk2  blk3  blk4", DARK),
    ("        [identity+rules][tools][persona][custom]", DARK),
    ("                       ↑ 边界 = block 边界", GREEN),
], Inches(1.3))
add_textbox(slide, col_l + Inches(0.12), ct + Inches(1.55), col_w - Inches(0.24), Inches(0.3),
            [("对齐后：persona 变化只 miss blk3+blk4，blk0~blk2 独立复用。未对齐：persona 跨 blk2 尾部，连带 miss。", False, RGBColor(0x44,0x44,0x44), 9)])
add_textbox(slide, col_l + Inches(0.12), ct + Inches(1.85), col_w - Inches(0.24), Inches(0.5),
            [("▪ 自部署 → 同款分词器 + block_size 已知 → 技术可行", False, RGBColor(0x44,0x44,0x44), 9),
             ("▪ 边界浪费 ≤ 15 token，占 system prompt ~5%", False, RGBColor(0x44,0x44,0x44), 9),
             ("▪ append-only 运行中无浪费，仅 section 变化时触发", False, RGBColor(0x44,0x44,0x44), 9)])
key2_top = ct + Inches(2.4)
add_rounded_rect(slide, col_l + Inches(0.12), key2_top, col_w - Inches(0.24), Inches(0.4), LIGHT_BLUE, BLUE)
add_textbox(slide, col_l + Inches(0.2), key2_top + Inches(0.04), col_w - Inches(0.4), Inches(0.35),
            [("储备方案：三层设计已达 85-99%，5% 边际收益暂不够。被迫固定插入增多或命中率瓶颈时再启用。", True, RGBColor(0x1E,0x40,0xAF), 9)])

# Footer
add_textbox(slide, Inches(10), Inches(7.15), Inches(3), Inches(0.25),
            [("dsh-lite · docs/kv-cache-design.md", False, RGBColor(0xAA,0xAA,0xAA), 8)],
            )

prs.save(r"D:\project\rust\deepseek-harness lite\docs\kv-cache-design.pptx")
print("PPTX saved")
