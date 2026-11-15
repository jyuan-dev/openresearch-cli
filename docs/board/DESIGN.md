# ORX Board：研究看板技术方案（MVP）

- 状态：**草案，等待确认**
- 分支：`board-mvp`（基于 upstream/main 6c15b7e5）
- 产品依据：`~/.claude/plans/pasted-content-id-4c48-orx-open-polished-feather.md` 中「★ 最终使用流程」A–G 节和「★ 选型」节

本文只覆盖 MVP，也就是实验闭环。文档协作、专利线、时间线留到 v2。文中的 `file:line` 均基于 6c15b7e5。

---

## 1. 范围

**MVP 包含**
- Epic（回答标准带版本号）
- Ticket（`experiment` / `analysis` 两类）
- Decision（批准 / 结论 / 提问 / Blocked / 预算 / 公开闸门 / 导入）
- 跨项目收件箱、项目看板
- 全局调度器与对账循环、run 结束钩子
- 机器校验、预算与耗时统计
- 机器 × GPU 槽位登记
- 保密模式：只派发到自有机器，拦截 `share --publish`
- 历史导入（Slot-JEPA、INTACT-JEPA）
- ntfy 推送、systemd 安装、非回环地址绑定时的 token 鉴权

**MVP 不做**
- Milestone / Goal 的完整界面（表先建好，界面放到 v2）
- 文档协作、专利线、时间线
- 拖拽
- Slurm / 云端算力的调度
- 多人协作

**命名**：`plan`（Claude plan mode、#1026 的计费 plan）和 `plane`（`src/plane/` 为共享 CLI 操作）都已被占用，所以统一用 **board / ticket / epic / decision**。

## 2. 代码组织（降低与上游的冲突）

```
src/board/
  mod.rs          // 对外入口：init(state)、类型重导出
  model.rs        // Epic / Ticket / Decision / Machine 等结构体与状态枚举（纯数据）
  transitions.rs  // 状态机：合法转换、谁能触发（纯函数，单测覆盖）
  checks.rs       // 机器校验（纯函数）
  scheduler.rs    // 调度循环：对账 + 派发（可注入时钟，便于测试）
  dispatch.rs     // 开 Executor / Planner 会话、注入任务
  hooks.rs        // 订阅 engine bus，处理 run 终态
  budget.rs       // GPU 小时、墙钟时间、耗时偏差
  gate.rs         // 闸门：建节点检查、保密模式、公开拦截
  import.rs       // 历史项目导入的启发式规则
  notify.rs       // ntfy
  api.rs          // axum 路由（挂进 up.rs 的 router）
src/store/board.rs, board.sql  // Store 的 board 方法与建表（放在 store 子模块里，才能访问私有连接）
src/commands/board.rs   // `orx board …`、`orx ticket …`、`orx epic …`
ui/src/board/           // InboxPage、BoardPage、TicketDrawer、DecisionCard、MachinesSettings …
docs/board/DESIGN.md
```

现有文件里**只加注册点**：

| 文件 | 改动 |
|---|---|
| `src/store.rs` | `mod board;` 并在 `search::initialize` 之后调用 `board::initialize`；`pending_work()` 加入 board 待办（见 §6.4） |
| `src/commands/up.rs` | `router()`（:653）里 `.merge(board::api::routes())`；启动时调用 `board::init(state)`；`collect_events`（:10752）加入 board 游标 |
| `src/main.rs` | 加 `Board` / `Ticket` / `Epic` 三个子命令 |
| `src/local/chat/mod.rs` | `with_turn_context`（:1415）里追加 `<orx-ticket>` 块 |
| `src/plane/local_plane.rs` | `create_experiment`（:342）开头调用 `board::gate::check_create` |
| `share --publish` 入口、run 提交时的 backend 解析 | 调用 `board::gate` |
| `ui/src/workspaceState.ts`、`App.tsx`、`routes/` | 加 `inbox` 和 `board` 两个目的地 |

## 3. 数据模型

迁移规则沿用现有做法：只新增表或可空列，用 `CREATE TABLE IF NOT EXISTS`，用删除触发器代替外键。时间戳统一为 ms 整数（`now_ms()`），id 为 TEXT（uuid）。

