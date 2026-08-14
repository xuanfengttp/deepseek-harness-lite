# DeepSeek Harness Lite — 设计方案 v2

> 状态:草案 v2,综合用户反馈修订,待确认后实施。
> 基于 deepseek-harness 架构移植到 Rust,面向网元设备嵌入式 agent。

---

## 第一部分:对用户问题的回答

### Q0:是否需要"everything is a plugin"+OTLP?+全量组件分析

#### 0.1 插件模型决策

dsh 的"everything is a plugin"依赖 Cordis 动态加载器 + patch 层 + scoped 注册,实现"一切可从配置替换"。这套机制的价值在于**多部署场景的运行时灵活性**,代价是加载器、patch 合并、scope 链、HMR 等重型基础设施。

**lite 版结论:不引入动态插件加载器,采用"编译期模块 + 配置期组合 + 运行期 skill"三级扩展。**

| 扩展层级 | 机制 | 对应 dsh 理念 |
|---|---|---|
| 编译期 | Rust 模块 + cargo feature 门控平台/工具集 | "一切可替换"的编译期等价 |
| 配置期 | 单 TOML 配置文件组合(启用的工具、模型端点、策略) | dsh 的 profile/bundle patch,但静态化 |
| 运行期 | 声明式 skill 文件(YAML+MD),用户加载 | dsh 的 skill 能力,保留 |

保留的精神:**核心不感知"网元",设备智能由 skill + 工具赋予**(对应 dsh capability seam 哲学)。砍掉的:Cordis 加载器、patch 合并、scope 链、HMR、动态注册/dispose 生命周期。

#### 0.2 OTLP 决策

**lite 版结论:不引入 OTLP。用结构化日志 + 可选轨迹(trajectory)替代。**

理由:
- OTLP 导出假设有 collector 网络基础设施,嵌入式环境通常没有
- OpenTelemetry SDK 本身有内存开销,与 10MB 目标冲突
- 用户已明确要"轨迹功能可设置开启,默认不开启"——这就是可观测性的轻量替代
- NMS 如需数据:通过 HTTP API 端点暴露,或 tail 日志文件

替代方案:
- **结构化日志**:log + 轻量格式化,写文件/stdout,分级
- **轨迹(trajectory)**:完整事件流(turn/step/chunk/tool),默认关闭(省内存),开启后放宽内存预算。对应 dsh session log 的精简版,仅内存不导出 OTLP

#### 0.3 dsh 全量 226 包分析(保留/改造/裁剪)

##### A. 保留并移植(核心,约 18 包 → Rust 模块)

| dsh 包 | 职责 | lite 处理 | 改造说明 |
|---|---|---|---|
| core/agent-loop | turn/step 驱动 | `agent` 模块 | 移植 turn/step 模型;**新增 tri-mode 分发**(workflow/todo/plan);phase 状态机简化;砍 waterfall 改钩子函数 |
| core/agent | Agent 接口+registry | 合并入 `agent` | 砍 initiator scope/AsyncLocalStorage;单进程单会话(活跃),多会话侧边栏+缓存卸载 |
| core/session | 事件 log+derive | `session` 模块 | append-only Rust enum;环形缓冲+flash 卸载;`derive_messages` 投影;砍 merge-extensible map |
| core/system-prompt | prompt 组装 | `prompt` 模块 | sections+tools+variables;**极简化**:一次一 skill,工具描述设上限 |
| core/tools | 工具注册+执行管线 | `tools` 模块 | 管线 5→3 阶段(check/execute/result);砍 waterfall around-middleware;保留 scoped 工具白名单(由 skill 控制) |
| core/scope | scoped 注册 | 简化 | 砍 scope 链;保留"活跃 skill 的工具集"概念 |
| llm/llm | message+stream+adapter | `llm` 模块 | HTTP 流式客户端(OpenAI 兼容);**支持 think 字段映射**;砍 adapter seam(单协议) |
| compaction/compaction-basic | 上下文压缩 | `compact` 模块 | **刚需**;滚动摘要;**独立上下文**(摘要不包含被摘要内容);触发阈值可配 |
| session/session-persistence | 持久化 | `persist` 模块 | flash 节流 checkpoint;紧凑二进制;与 compaction 协同 |
| skill/skill | skill 注册表 | `skill` 模块 | 声明式加载;YAML+MD;**砍 watcher/chokidar**;保留目录+扁平两种形态 |
| skill/skill-filesystem | 文件 skill provider | 合并入 `skill` | 简化为扫描配置目录;无文件监听 |
| shell/tool-bash + shell/bash-local | shell 执行 | `tools/shell` | 命令执行;平台分支编译期 cfg |
| shell/tool-pwsh + shell/pwsh-local | pwsh 执行 | `tools/shell` | 同上 |
| fs/tool-fs + fs/fs-local | 文件读写 | `tools/file` | 读写+目录列表 |
| fs/tool-fs-search | 文件搜索 | `tools/file` | glob 搜索(精简) |
| todo/tool-todo | 待办列表 | `todo` 模块 | 保留;**与 workflow 模式协同** |
| spill/spill-policy + spill-local | 结果截断 | 合并入 `tools` | 保留截断逻辑;砍 spill 文件落盘 |
| sandbox/sandbox-policy + sandbox-local | 权限策略 | `policy` 模块 | 极简 allow/deny;无沙箱执行器(设备本身已受限) |