```sql
-- 项目级 board 设置（不改 local_projects，单独一张表）
CREATE TABLE IF NOT EXISTS board_projects (
  project_id     TEXT PRIMARY KEY,
  enabled        INTEGER NOT NULL DEFAULT 0,   -- 开启后，建节点闸门生效
  priority       INTEGER NOT NULL DEFAULT 100, -- 跨项目手动排序，越小越先
  confidential   INTEGER NOT NULL DEFAULT 1,   -- 保密模式（专利申请前）
  deadline_ms    INTEGER,                      -- 用于到期提醒
  key_prefix     TEXT NOT NULL,                -- ticket 编号前缀，如 SJ
  next_seq       INTEGER NOT NULL DEFAULT 1,
  updated_at     INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS board_epics (
  id TEXT PRIMARY KEY, project_id TEXT NOT NULL, key TEXT NOT NULL,   -- SJ-E3
  title TEXT NOT NULL, question TEXT NOT NULL,                        -- 研究问题
  status TEXT NOT NULL,          -- proposed|active|answered|dropped
  priority INTEGER NOT NULL DEFAULT 100,
  criteria_version INTEGER NOT NULL DEFAULT 0,  -- 已批准的回答标准版本，0 = 尚未批准
  rules_json TEXT NOT NULL,      -- 授权规则（§7），默认全关
  budget_gpu_hours REAL, budget_deadline_ms INTEGER,
  conclusion TEXT, proposed_by TEXT NOT NULL,   -- user|agent
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  UNIQUE(project_id, key)
);

-- 回答标准按版本保存，批准时锁定其中一版
CREATE TABLE IF NOT EXISTS board_epic_criteria (
  epic_id TEXT NOT NULL, version INTEGER NOT NULL,
  body_md TEXT NOT NULL,          -- 给人读的文字
  checks_json TEXT NOT NULL,      -- 机器校验参数（§8），可覆盖全局默认值
  approved_at INTEGER, created_at INTEGER NOT NULL,
  PRIMARY KEY(epic_id, version)
);

CREATE TABLE IF NOT EXISTS board_tickets (
  id TEXT PRIMARY KEY, project_id TEXT NOT NULL, epic_id TEXT,        -- 允许为空：unplanned
  key TEXT NOT NULL,             -- SJ-042
  kind TEXT NOT NULL,            -- experiment|analysis
  batch_id TEXT,                 -- 同一轮的提议批次（Round）
  title TEXT NOT NULL, hypothesis TEXT, change_spec TEXT,
  status TEXT NOT NULL,          -- §4.1
  priority INTEGER NOT NULL DEFAULT 100,
  base_experiment_id TEXT,       -- 打算从哪个节点分支
  experiment_id TEXT,            -- 建出的节点（1:1，仅 experiment 类）
  needs_gpus INTEGER NOT NULL DEFAULT 0,
  est_minutes INTEGER,           -- 进 ready 前必填（experiment 类）
  validation_json TEXT,          -- 指标、频率、训练日志位置（进 ready 前必填）
  harness TEXT,                  -- 为空时用项目默认值
  session_id TEXT,               -- Executor 会话
  machine_id TEXT, slot_lease_at INTEGER,
  repair_count INTEGER NOT NULL DEFAULT 0,
  outcome TEXT,                  -- supports|refutes|inconclusive|null
  core_hypothesis INTEGER NOT NULL DEFAULT 0,  -- 否定它时，结论必须单独成卡
  result_json TEXT,              -- 指标、seed、相对基线的差值、校验结果
  conclusion TEXT,
  tainted INTEGER NOT NULL DEFAULT 0, tainted_reason TEXT,
  proposed_by TEXT NOT NULL,     -- user|agent|user-chat（聊天里人亲自下的指令）
  started_at INTEGER, finished_at INTEGER,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  UNIQUE(project_id, key)
);
CREATE INDEX IF NOT EXISTS board_tickets_status ON board_tickets(status, priority);
CREATE UNIQUE INDEX IF NOT EXISTS board_tickets_exp ON board_tickets(experiment_id) WHERE experiment_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS board_relations (
  from_id TEXT NOT NULL, to_id TEXT NOT NULL,
  kind TEXT NOT NULL,      -- blocked_by|references|reproduces|supersedes|relates_to|depends_on(epic)
  created_at INTEGER NOT NULL, PRIMARY KEY(from_id, to_id, kind)
);
-- branches_from 不入表，从 local_experiments.parent_experiment_id 推导

CREATE TABLE IF NOT EXISTS board_decisions (
  id TEXT PRIMARY KEY, project_id TEXT NOT NULL, epic_id TEXT,
  kind TEXT NOT NULL,      -- approve_batch|accept_batch|accept_one|question|blocked|budget|publish_gate|import|close_epic
  ticket_ids_json TEXT NOT NULL,   -- 涉及的 ticket，有序
  payload_json TEXT NOT NULL,      -- 卡片内容（提问的选项和推荐项、校验失败原因等）
  status TEXT NOT NULL,            -- open|resolved|superseded
  resolution_json TEXT,            -- 你的选择（逐项勾选结果、选项、文字）
  dedupe_key TEXT NOT NULL,        -- 幂等：同一事件只生成一张卡
  created_at INTEGER NOT NULL, resolved_at INTEGER,
  UNIQUE(dedupe_key)
);

-- 活动记录：审计与专利日期证据，只追加不修改
CREATE TABLE IF NOT EXISTS board_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL,
  project_id TEXT NOT NULL, subject_id TEXT NOT NULL,
  actor TEXT NOT NULL,       -- user|agent:<session>|system
  kind TEXT NOT NULL, detail_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS board_machines (
  id TEXT PRIMARY KEY, name TEXT NOT NULL,
  backend TEXT NOT NULL,     -- local | ssh:<alias>（对应 ORX 现有 compute 后端）
  gpu_slots INTEGER NOT NULL, own INTEGER NOT NULL DEFAULT 1,  -- own=1 才可在保密模式下使用
  enabled INTEGER NOT NULL DEFAULT 1, updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS board_settings (k TEXT PRIMARY KEY, v TEXT NOT NULL);
-- 全局默认值：校验参数、ntfy 地址、并发上限、耗时偏差阈值（1.5）等
```

`board_milestones`、`board_artifact_refs` 在 MVP 里先建表、不做界面，避免 v2 再做破坏性迁移。

## 4. 状态机（`transitions.rs`，纯函数）

### 4.1 Ticket

```
proposed ──approve(人)──▶ ready ──dispatch(调度器)──▶ running ──report(agent)──▶ review ──accept(人)──▶ done
   │                       │  ▲                          │ │  ▲                    │
   └─reject(人)─▶ rejected  │  └──answer(人)── waiting ◀─ask┘ │  └─challenge(人)─────┘
                           └─edit(人)                     └─block(agent/系统)─▶ blocked ─resolve(人)─▶ ready|rejected
done ──taint(人)──▶ done + tainted=1（状态不变，只加标记）
任何未结束状态 ──cancel(人)──▶ cancelled；running ──requeue(调度器)──▶ ready（启动失败时）
```

规则：
- **只有人能**批准、拒绝、接受、回答、解除 blocked、标记 tainted。判定方式：CLI 进程的环境里有 `ORX_CHAT_SESSION_ID` 就视为 agent，这类调用一律拒绝；HTTP 请求来自 dashboard 时视为人。
- 自动批准只能由 Epic 授权规则（§7）触发，记为 `actor=system(rule:<name>)`。
- `review → done` 时，如果对应的 Epic 规则 `auto_done` 已打开，并且校验全部通过，由系统完成；否则进入批量接受卡或单张结论卡（§8.2）。
- `running` 状态下修改假设 = `cancel` 当前 ticket 再新建一张；修改实现走评论。

### 4.2 Epic

`proposed → active`（回答标准 v1 被批准）`→ answered | dropped`。Epic 处于 active 时才能派发它的 ticket。回答标准出了新版本，需要再走一次批准卡；新版本批准前，已派发的 ticket 继续按旧版本校验。

### 4.3 Decision

`open → resolved`。另外，当它关联的 ticket 状态已经变化（比如被取消），状态变为 `superseded`。卡片的 `dedupe_key` 由“事件类型 + 对象 id + 版本”组成，保证同一事件只出一张卡（对应验收标准 G3）。

## 5. 调度器（`scheduler.rs`）

在 `orx up` 进程内用一个 tokio 任务运行，每 5 秒跑一轮；收到 engine bus 的 `RunEvent::Changed`，或 board 有写入时，立即再跑一轮。

```
loop tick:
  1. reconcile()   // 对账，见 §5.2
  2. dispatch()    // 派发，见 §5.1
  3. planner()     // 唤醒 Planner，见 §5.3
  4. budgets()     // 预算到 80% / 100% 时出卡
```

### 5.1 派发规则

候选 = 所有满足以下条件的 ticket：

- `status=ready`
- 所属项目 `board_projects.enabled=1`
- 所属 Epic 为 active
- 所有 `blocked_by` 依赖都已 done

候选按 `(项目优先级, Epic 优先级, ticket 优先级, created_at)` 排序，然后逐个检查：

1. **预算**：Epic 的剩余 GPU 小时 ≥ `est_minutes × needs_gpus`，并且没有超过截止日期。不满足就跳过，并确保存在一张预算卡。
2. **槽位**：`needs_gpus > 0` 时，找一台有空闲槽位的机器（保密项目只考虑 `own=1` 的机器）。空闲槽位 = `gpu_slots` − 该机器上 running ticket 的 `needs_gpus` 之和。`needs_gpus = 0` 的 ticket 不占槽位。
3. **并发**：同时运行的 Executor 会话不超过 `board_settings.max_sessions`（默认 8，防止本机被 agent 进程压垮）。
4. 通过后：在一个事务里写入 `status=running`、`machine_id`、`slot_lease_at`、`started_at`，然后调用 `dispatch::start_executor`。如果启动失败，回滚为 ready，并记一次 `repair_count`。

**提问只暂停自己那一张**：`waiting` 不在候选集里，所以其他 ticket 照常派发（对应 G2）。

### 5.2 对账（daemon 重启或崩溃后的兜底）

逐个检查每张 `running` 或 `waiting` 的 ticket：

- 会话不存在或已删除 → 重新开会话（带上 ticket 当前状态和最近的活动记录），超过次数上限就转 blocked。
- 节点已经有终态 run，但没有被处理过 → 补跑一次 `hooks::on_run_terminal`。这里依赖 §6 的幂等设计。
- 超过 `est_minutes × 阈值`（默认 1.5 倍）→ 标红，并在 ticket 上留一条系统评论。是否停止派发，由该 Epic 的规则决定。

启动时先跑一次完整对账，对应 G4。

### 5.3 Planner 按事件唤醒

触发事件：Epic 下有 ticket 进入 done、预算到 80%、用户在 Epic 上留言“想法”、Epic 刚变为 active。触发后开一个一次性的 Planner 会话（复用 §6.2 的开会话方式），任务是给这个 Epic 提出下一批 ticket，或者提议关闭 Epic。同一个 Epic 同时最多一个 Planner。Planner 通过 `orx ticket propose --batch` 提交，结果统一成一张 `approve_batch` 卡。

## 6. 与 ORX 的连接点

### 6.1 run 终态 → ticket（`hooks.rs`）

- 订阅 `engine::bus`，过滤出终态的 `RunEvent::Changed`（`Phase` 为 done / failed / cancelled / lost）。
- 收到 `Lagged` 时，从数据库回读所有 running ticket 的节点状态（bus.rs 已经约定了这种处理方式）。
- 处理逻辑：用 `experiment_id` 找到 ticket，再按 `board_events` 里有没有 `(run_id, kind=run_terminal)` 这条记录判断是否已处理过，保证**幂等**。
- 处理内容只是**记录 run 结束**。结论要由 Executor 读完日志后调用 `orx ticket report` 来写。如果 Executor 会话已经不在了，由对账负责重新拉起。
- Executor 能收到 run 结果，靠的是现有的 `subscribe_run` 通知机制（store.rs:5936）：派发时已经让它的会话订阅了这个节点的 run，所以这里不需要额外做什么。

### 6.2 开 Executor 会话（`dispatch.rs`）

照搬 `agent_tools::delegate` 的流程（`src/commands/up/agent_tools.rs:400-520`）：