##### B. 改造为新模式(约 5 包 → 新模块)

| dsh 包 | lite 处理 | 说明 |
|---|---|---|
| plan/plan-mode | `plan` 模块(tri-mode 之一) | 面向不确定性问题;agent 规划→执行→重规划 |
| goal/goal + goal-round-driver | 合并入 `plan` | goal 驱动合并进 plan 模式;简化 |
| workflow/workflow + workflow-worker-thread + tool-workflow | `workflow` 模块(tri-mode 之一) | **确定性 SOP 执行**,绕过 agent loop;skill 显式定义步骤序列;最小化 LLM 调用 |
| tool-ralph | 砍 | 多代理迭代,不需要 |

##### C. 新增(lite 独有,约 6 模块)

| 模块 | 说明 |
|---|---|
| `ssh` | 内置 SSH 客户端(russh),连接池+交互式命令接口,占位 |
| `memory` | 跨会话长期记忆,可配置存储(默认 flash KV+文本,有界),memory 工具读写 |
| `server` | 极简 HTTP 服务+静态网页(内嵌),SSE 流式 |
| `dispatcher` | **tri-mode 任务分发器**:入口分类→workflow/todo/plan |
| `cross` | 交叉编译配置与脚本 |
| `trajectory` | 可选轨迹记录(默认关闭) |

##### D. 裁剪(冗余,直接砍,约 180+ 包)