1. 组装 `StoredChatSession`：项目、`ticket.harness` 或项目默认 harness、默认权限、自己的 worktree。
2. `store.create_chat_session` → `emit_session` → `state.chat.send_message(..., OrxDisplay{kind: Task})`。任务正文由 ticket 生成，包括假设、change_spec、base 节点、指定的机器与 backend、validation 配置，以及“完成后调用 `orx ticket report`”的约定。
3. 写入 `ticket.session_id`。

这需要在 `OrxKind` 里新增一种 `Ticket`，用于在聊天里显示成 ticket 卡片；也可以先复用 `Task`。

### 6.3 每轮注入上下文

`with_turn_context`（`chat/mod.rs:1415`）在发现会话绑定了 ticket 时，追加：

```xml
<orx-ticket key="SJ-042" status="running" epic="SJ-E3">
假设 / 变更范围 / 分支起点 / 指定机器 / validation / 回答标准 vN 摘要 / 未回答的提问 / 你的最新评论
可用命令：orx ticket report|ask|comment|block（你不能批准、不能新建 ticket，只能用 propose 建议）
</orx-ticket>
```

Planner 会话注入的是 `<orx-epic>` 块。

### 6.4 让 daemon 保持常驻

`Api::busy()`（`api_v2.rs:215`）目前依据的是 `pending_work()` 和 SSE 客户端数。这里在 `pending_work` 里**加一项**：存在 `ready / running / waiting` 的 ticket，或者存在没处理完的 Decision 卡时，都算 busy。这样 headless 模式就不会在 10 分钟空闲后退出（`api_v2.rs:1128`）。实际部署走 systemd（§11），这一项是额外的保险。

### 6.5 闸门（`gate.rs`）

| 闸门 | 位置 | 规则 |
|---|---|---|
| 建节点 | `LocalPlane::create_experiment` 开头 | 项目开启 board 时：① 调用者绑定的 ticket 处于 running 且 `experiment_id` 为空 → 放行，并回填 `experiment_id`。② 调用者是普通聊天，且当前这一轮由人发起（`TurnGuard` 那一轮的来源是 user 消息，不是通知、委派或 Planner）→ 自动建一张 `proposed_by=user-chat` 的 unplanned ticket，状态直接设为 running，然后放行（G7）。③ 其他情况一律拒绝，并提示“请先 `orx ticket propose`”（G8）。 |
| 派发机器 | run 提交时的 backend 解析（`src/local/mod.rs` 附近） | 如果会话绑定了 ticket，backend 必须等于该 ticket 分到的机器；保密项目只允许 `own=1` 的机器，其他（modal、hf、openresearch 等）一律拒绝（G5） |
| 公开 | `orx project share --publish` 入口 | 保密项目直接拒绝，并生成一张 `publish_gate` 卡；只有你在卡上点“允许一次”，系统才写入一次性许可。之后要重新执行命令，凭许可放行 |

“当前这一轮是否由人发起”是闸门 ② 的关键判断，实现时要核实 turn 的来源字段能否可靠区分。如果做不到，MVP 先退一步：普通聊天里一律需要先建 ticket，靠 ticket 抽屉里的“快速新建并直接运行”按钮弥补便利性。

## 7. Epic 授权规则（`rules_json`）

```json
{ "auto_refill_same_batch": false, "auto_seed_rerun": false, "undo_window_min": 15,
  "repair_cap": 3, "early_stop": null, "auto_done": false,
  "negative_in_batch": true, "overrun_factor": 1.5, "overrun_pauses_dispatch": false }
```

新 Epic、修改假设、追加预算三类操作**写死为必须由人批准**，不受这些规则影响。

## 8. 机器校验与结论卡

### 8.1 校验参数（`checks.rs`，纯函数）

全局默认值存在 `board_settings`，Epic 回答标准的 `checks_json` 可以逐项覆盖：

```json
{ "metric": "success_rate", "higher_is_better": true, "min_seeds": 3,
  "noise_floor": null, "min_delta": null, "baseline_ticket": null,
  "require_finite": true, "require_baseline_repro": true }
```

输入是 run 终态、`ticket.result_json`（由 Executor 报告：每个 seed 的指标、基线的值），以及同批次里基线 ticket 的结果。输出是 `{passed: bool, failures: [reason], direction: supports|refutes|inconclusive}`。

### 8.2 分卡规则

- 校验全部通过，并且不是“否定了核心假设或会导致放弃竞争路线”的负面结论 → 归入该 Epic 当前打开的 `accept_batch` 卡；负面结论在卡里单独分组（G6）。
- 其他情况 → 单独一张 `accept_one` 卡，高亮没通过的项目。

## 9. 预算（`budget.rs`）

- **GPU 小时** = Σ（run 结束时间 − 开始时间）× ticket 的 `needs_gpus`。这是按申报的 GPU 数计算的，界面上注明“估算”。
- **墙钟时间** = Epic 的截止日期减去当前时间。
- 每轮报告（同一个 `batch_id` 全部终态时生成）包含：预计耗时、实际耗时、累计消耗、剩余预算、下一批的预计耗时。实际超过预计 1.5 倍时标红。
- 预算到 100%：停止派发新 ticket，运行中的跑完，生成预算卡（G9）。

## 10. API 与实时更新

路由都挂在 `/api/board/*` 下（`board::api::routes()`），写法沿用 `up.rs` 的 `Json + ApiResult` 和 camelCase 结构体。

```
GET  /api/board/inbox                       跨项目：open decisions + running + since-last-view
GET  /api/board/projects/{id}               看板数据：epics、tickets（按状态、批次分组）
POST /api/board/projects/{id}/settings      enabled/priority/confidential/deadline
POST /api/board/epics  | PATCH /api/board/epics/{id}
POST /api/board/epics/{id}/criteria         新版本   POST …/criteria/{v}/approve
POST /api/board/tickets | PATCH /api/board/tickets/{id}
POST /api/board/tickets/{id}/comment        → 转发给 Executor 会话（send_message，忙时排队）
POST /api/board/tickets/{id}/taint
POST /api/board/decisions/{id}/resolve      { choice, selected:[ticketIds], text }
GET/POST /api/board/machines
GET/POST /api/board/settings
```

**SSE**：在 `collect_events`（`up.rs:10752`）的游标里加上 `board_tickets`、`board_decisions`、`board_epics` 的 `updated_at`，产出 `board.ticket.updated`、`board.decision.updated`、`board.epic.updated` 三种事件。前端在 `ui/src/events.ts` 里注册监听，在 `queries/invalidation.ts` 里使对应查询失效。轮询频率保持现有的 2 Hz。

## 11. 部署、远程访问与通知

- **`orx board install`**：写入 `~/.config/systemd/user/orx-board.service`，内容是 `orx up --headless --host <tailscale-ip> --port <p>`，并设置 `Restart=always`，然后执行 `loginctl enable-linger`。同时生成访问 token。
- **鉴权缺口（必须补）**：现在除了 `--remote` 模式，绑定非回环地址时都不需要登录（`main.rs` 的 `--host` 帮助里也写了这一点）。MVP 里，只要绑定非回环地址，就套上现有的 `require_remote_auth` 中间件（`up.rs:1092-1097`），复用 `RemoteAuth` 的 token 摘要校验。手机首次打开时输入 token，之后存在 cookie 里。
- **ntfy**：`board_settings.ntfy_url` 指向工作站上自建的 ntfy（只在 Tailscale 内可访问）。推送正文固定为“ORX：N 项待处理”，**不含任何研究内容**（G10）。由 Decision 的创建触发；30 秒内的多条合并成一条推送。
- **PWA**：补上 `manifest.webmanifest`，收件箱页做成手机优先的布局。ORX 已有手机布局的基础（#818）。

## 12. CLI

```
orx board                       # 跨项目收件箱（文本）
orx board view <projectId>      # 项目看板
orx board enable <projectId> [--prefix SJ] [--priority N] [--confidential on|off]
orx board machines [add <name> --backend local|ssh:<alias> --gpus N [--not-own] | remove <name>]
orx board decide <decisionId> [--select KEY]… [--option X] [--text T]
orx board install               # systemd + token + 自检
orx board import <projectId>    # 历史导入 → 生成一张 import 决策卡

orx epic add|propose <projectId> --title --question [--criteria-file]
orx epic criteria <epicId> --body … | --stdin [--checks-file f]   # 新版本（agent 可提，人批）
orx epic approve <epicId> [--version N] | rules <epicId> --file r.json | close --conclusion | drop

orx ticket propose <epicId> --title --hypothesis --change --base <expId> --gpus N --est 90m --validation-file v.json [--batch B]
orx ticket add …                              # 人用：直接 ready
orx ticket show <key> | approve | reject | accept | challenge   # 后四个只有人能用
orx ticket report <key> --result-file r.json --outcome refutes --conclusion "..."   # Executor
orx ticket ask <key> --question "..." --option A --option B --recommend A           # Executor
orx ticket block <key> --reason "..."
orx ticket comment <key> "..."
orx ticket taint <key> --reason "..."
```

bundled skill 需要新增 `orx-board` 模块，并修改 `orx-experiment-tree`：项目开启 board 后，auto-research loop 的第 2–3 步改为 `propose`，第 7 步的 promote 改为提议下一批。

## 13. 界面

| 页面 / 组件 | 内容 | 手机 |
|---|---|---|
| **InboxPage**（`/inbox`，开启 board 后作为默认首页） | 顶栏（GPU 池占用、预算、保密状态灯）、Decision 卡队列、运行中、自上次查看以来 | ✅ 主要适配对象 |
| **DecisionCard** | 按 kind 渲染：逐项勾选、选项按钮、文字回答、批量接受（负面结论单独分组） | ✅ |
| **BoardPage**（`/projects/$id/board`） | 列 = 状态，按 Epic 分泳道，按批次分组；用按钮改状态，不做拖拽 | 只读 |
| **TicketDrawer**（复用 `DetailDrawer`） | 假设、状态、关联、结果与校验、活动记录、评论框；“打开工作台”跳到它的会话和节点 | 部分 |
| **EpicPanel** | 回答标准（带版本）、授权规则开关、预算、结论 | 只读 |
| **设置 → 机器与 GPU** | 机器 × 槽位、是否自有、ntfy、全局校验默认值 | ❌ |

所有文案都走 paraglide i18n，以中文为主，并补齐其他语言的键。

## 14. 历史导入（`import.rs`）

- 按标题分组：`^(E\d+)([a-z](-[a-z])?)?\b` 归到 Epic `E\d+`；含 `Baseline Reproduction` 的归到“基线复现”Epic；其余归到“未分类”。
- 每个节点生成一张 ticket：状态为 done；`experiment_id` 指向原节点；`outcome` 为空，等你补；`proposed_by=user`。
- 结果汇总成**一张 import 决策卡**，你在卡上校对 Epic 归类后再提交。在你确认之前，导入不生效。
- 先导入 Slot-JEPA（`96c50231…`）和 INTACT-JEPA（`a4f05104…`）。

## 15. 测试计划