| 域 | 包 | 裁剪理由 |
|---|---|---|
| **前端** | client/* (40+ 包) | 替换为单页静态 HTML+原生 JS |
| **协议** | acp/、sdk/(protocol/server/client) | ACP 自动化+JSON-RPC,嵌入式不需要 |
| **类型图** | typert/(generator/loader/protocol/registry) | 类型图生成,重,不需要 |
| **API 网关** | api/(gateway/remotes) | BFF+RPC gateway,不需要 |
| **云沙箱** | e2b/(e2b/fs-e2b/subprocess-e2b) | 云沙箱,设备已受限 |
| **LSP** | lsp/(lsp/lsp-stdio/tool-lsp) | 语言服务器,网元不需要 |
| **终端** | terminal/(terminal/terminal-bash/tool-terminal) | 持久终端会话,不需要 |
| **Web 搜索** | web/(web/tool-web/web-fetch-http/web-search-*) | 无外网或受限;**网络诊断也砍**(用户确认设备不一定有) |
| **子代理** | subagent/* (10 包) | 全砍;如需子任务,workflow 模式或单进程串行 |
| **多代理编排** | workflow/(tool-ralph) | ralph 砍;workflow 保留但重定义为 SOP 模式 |
| **遥测** | session/session-telemetry-otel | OTLP 导出,不需要 |
| **HMR** | client/hmr + cordis-plugin-hmr | 编译二进制不需要热更新 |
| **全文搜索** | session-query/* (4 包) | SQLite 全文搜索,不需要 |
| **LLM 标题** | session/session-title-llm + 变体(3 包) | 小模型生成标题浪费;用首条消息截断 |
| **Code Mode** | code-runtime/* + tools/code-mode | 代码执行模式,小模型用不上,重 |
| **附件** | attachment/* (2 包) | 图片附件,设备场景不需要 |
| **身份** | identity/anonymous-user-id | 不需要 |
| **后台作业** | jobs/* (3 包) | 不需要 |
| **重复提醒** | guard/repeat-tool-reminder | 砍,小模型靠 prompt 引导 |
| **设置** | settings/settings-file(重) | 极简:env+单配置文件 |
| **凭证** | credentials/* (2 包) | 极简:env+配置文件内联 |
| **投影缓存** | session/session-projection-cache | 简化或去掉 |
| **MCP** | mcp/mcp-client | MCP 客户端,暂不需要 |
| **存储抽象** | storage/* (4 包) | 通用存储抽象,用直接文件 IO 替代 |
| **调度** | schedule/schedule | cron 调度,不需要 |
| **诊断** | runtime-diagnostics/invariants | 运行时不变式,编译期断言替代 |
| **测试支撑** | test-support/* (6 包) | 测试基础设施,不进二进制 |
| **示例** | examples/* (3 包) | demo bundles,不进二进制 |
| **扩展** | extensions/* (4 包) | cordis runner 桥接,不需要 |
| **host** | host/* (6 包) | 目录选择器/前端静态/webserver,用极简 server 替代 |
| **context** | context/* (4 包) | tmux/time/session-reference;agent-instructions 保留(精简) |
| **feedback** | feedback/* (2 包) | 反馈功能,不需要 |
| **preset** | preset/* (2 包) | agent preset 组合,用 skill 替代 |
| **hooks** | hooks/* (3 包) | Claude Code/Codex hook 桥接,不需要 |
| **boot** | boot/* (2 包) | app-boot/cmdline,Rust main 替代 |
| **util** | util/* (7 包) | 按需内联少量(brand→不需要,timeout→内联) |
| **vendor** | vendor/cordis | 整个 Cordis 框架,Rust 重写替代 |
| **python** | python/ | Python SDK,不需要 |
| **native** | native/ | landlock addon,Linux 沙箱,设备已受限不需要 |

##### E. 汇总

| 类别 | dsh 包数 | lite 处理 |
|---|---|---|
| 保留移植 | ~18 | → Rust 核心模块 |
| 改造新模式 | ~5 | → workflow/plan 模块 |
| 新增 | 0 → 6 | lite 独有模块 |
| 裁剪 | ~180+ | 直接砍 |
| **总计** | 226 | **~29 个 Rust 模块** |

---

### Q3:tokio 单线程运行时的影响

**结论:可接受,推荐采用。**

tokio `current_thread` 运行时(非 multi-thread)的影响分析:

| 维度 | multi-thread | current_thread(lite 用) | 对 lite 的影响 |
|---|---|---|---|
| 内存 | 每 worker 线程有栈(默认 2MB×N)+ work-stealing 队列 | 单线程,无 worker 池,无 stealing | **省 ~2-4MB**,关键 |
| I/O 并发 | 多线程 epoll | 单线程 epoll(同样异步非阻塞) | **无影响**:HTTP 流式、SSH、文件 IO 都是 I/O 密集,单线程 epoll 并发足够 |
| CPU 密集 | 可并行 | 会阻塞 reactor | **需注意**:shell 命令、文件读写必须 `spawn_blocking`(独立阻塞线程池,限 1-2 个),不阻塞 reactor |
| 吞吐 | 高 | 单用户场景足够 | **无影响**:网元 agent 是单用户交互,非高并发服务 |
| 编译 | tokio full features | tokio `rt` + `rt-multi-thread` 关闭 | **二进制更小** |

**关键约束**:所有阻塞操作(shell exec、文件大块读写)必须包在 `spawn_blocking` 里,阻塞线程池限制为 1-2 个线程。这是唯一需要注意的点,不影响异步 I/O 并发。

**结论**:current_thread + spawn_blocking(1-2) 是内存最优且功能完备的选择。省 2-4MB 内存,对单用户交互式 agent 零功能损失。

---

## 第二部分:修订后的完整设计

### 1. 目标与硬约束(v2)

| 维度 | 目标 |
|---|---|
| 形态 | 网元设备嵌入式 agent,编译为静态二进制直接运行 |
| 语言 | Rust 核心重写(移植 dsh 架构设计,非裁剪 TS 代码) |
| 模型 | 35B A3B(MoE,3B 激活),短上下文;**用户可配置**(本地推理服务或旁路服务器);agent 作 HTTP 客户端(OpenAI 兼容) |
| 交互 | 轻量网页客户端(单页静态 HTML+原生 JS,对话+MD 渲染)+ 内嵌极简 HTTP 服务 |
| 轨迹 | 可选轨迹记录,**默认关闭**(省内存),开启后放宽内存预算 |
| 会话 | 单活跃会话在内存;多会话侧边栏(元数据);**内存卸载+双页缓存**快速切换 |
| 扩展 | 声明式 skill 文件(YAML 头部+MD 正文,Claude 兼容格式) |
| 执行模式 | **tri-mode 分发**:workflow(确定性 SOP)/ todo(agent 引导确定性)/ plan(不确定性探索) |
| think | skill 显式 `think` 字段,控制 LLM 推理模式开关,按任务类型 |
| 内置工具 | shell 执行、文件读写、SSH 客户端(占位);**网络诊断砍掉** |
| 记忆 | 会话内 session log(短期)+ 跨会话长期记忆(可配置存储);持久化到 flash |
| compaction | **独立上下文**滚动摘要;与持久化协同 |
| 内存 | 运行时 RSS ≈ 10MB(轨迹关闭时);开启轨迹后放宽 |
| 平台 | aarch64 / armv7(hard-float + soft-float),musl 静态链接 |
| 分发 | 单二进制 + config/ + skills/ 目录,无运行时依赖 |

### 2. 核心架构:tri-mode 任务分发(新增重点)

这是 lite 相对 dsh 最重要的架构创新。dsh 只有 agent loop 一种执行路径;lite 按任务确定性程度分三种模式,skill 显式声明:

```
用户输入
  → dispatcher 分类(skill 声明 mode,或默认 agent 判定)
     ├─ workflow 模式:确定性 SOP,绕过 agent loop
     ├─ todo 模式:agent 引导,每步确定,skill 控制流程
     └─ plan 模式:不确定性,agent 规划→执行→重规划
```

#### 2.1 workflow 模式(确定性 SOP)

- **场景**:设备固定操作,如"巡检""重启接口""查配置",步骤确定
- **机制**:skill 显式定义步骤序列(类似脚本),每步是工具调用 + 可选 LLM 判断
- **绕过 agent loop**:不组装完整 prompt、不协商工具 schema、不重新派生消息。直接按步骤序列执行工具,仅在需要解析/判断时调用 LLM(最小化)
- **效率**:避免 agent loop 开销(prompt 组装、消息派生、多轮交互),单次 LLM 调用或纯工具执行
- **skill 控制**:skill 声明 `mode: workflow` + `steps:` 步骤定义

```yaml
# skill 示例:workflow 模式
---
name: interface-health-check
description: 接口健康巡检,确定性步骤
mode: workflow
think: false
tools:
  allow: [shell, ssh_exec]
steps:
  - id: show_interface
    tool: shell
    args: { command: "show interface brief" }
  - id: parse_status
    llm_judge: "从上一步输出判断哪些接口 down,返回 JSON {down_interfaces: [...]}"
    input: "{{steps.show_interface.result}}"
  - id: restart_down
    tool: shell
    args: { command: "restart interface {{steps.parse_status.down_interfaces}}" }
    when: "steps.parse_status.down_interfaces | length > 0"
---
# 接口健康巡检 SOP...
```

#### 2.2 todo 模式(agent 引导确定性)

- **场景**:步骤确定但每步内容需 agent 判断(如"诊断这个告警":查日志→分析→给结论)
- **机制**:agent loop 运行,但 skill 约束步骤序列;每步 agent 在约束内调用工具;LLM 引导每步执行
- **与 workflow 区别**:workflow 的步骤是纯工具脚本(极少 LLM);todo 的每步都经 LLM 引导(agent loop 跑,但路径确定)
- **skill 控制**:skill 声明 `mode: todo` + `steps:`(步骤是引导而非脚本)

#### 2.3 plan 模式(不确定性探索)

- **场景**:新问题、未知故障、需要探索推理
- **机制**:完整 agent loop;agent 自主规划(可用 todo 工具跟踪)→执行→根据结果重规划;对应 dsh plan-mode + goal
- **skill 控制**:skill 声明 `mode: plan`(或不声明 mode 时的默认)
- **think**:此类任务通常 `think: true`(开启推理)

#### 2.4 dispatcher 分类逻辑

- skill 显式声明 `mode` → 直接用
- skill 未声明 → 默认 `plan`(完整 agent loop,最通用)
- 运行时可被用户覆盖(`/mode workflow` 命令)

### 3. think 字段(LLM 推理模式控制)

- skill frontmatter 声明 `think: true/false`
- 映射到 LLM 请求参数(OpenAI 兼容的 `reasoning_effort` 或 provider 特定字段)
- `think: false`(workflow/todo 确定性任务)→ 关闭推理,快、省 token
- `think: true`(plan 诊断任务)→ 开启推理,质量优先
- 小模型推理弱,确定性任务不该浪费推理预算在已知步骤上

### 4. Skill 格式(Claude 兼容)

移植 dsh 的 SKILL.md 格式,新增 lite 专有字段:

```yaml
---
# 通用字段(dsh/Claude 兼容)
name: interface-diagnostics        # kebab-case,必需
description: 网元接口诊断技能       # 必需,路由描述
whenToUse: 当用户报告接口异常时     # 可选,额外路由指引
# disable-model-invocation: false  # 可选,是否对模型可见
# user-invocable: true             # 可选,是否对用户可见

# lite 专有字段
mode: plan                         # workflow | todo | plan(默认 plan)
think: true                        # 是否开启 LLM 推理(默认 false)
tools:                             # 工具白名单
  allow: [shell, file_read, file_write, ssh_exec, memory]
variables:                         # 变量({{var}} 插值)
  device_model: "from-config"
steps:                             # workflow/todo 模式的步骤定义
  - id: step1
    tool: shell
    args: { command: "..." }
---
# markdown 正文:技能指令
你是网元设备诊断助手。按以下流程:
1. 先查接口状态
2. 分析异常原因
3. 给出修复建议
```

- 两种文件形态(同 dsh):扁平 `name.md` 或目录 `name/SKILL.md`
- 扫描目录:配置指定的 skills/(无 watcher,启动时扫描+命令重扫)
- 模型侧渲染:移植 dsh 的 `<skill_content>` 包裹格式

### 5. 记忆系统(两层,可配置)

#### 5.1 短期:session log(会话内)

- append-only Rust enum 事件(TurnStart/End、StepStart/End、UserMessage、AssistantChunk、AssistantMessage、ToolCall、ToolResult、TodoWrite、RequestHeader)
- `derive_messages()` 投影 model history
- **环形缓冲**:内存中仅保留最近 N 事件(按上下文预算动态定),老事件落盘后释放
- 轨迹关闭:仅保留 derive 所需的 surface 事件(user/assistant/tool),丢弃 raw chunk
- 轨迹开启:保留全部事件(含 chunk、timing),放宽内存

#### 5.2 长期:跨会话记忆(可配置)

- **存储后端可配置**:默认 flash KV+文本(有界);接口抽象,可换实现
- agent 通过 `memory` 工具读写:`memory_read(key)` / `memory_write(key,value)` / `memory_recall(query)`
- 用途:设备拓扑、已知问题、用户偏好、反复模式
- 有界(默认 256 条,可配),LRU 淘汰
- 配置项:`memory.backend`(flash/sqlite/custom)、`memory.max_entries`、`memory.path`

#### 5.3 compaction(独立上下文,刚需)

- **触发**:派生消息 token 超阈值(context_window × 0.7)
- **独立上下文**:摘要请求使用**全新独立上下文**,仅包含待摘要的消息,不包含当前对话上下文。这保证短上下文下摘要质量
- **策略**:保留最近 K 轮原文,更早的用 LLM 总结为一条摘要消息
- 摘要调用同一模型端点(用户配置的),但用独立请求
- 摘要结果作为 surface replace(保持 log 可重建)
- 与持久化协同:compaction 后的老事件可释放内存(已落盘)

### 6. 会话内存卸载 + 双页缓存

```
侧边栏: [会话A*] [会话B] [会话C]    (* = 活跃)
         ↓活跃      ↓仅元数据   ↓仅元数据
内存:   [完整log]   -           -
flash:   checkpoint  checkpoint  checkpoint

切换 A→B:
  1. B 预加载(双页:后台已预热 B 到 buffer2)
  2. 瞬间交换:buffer2 → 活跃,B 立即可用
  3. 后台:A 写回 flash,释放 buffer1
  4. 预热 C 到空闲 buffer
```

- 活跃会话:完整事件 log 在内存
- 非活跃:侧边栏仅显示元数据(标题、时间、末条消息预览,从 flash 索引读)
- 双页缓存:活跃 buffer + 预热 buffer;切换时瞬间交换;内存临时波动可接受
- flash 存:每个会话的 checkpoint(紧凑二进制)+ 元数据索引

### 7. 网页客户端 + HTTP 服务

- HTTP 服务:hyper 裸用,端点:
  - `GET /` → 内嵌静态页(include_str!)
  - `POST /api/chat` → 发消息(带 mode 选择)
  - `GET /api/stream`(SSE)→ 流式 assistant chunk + 工具事件
  - `GET /api/sessions` → 会话列表(侧边栏)
  - `POST /api/session/switch` → 切换会话
  - `GET /api/skills` + `POST /api/skill/activate` → skill 列表/切换
  - `GET /api/settings` + `POST /api/settings` → 设置(含轨迹开关)
- 网页:单 `index.html` + `app.js`(原生 JS,无框架无构建)
  - 对话框 + **markdown 渲染**(轻量 MD 解析器,内嵌)
  - 侧边栏:会话列表
  - 设置面板:轨迹开关、skill 切换、mode 选择
  - SSE 流式渲染
- 资源编译期内嵌,无文件系统依赖

### 8. 内置工具

| 工具 | 说明 | 状态 |
|---|---|---|
| `shell` | 命令执行(平台分支:bash/pwsh) | 完整 |
| `file_read` | 读文件 | 完整 |
| `file_write` | 写文件 | 完整 |
| `file_search` | glob 搜索 | 完整 |
| `ssh_exec` | SSH 执行命令(连接池) | **占位**(连接+单命令,后续专项) |
| `memory_read` / `memory_write` / `memory_recall` | 长期记忆 | 完整 |
| `todo_write` | 待办列表 | 完整(与 workflow/todo 模式协同) |

**砍掉**:网络诊断(用户确认设备不一定有,自实现太重)、web 搜索、web fetch

### 9. Rust 工程结构

```
deepseek-harness-lite/
├── Cargo.toml
├── src/
│   ├── main.rs              # CLI 入口 + 配置加载 + 启动
│   ├── config.rs            # TOML 配置解析
│   ├── dispatcher.rs        # tri-mode 任务分发(workflow/todo/plan)
│   ├── agent.rs             # AgentLoop: turn/step 驱动(plan 模式)
│   ├── workflow.rs          # workflow 模式:确定性 SOP 执行
│   ├── session.rs           # 事件 log + derive + 环形缓冲 + 卸载
│   ├── prompt.rs            # 系统提示组装(sections + tools)
│   ├── tools/
│   │   ├── mod.rs           # ToolRegistry + 3 阶段管线
│   │   ├── shell.rs         # shell 执行
│   │   ├── file.rs          # 文件读写 + 搜索
│   │   ├── ssh.rs           # SSH 客户端(占位)
│   │   └── memory.rs        # 长期记忆工具
│   ├── llm.rs               # HTTP 流式客户端 + think 字段映射
│   ├── skill.rs             # 声明式 skill 加载(YAML+MD)
│   ├── compact.rs           # 上下文压缩(独立上下文滚动摘要)
│   ├── persist.rs           # flash 持久化(节流 checkpoint)
│   ├── memory.rs            # 跨会话长期记忆(可配置后端)
│   ├── todo.rs              # 待办列表
│   ├── policy.rs            # 极简权限(allow/deny)
│   ├── trajectory.rs        # 可选轨迹记录(默认关闭)
│   └── server.rs            # 极简 HTTP + 静态网页
├── web/                     # 静态网页(编译期内嵌)
│   ├── index.html
│   └── app.js
├── skills/                  # 示例 skill
│   ├── interface-diagnostics.yaml
│   └── health-check.md
├── config/
│   └── default.toml
└── cross/                   # 交叉编译
    └── README.md
```

### 10. 依赖选型(最小化,全 pure-Rust)

| 用途 | 选型 | 备注 |
|---|---|---|
| 异步运行时 | tokio(current_thread) | 省 2-4MB |
| HTTP server+client | hyper + hyper-util | 共享,不引入 reqwest/axum |
| 序列化 | serde + serde_json | |
| 配置 | toml | 轻量 |
| skill YAML | yaml-rust2 | 纯 Rust |
| SSH | russh | 纯 Rust,占位 |
| 持久化编码 | bincode | 紧凑二进制 |
| 日志 | log + env_logger | 最小 |
| 网页资源 | include_str! | 零依赖 |
| MD 渲染(网页) | 内嵌轻量 JS 解析器 | 无 Rust 依赖 |

**不引入**:reqwest、axum、sqlx、tracing 全家桶、任何 C 绑定。

### 11. 内存预算(10MB,轨迹关闭)

| 组件 | 预估 MB |
|---|---|
| tokio current_thread + 栈 | 0.8 |
| hyper server(1-2 连接) | 0.5 |
| LLM HTTP client(hyper) | 0.5 |
| 活跃 session log(环形缓冲) | 1.5 |
| Prompt + 工具 schema(单 skill) | 0.3 |
| Skill registry | 0.2 |
| 工具执行缓冲(截断) | 0.8 |
| Compaction 工作内存 | 1.0 |
| 长期记忆 cache | 0.5 |
| 双页缓存(预热 buffer) | 1.0 |
| serde/json/杂项 | 0.5 |
| 碎片/余量 | 1.4 |
| **合计** | **~9.5 MB** |

轨迹开启后:+~3-5MB(完整事件保留),放宽至 ~14MB。

### 12. 交叉编译

| 目标 | 用途 |
|---|---|
| `aarch64-unknown-linux-musl` | 64-bit ARM 静态 |
| `armv7-unknown-linux-musleabihf` | ARMv7 硬浮点 |
| `armv7-unknown-linux-musleabi` | ARMv7 软浮点 |
| `x86_64-unknown-linux-musl` | 开发/测试 |

- musl 静态链接 → 单二进制无 glibc 依赖
- 全 pure-Rust 依赖 → 交叉编译零额外工具链
- `cargo zigbuild` 或 `cross` 用于 CI
- strip + LTO 优化体积

### 13. 实施路线图

| 阶段 | 内容 | 产出 |
|---|---|---|
| P0 脚手架 | **原版代码移入 reference/ 子目录+gitignore**;Cargo 工程;配置;CLI;交叉编译空壳 | 三目标可编译空壳 |
| P1 核心闭环 | agent loop(plan 模式)+ session log + prompt + llm client + shell 工具 | 最小 turn/step 对话 |
| P2 tri-mode | dispatcher + workflow 模式 + todo 模式 + think 字段 | 三模式可切换 |
| P3 skill | 声明式 skill 加载(YAML+MD,Claude 兼容)+ 工具白名单 | skill 驱动任务 |
| P4 记忆与压缩 | compaction(独立上下文)+ flash 持久化 + 长期记忆(可配置) | 上下文不爆+重启恢复 |
| P5 会话管理 | 多会话侧边栏 + 内存卸载 + 双页缓存 | 快速切换 |
| P6 交互 | 极简 HTTP server + 静态网页(MD 渲染+SSE)+ 轨迹开关 | 浏览器可用 |
| P7 SSH 占位 | russh 集成 + ssh_exec 工具 | 连接+单命令(占位) |
| P8 体积优化 | 依赖审计 + LTO/strip + 内存 profiling + 三目标验证 | 命中 10MB |

---

## 第三部分:待最终确认

以上方案已综合你所有反馈。实施前请确认以下几点:

1. **tri-mode 分发**是否是你想要的形态?workflow(纯脚本极少 LLM)/ todo(agent 引导每步)/ plan(完整探索)
2. **workflow 步骤定义**用 YAML `steps:`(如上示例),还是你倾向其他形式(如嵌入脚本语言)?
3. **原版代码移入 `reference/` 子目录 + gitignore**,然后 Rust 新建——这个结构调整确认?
4. **实施从 P0 开始**(脚手架+交叉编译空壳),逐阶段交付,每阶段确认后再进下一步——这个节奏可以?
5. 还有遗漏的需求或需要调整的点吗?