| 层 | 内容 | 位置 |
|---|---|---|
| 纯函数单测 | transitions 的合法和非法转换；checks 的各种组合；调度排序、槽位、预算、保密过滤（注入时钟）；导入的启发式规则 | `src/board/*.rs` 里的 `#[cfg(test)]` |
| Store | 迁移可以反复执行；dedupe_key 保证幂等；删除触发器 | `src/board/store.rs` 测试，沿用 `Store::open_at(temp)` |
| 集成 | 用 `src/engine/fake.rs` 驱动 run 终态 → hooks → ticket/decision；kill 之后重启 → 对账恢复；闸门拒绝 `create_experiment` | `tests/board_*.rs` |
| UI | DecisionCard 的各种卡、Inbox 渲染、事件触发的查询失效 | `ui/tests/board*.test.mjs` |
| E2E | 用 fake ACP agent 逐条走 G1–G9 | `e2e/tests/board.spec.ts` |
| 门禁 | `cargo test --locked`、`cargo clippy --all-targets -- -D warnings`；`ui/` 下 `pnpm test`、`pnpm typecheck`、`pnpm lint:i18n`、`pnpm lint:styles` | — |

## 16. 实施切片（每一片都可以单独合并、单独验证）

| # | 切片 | 交付 | 验收 |
|---|---|---|---|
| ① | 数据层 + CLI | 表、transitions、store 方法、`orx board/epic/ticket` 基本命令，人能用 CLI 走完整个状态机 | 单测；能在 demo 项目上用 CLI 创建并批准 |
| ② | 调度器 + 钩子 + 对账 + 预算 | 派发时开 Executor 会话、`<orx-ticket>` 注入、run 终态钩子、`ticket report/ask`、校验、分卡 | G1–G4、G9（集成测试 + fake engine） |
| ③ | 界面 | Inbox、DecisionCard、Board、TicketDrawer、机器设置 | UI 测试；手动走一遍 |
| ④ | 闸门 + 保密 | create_experiment 闸门、backend 限制、share 拦截 | G5、G7、G8 |
| ⑤ | 导入 | Slot-JEPA、INTACT-JEPA | 导入卡确认后，看板分组正确 |
| ⑥ | 部署 | `board install`、非回环 token 鉴权、ntfy、PWA | G10；重启机器后自动恢复 |

## 17. 风险与需要在实现中核实的点

1. **闸门 ②**（区分“人亲自下的指令”）要依赖 turn 的来源字段，见 §6.5 的退路。
2. **Executor 会话的可靠性**：harness 可能卡住、上下文被压缩，所以对账循环和会话重建是必须做的，不是可选项。
3. **槽位是人工登记的**，和机器上的真实占用可能对不上（比如你手动跑的任务）。MVP 接受这一点；v2 可以用 `nvidia-smi`（本机和 SSH）做校准。
4. **与上游合并**：`store.rs`（1.2 万行）、`up.rs`（1.35 万行）、`chat/mod.rs`（1.5 万行）是冲突热点，已经把改动压缩到注册点。合并节奏是每月一次。
5. **自动升级**：等 MVP 能日常使用时，再关掉已安装 orx 的 autoUpdate，换成 fork 编译的版本（你已同意）。


## 18. 切片 ② 实现记录（与上文不同或新增的地方）

- **调度器位置**：`src/commands/up/board.rs`，它是 `up` 的子模块，所以能复用 New Chat 的建会话逻辑。为此把 `create_chat_session` 里的构造过程抽成了 `new_chat_session`，Executor 和 New Chat 走同一条路径，用的是你的 New Chat agent、模型和权限模式；ticket 指定了 harness 时用指定的。
- **store 不能跨 await**：每一轮先在同步块里读写 store，再做异步的开会话和发消息。
- **提问不占 GPU**：处于 waiting 的 ticket 只有在它的 run 还在跑时才占用槽位（对应 G2）。被回答后如果槽位已被别的 ticket 占了，会暂时超订，MVP 接受这一点。
- **用户消息送达**：你在 CLI 或看板上的回答、质疑、评论，由调度器转发到该 ticket 的聊天（忙时排队），并记一条 `delivered` 事件，保证不重复发送。
- **agent 检测期间不派发**：`orx up` 刚启动、agent 还在检测时，开会话会失败。所以这段时间跳过派发，避免把一次无意义的失败算作 repair。
- **没有分支起点的 ticket**：简报要求 agent 用 `--baseline` 新建根节点，不再挂在项目当前的根节点上，因为那个根可能是另一张 ticket 的实验（e2e 中实际发生过）。
- **取消或拒绝 ticket**：调度器会停掉它的聊天，并清空排队的消息。它已经启动的 run **还不会**自动取消，留待切片 ④ 处理。
- **并发保护**：`move_ticket` 用“状态仍是读到的那个”作为更新条件，同一张 ticket 不会被派发两次。
- **全局设置**：`orx board set checks '<json>'`、`orx board set max-sessions <n>`。

## 19. 切片 ③ 实现记录

- **入口**：左侧栏 “Research board / 研究看板”，位于 Customize 下方，路由 `/projects/$projectId/board`。服务端（`workspace_state.rs`）和共享测试向量 `tests/contracts/locations.json` 都已接受这个位置，因此重开应用会回到看板。
- **页面**：一个页面三个分页（Segmented）。
  - **待你处理**：跨项目的 Decision 卡和运行中的 ticket，以及 GPU 槽位占用。
  - **本项目**：开关、研究问题、按状态分列的 ticket（自动换行的网格，不做横向滚动），点 ticket 展开活动记录、评论和取消。
  - **机器**：列出、添加、移除机器。
- **实时更新**：SSE `board.updated`，服务端用一个变化标记（`board_change_marker`）覆盖所有 board 表。CLI 或 agent 在另一个进程写入后，打开的页面约 0.2 秒内刷新（已实测）。`/api/board/*` 的写入按 URL 失效 board 查询族；断线重连时这些查询族也会被修复。
- **文案**：94 个 key，11 种语言都已翻译。计数一律写成“标签：数字”，不使用复数形式。
- **HTTP 即用户**：dashboard 调用 board API 时视为用户本人。和 CLI 依赖 `ORX_CHAT_SESSION_ID` 一样，这只能防止 agent 误操作，防不住刻意绕过的本机进程。

**已知缺口（后续处理）**
- 看板发给 ticket 聊天的消息显示为“from Another chat”，应显示为来自研究看板。
- 本机上所有 `renderApp` 整应用 UI 测试都因路由库报错失败（上游代码同样失败，与看板无关）。看板测试改用组件级 `render()`。
- 移动端只验证了 390px 宽度的布局，PWA manifest 和推送留在切片 ⑥。

## 20. 切片 ④ 实现记录（闸门与保密）

纯规则在 `src/board/gate.rs`，store 方法 `gate_experiment_create`、`gate_launch`、`gate_publish` 读取输入并执行结论。

- **建节点闸门**（G7、G8）：放在 `local::experiments::create_experiment` 里，CLI、dashboard 和 agent 都经过这一处。只对开启了 board 的项目生效。
  - 聊天绑定的 ticket 处于 running / waiting → 放行（Executor 的 repair 子节点也算在内）。
  - **这一轮是你亲自发起的**：判断依据是这一轮的用户消息里有没有 `orx` 来源标记。orx 通知、其他聊天、看板发来的消息都带这个标记，你输入的没有。满足时自动建一张 `user-chat` 的 unplanned ticket，状态直接是 running，挂在这个聊天上，然后放行。同一轮里再建节点归到同一张；你下一次提出要求时另建一张。
  - 其余情况（agent 在通知触发的轮次里自己决定）→ 拒绝，并提示 `orx ticket propose`。
  - 没有聊天会话（你在自己的终端或页面上操作）→ 放行，不建 ticket。
- **派发机器闸门**（G5）：放在 `Api::launch`，CLI 和 dashboard 的 run 都走这里。
  - ticket 分到了机器 → 只能在那台机器上跑。
  - 保密项目 → 只允许 `local` 或登记为自有的 SSH 机器；modal、hf、openresearch、未登记或非自有的 SSH 一律拒绝。
- **公开闸门**（G5）：`orx project share --publish` 和 dashboard 的“发布链接”都经过 `gate_publish`。
  - 保密项目会被拒绝，并打开一张 `publish_gate` 卡（同时只有一张）。
  - 你选“允许一次”后，下一次发布消耗这张许可；选“保持保密”则继续拦截。
- **取消即停算力**：ticket 被取消或拒绝时，调度器先取消它尚未结束的 run（它的实验，以及它的聊天发起的 run），再停聊天。取消的 run id 记在 `chat_stopped` 事件里。
- **unplanned ticket 的聊天被删除**：直接转为 blocked，不会重新排队，避免被别的 Executor 接手。
- **界面**：公开卡的主按钮是“保持保密”；“聊天中要求”标记 unplanned ticket。

**e2e（隔离数据目录，真实 agent）**
- 没有 ticket 的 agent 建节点被拒。
- `share --publish` 被拦下并生成卡片。
- 保密项目里 `ssh:rented` 和 `modal` 的 run 在提交前就被拒。
- 你在聊天里让 agent 建并运行实验 → 自动生成 TB-007（unplanned），节点挂上，run 在本机启动。
- `orx ticket cancel TB-007` → run 在 2 秒内进入 Cancelled，聊天停止。

**已知缺口**
- 判断依据是“这一轮由谁发起”。如果你在一轮里要求 X，agent 在同一轮里顺手又建了 Y，Y 也会被算进这张 ticket。
- unplanned ticket 没有 Epic，不进调度队列，也不参与 Epic 预算；结束后需要你在看板上接受或取消。
- 闸门依赖 `ORX_CHAT_SESSION_ID` 等环境变量，属于“防误操作”，防不住刻意绕过的本机进程。

## 21. 切片 ⑤ 实现记录（历史导入）

纯规则在 `src/board/import.rs`，store 方法是 `propose_import` 和 `apply_import`。

- **只起草，不写入**：`orx board import <projectId>`，或看板“本项目”页上的“起草导入”，只会打开或更新一张 `import` 卡。项目里有多少个还没有 ticket 的实验，页面上会直接提示。重复执行会覆盖这张卡上的草稿，不会再多出一张卡。
- **自动分组**，按以下顺序：
  1. 标题里含 “Baseline Reproduction” → “Baseline reproduction”；
  2. 前缀标签（最多 4 个字母 + 数字 + 可选的变体字母和 `-x`）→ 按标签分组，例如 `E1a`、`E1c` 归到 `E1`，`E2e-b` 归到 `E2`，`RP1`、`P6-fast` 同理。单个节点也单独成组。
  3. 至少 3 个节点共用同一个开头单词 → 成为一组，但跳过 does、is、what 这类非主题词；
  4. 其余归到 “Unsorted”。
- **分组文件**：`--groups <file>` 可以替代自动分组，可以由你或你让 agent 来写。格式是 `[{"title": "...", "experimentIds": ["<id 或 id 前缀>"]}]`，文件里没有列出的实验归到 Unsorted。`orx board decide <id> --option import --groups <file>` 也接受同样格式的文件。
- **在卡上校对**：可以重命名分组，可以把单个实验移到别的分组（下拉框），也可以把两个分组改成同名来合并。点“导入”之前不写入任何内容；选“放弃”也不写入。只有你能确认。
- **确认后**，在一个事务里写入：
  - 每组一个 `answered` Epic，question 先填组名，之后可以改；
  - 每个节点一张 ticket，挂到原节点 `experiment_id`，`base_experiment_id` 设为父节点，`conclusion` 用节点描述，`created_at` 用节点的创建时间；
  - 状态：有 run 以 done 结束 → done；已归档或从未成功跑完 → cancelled。
  - 已经被其他 ticket 占用的节点会让整次导入失败并提示重新起草，不会只导入一半。
- **看板页**：已回答或已放弃的研究问题下的 ticket 不在状态列里显示，只显示一行计数（“已回答研究问题中的 ticket：N”）。研究问题列表里标“已回答”。
- **在你真实数据的副本上验证**（真实库没有动）：
  - Slot-JEPA 起草为 14 组，与 E 编号一一对应，只有 1 个节点进了 Unsorted。确认后得到 35 张 done、4 张 cancelled。
  - INTACT-JEPA 的标题是自由文本，自动分组只能得到 44 组、115 个 Unsorted，**需要用分组文件**。

**已知缺口**
- 导入后的 Epic 没有回答标准，`orx epic show` 会提示“去提议”，对已回答的 Epic 来说是多余的。
- 导入的 Epic 没有“重新打开”，要在同一方向继续做，需要新建 Epic。

## 22. 切片 ⑥ 实现记录（部署、登录、推送、PWA）

- **其他设备要登录**（G10）：`orx up --host 0.0.0.0`（包括 Tailscale 访问）时，所有**非本机**的请求都要带访问 token；本机的浏览器、CLI 和 agent 照常不需要。
  - token 存在数据目录下的 `board-access-token`（0600 权限）。`orx board token` 打印登录链接，形如 `http://<tailscale-ip>:<port>/?access=…`，IP 由 `tailscale ip -4` 取得；`--rotate` 换新 token，所有已登录的设备随之失效。
  - 在设备上打开一次链接：服务器写入 HttpOnly、SameSite=Strict、有效期一年的 cookie，并 303 跳回去掉 token 的地址，所以 token 不会留在地址栏和历史记录里。也接受 `Authorization: Bearer`。
  - 没登录时：页面返回一张不含任何项目信息的登录说明页，`/api/*` 返回 401。安装用的 manifest 和图标不需要登录，它们不含研究内容。
  - 原有的“只服务私有网络”（RFC1918、链路本地、CGNAT/Tailscale）、Host 和 Origin 检查都保留。
- **常驻服务**：`orx board install [--port 4791] [--print]`。
  - 写入 `~/.config/systemd/user/orx-board.service`，`ExecStart` 为当前 orx 可执行文件，参数 `up --host 0.0.0.0 --port P --no-browser`。不用 headless 模式，因为 headless 空闲 10 分钟会退出。
  - 写入当前的 `PATH`，否则服务找不到 claude、codex 等 agent CLI；设置了 `ORX_DATA_DIR` 时一并写入。`Restart=always`。
  - 然后执行 `enable --now`，并尝试 `loginctl enable-linger`。端口被占用时给出提示。
  - `orx board uninstall` 停止服务并删除 unit。
- **ntfy 推送**：`orx board set ntfy '{"url":"http://<ip>:<port>/<topic>","token":"…","clickUrl":"…"}'`。
  - 正文固定为 “ORX: N waiting for you”，标题 “OpenResearch”，**不含任何研究内容**。
  - 有比上次推送更新的卡片时，立即推送；两次推送至少间隔 30 秒，期间新增的卡合并到下一条。推送失败也按 30 秒间隔重试。
  - 推送进度记在 `board_settings.ntfyPushed`，重启后不会重复推送。
- **PWA**：`manifest.webmanifest`（standalone，主题色 #9a2036）、192/512/maskable PNG 图标和 apple-touch-icon，`index.html` 里补上了 manifest、theme-color 和 iOS 的 meta。服务器以 `application/manifest+json` 返回 manifest。

**e2e**
- 通过 Tailscale 地址访问：未登录时页面和 API 都返回 401；打开登录链接后 303 跳转并写入 cookie；带 cookie 返回 200；cookie 错误返回 401；本机不带 token 返回 200。
- 用 Pixel 7 模拟的浏览器通过 Tailscale 登录，在收件箱里批准了 TB-017；manifest 能被解析。
- 假 ntfy 服务：新卡立即推送 “2 waiting”；3 张卡连续出现时，先推 “3”，约 30 秒后合并推 “5”；重启后不重复推送。
- 用 scratch 数据真实执行 `board install`，`kill -9` 主进程后 systemd 约 2 秒内重启，对账照常；再 `uninstall`，没有残留。

**注意：你现有的 `orx-tailscale-bridge.service`**
- 它在 Tailscale 地址的 4791 端口上把请求代理到 127.0.0.1，并自己附上认证 token，所以 tailnet 里的任何设备都能**不登录**直接使用 dashboard。
- 经过它的请求在 orx 看来来自本机，新的登录检查对它不起作用。
- 它还占用 4791 端口，会和 `orx board install` 默认的端口冲突。
- 换成 board 服务后，应停用这个 bridge（由你决定，我没有改动它）。
